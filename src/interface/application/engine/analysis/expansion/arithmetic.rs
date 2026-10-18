use std::collections::BTreeSet;
use tree_sitter::Node;

use super::super::execution::Environment;
use super::super::execution::children;
use super::super::{Frame, Resolver, Task};
use super::Expansion;
use crate::interface::application::engine::word::{self, Word};
use crate::interface::application::engine::{
  Access, DerivedOrigin, ExecutionKind, OriginPart, ResolutionIssue, Value,
  ValueIssue,
};

type Number = Result<i64, ValueIssue>;

fn integer(value: &Value) -> Number {
  let Value::Literal(bytes) = value else {
    let Some(issue) = value.issue() else {
      unreachable!()
    };
    return Err(issue);
  };
  let text = std::str::from_utf8(bytes)
    .map_err(|_| ValueIssue::Arithmetic)?
    .trim();
  if text.is_empty() {
    return Ok(0);
  }
  let (negative, text) = if let Some(text) = text.strip_prefix('-') {
    (true, text)
  } else {
    (false, text.strip_prefix('+').unwrap_or(text))
  };
  let (radix, digits) = if let Some(text) =
    text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))
  {
    (16, text)
  } else if text.len() > 1 && text.starts_with('0') {
    (8, &text[1..])
  } else {
    (10, text)
  };
  let n =
    i128::from_str_radix(digits, radix).map_err(|_| ValueIssue::Arithmetic)?;
  let n = if negative { -n } else { n };
  i64::try_from(n).map_err(|_| ValueIssue::Arithmetic)
}

fn value(number: Number) -> Value {
  let number = number.and_then(|n| {
    std::os::raw::c_long::try_from(n)
      .map(|_| n)
      .map_err(|_| ValueIssue::Arithmetic)
  });
  number.map_or_else(Value::Unknown, |n| {
    Value::Literal(n.to_string().into_bytes())
  })
}

fn result<'a>(number: Number, env: Environment) -> Task<'a> {
  Task::expanded(Expansion {
    word: Word::value(value(number), false),
    env,
  })
}

fn binary(operator: &[u8], left: Number, right: Number) -> Number {
  let (a, b) = (left?, right?);
  match operator {
    b"+" => a.checked_add(b),
    b"-" => a.checked_sub(b),
    b"*" => a.checked_mul(b),
    b"/" => a.checked_div(b),
    b"%" => a.checked_rem(b),
    b"<<" if a >= 0 => u32::try_from(b)
      .ok()
      .filter(|b| *b < 64)
      .and_then(|b| i64::try_from(i128::from(a) << b).ok()),
    b">>" if a >= 0 => {
      u32::try_from(b).ok().filter(|b| *b < 64).map(|b| a >> b)
    }
    b"&" => Some(a & b),
    b"|" => Some(a | b),
    b"^" => Some(a ^ b),
    b"==" => Some(i64::from(a == b)),
    b"!=" => Some(i64::from(a != b)),
    b"<" => Some(i64::from(a < b)),
    b"<=" => Some(i64::from(a <= b)),
    b">" => Some(i64::from(a > b)),
    b">=" => Some(i64::from(a >= b)),
    b"&&" => Some(i64::from(a != 0 && b != 0)),
    b"||" => Some(i64::from(a != 0 || b != 0)),
    _ => None,
  }
  .ok_or(ValueIssue::Arithmetic)
}

impl<'a> Resolver<'a> {
  pub(in crate::interface::application::engine::analysis) fn arithmetic(
    &mut self,
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    if node.kind() == "arithmetic_expansion" {
      let source = self
        .workspace
        .document(frame.snapshot.document)
        .expect("snapshot")
        .source
        .as_bytes();
      if let Some(expression) = node.child_by_field_name("expression") {
        let bytes = word::lexical(expression, source);
        if bytes.iter().all(u8::is_ascii_digit) {
          return result(integer(&Value::Literal(bytes)), env);
        }
      }
      if dynamic_arithmetic(node) {
        return self.expanded_arithmetic(node, env, frame);
      }
    }
    let source = self
      .workspace
      .document(frame.snapshot.document)
      .expect("snapshot")
      .source
      .as_bytes();
    match node.kind() {
      "arithmetic_expansion" | "parenthesized_arithmetic" => {
        node.child_by_field_name("expression").map_or_else(
          || result(Err(ValueIssue::Recovery), env.clone()),
          |expression| Task::arithmetic(expression, env.clone(), frame),
        )
      }
      "arithmetic_number" | "arithmetic_source_fragment" => {
        result(integer(&Value::Literal(word::lexical(node, source))), env)
      }
      "arithmetic_variable" => {
        let id = self.site(node, frame);
        if let Some(id) = id {
          self.read(id, &env, frame);
        }
        let number = id
          .and_then(|id| self.variable_value(id, &env))
          .map_or(Err(ValueIssue::Dynamic), |v| integer(v));
        result(number, env)
      }
      "arithmetic_unary_expression" => {
        let operator = node
          .child_by_field_name("operator")
          .map(|n| word::lexical(n, source))
          .unwrap_or_default();
        let Some(operand) = node.child_by_field_name("operand") else {
          return result(Err(ValueIssue::Recovery), env);
        };
        Task::arithmetic(operand, env, frame).then_word(move |_, expansion| {
          let number = integer(&expansion.word.scalar()).and_then(|n| {
            match operator.as_slice() {
              b"+" => Some(n),
              b"-" => n.checked_neg(),
              b"!" => Some(i64::from(n == 0)),
              b"~" => Some(!n),
              _ => None,
            }
            .ok_or(ValueIssue::Arithmetic)
          });
          result(number, expansion.env)
        })
      }
      "arithmetic_binary_expression" => self.calculate_binary(node, env, frame),
      "arithmetic_assignment_expression" => {
        self.calculate_assignment(node, env, frame)
      }
      "arithmetic_conditional_expression" => {
        Self::calculate_conditional(node, env, frame)
      }
      _ => {
        let before = env.clone();
        self
          .runtime_arithmetic(node, env, frame)
          .then(move |_, mut flow| {
            result(
              Err(ValueIssue::UnsupportedExpansion),
              flow
                .0
                .remove(&super::super::execution::Transfer::Next)
                .unwrap_or(before),
            )
          })
      }
    }
  }

  fn calculate_binary(
    &mut self,
    node: Node<'a>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let (Some(left), Some(right), Some(operator)) = (
      node.child_by_field_name("left"),
      node.child_by_field_name("right"),
      node.child_by_field_name("operator"),
    ) else {
      return result(Err(ValueIssue::Recovery), env);
    };
    let source = self
      .workspace
      .document(frame.snapshot.document)
      .expect("snapshot")
      .source
      .as_bytes();
    let op = word::lexical(operator, source);
    let short = matches!(op.as_slice(), b"&&" | b"||");
    let conflict = !short && self.arithmetic_conflict(left, right, frame);
    let previous_order = env.order.clone();
    if conflict {
      env.order.extend(
        self
          .arithmetic_names(node, frame)
          .1
          .into_iter()
          .map(crate::interface::application::engine::EffectTarget::Variable),
      );
    }
    Task::arithmetic(left, env, frame).then_word(move |_, left| {
      let a = integer(&left.word.scalar());
      let skip =
        a.is_ok_and(|a| (op == b"&&" && a == 0) || (op == b"||" && a != 0));
      if skip {
        return result(Ok(i64::from(op == b"||")), left.env);
      }
      let before = left.env.clone();
      Task::arithmetic(right, left.env, frame).then_word(
        move |resolver, mut right| {
          let mut number = binary(&op, a, integer(&right.word.scalar()));
          if short && a.is_err() {
            right.env.join(&before);
          }
          if conflict {
            for name in resolver.arithmetic_names(node, frame).1 {
              right.env.alternatives.forget(&name);
              if let Some(binding) = right.env.variables.values.get_mut(&name) {
                binding.taint(ResolutionIssue::EvaluationOrder);
              }
            }
            number = Err(ValueIssue::Unspecified);
          }
          right.env.order = previous_order;
          result(number, right.env)
        },
      )
    })
  }

  fn calculate_assignment(
    &mut self,
    node: Node<'a>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let (Some(mut left), Some(right), Some(operator)) = (
      node.child_by_field_name("left"),
      node.child_by_field_name("right"),
      node.child_by_field_name("operator"),
    ) else {
      return result(Err(ValueIssue::Recovery), env);
    };
    while left.kind() == "parenthesized_arithmetic" {
      let Some(inner) = left.child_by_field_name("expression") else {
        return result(Err(ValueIssue::Recovery), env);
      };
      left = inner;
    }
    let source = self
      .workspace
      .document(frame.snapshot.document)
      .expect("snapshot")
      .source
      .as_bytes();
    let op = word::lexical(operator, source);
    let id = self.site(left, frame);
    let conflict = op != b"=" && id.and_then(|id| self.workspace.occurrence(id)).is_some_and(|site| {
      matches!(&site.name, Value::Literal(name) if self.arithmetic_names(right, frame).1.contains(name))
    });
    let previous_order = env.order.clone();
    if conflict {
      env.order.extend(
        self
          .arithmetic_names(node, frame)
          .1
          .into_iter()
          .map(crate::interface::application::engine::EffectTarget::Variable),
      );
    }
    if conflict {
      self.gap(node, frame, ResolutionIssue::EvaluationOrder);
    }
    let old = if op == b"=" {
      Ok(0)
    } else {
      if let Some(id) = id {
        self.read(id, &env, frame);
      }
      id.and_then(|id| self.variable_value(id, &env))
        .map_or(Err(ValueIssue::Dynamic), |v| integer(v))
    };
    Task::arithmetic(right, env, frame).then_word(move |resolver, mut right| {
      let rhs = integer(&right.word.scalar());
      let readonly = resolver.site(left, frame).and_then(|id| resolver.workspace.occurrence(id)).is_some_and(|site| matches!(&site.name, Value::Literal(name) if right.env.readonly(name).is_some()));
      let number = if readonly { Err(ValueIssue::Dynamic) } else if conflict {
        Err(ValueIssue::Unspecified)
      } else if op == b"=" {
        rhs
      } else {
        binary(&op[..op.len().saturating_sub(1)], old, rhs)
      };
      resolver.write(left, &mut right.env, frame, false, value(number));
      right.env.order = previous_order;
      if conflict {
        for name in resolver.arithmetic_names(node, frame).1 {
          right.env.alternatives.forget(&name);
          if let Some(binding) = right.env.variables.values.get_mut(&name) { binding.taint(ResolutionIssue::EvaluationOrder); }
        }
      }
      result(number, right.env)
    })
  }

  fn calculate_conditional(
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let (Some(condition), Some(yes), Some(no)) = (
      node.child_by_field_name("condition"),
      node.child_by_field_name("consequence"),
      node.child_by_field_name("alternative"),
    ) else {
      return result(Err(ValueIssue::Recovery), env);
    };
    Task::arithmetic(condition, env, frame).then_word(move |_, condition| {
      if let Ok(n) = integer(&condition.word.scalar()) {
        return Task::arithmetic(
          if n == 0 { no } else { yes },
          condition.env,
          frame,
        );
      }
      let before = condition.env.clone();
      Task::arithmetic(yes, condition.env, frame).then_word(
        move |_, mut yes| {
          Task::arithmetic(no, before, frame).then_word(move |_, no| {
            yes.env.join(&no.env);
            let mut v = yes.word.scalar();
            v.join(&no.word.scalar());
            Task::expanded(Expansion {
              word: Word::value(v, false),
              env: yes.env,
            })
          })
        },
      )
    })
  }

  fn arithmetic_conflict(
    &mut self,
    left: Node<'_>,
    right: Node<'_>,
    frame: Frame,
  ) -> bool {
    let (lr, lw) = self.arithmetic_names(left, frame);
    let (rr, rw) = self.arithmetic_names(right, frame);
    let conflict =
      !lw.is_disjoint(&rr) || !rw.is_disjoint(&lr) || !lw.is_disjoint(&rw);
    if conflict {
      self.gap(left, frame, ResolutionIssue::EvaluationOrder);
    }
    conflict
  }

  fn arithmetic_names(
    &self,
    node: Node<'_>,
    frame: Frame,
  ) -> (BTreeSet<Vec<u8>>, BTreeSet<Vec<u8>>) {
    let mut reads = BTreeSet::new();
    let mut writes = BTreeSet::new();
    let mut nodes = vec![node];
    while let Some(node) = nodes.pop() {
      if let Some(site) = self
        .site(node, frame)
        .and_then(|id| self.workspace.occurrence(id))
        && let Value::Literal(name) = &site.name
      {
        if !matches!(site.access, Access::Write | Access::Definition) {
          reads.insert(name.clone());
        }
        if !matches!(site.access, Access::Read) {
          writes.insert(name.clone());
        }
      }
      nodes.extend(children(node));
    }
    (reads, writes)
  }
}

struct ArithmeticSource<'a> {
  remaining: std::vec::IntoIter<Node<'a>>,
  offset: usize,
  end: usize,
  body: Vec<u8>,
  parts: Vec<OriginPart>,
  issue: Option<ValueIssue>,
}

impl<'a> Resolver<'a> {
  fn origin(
    &self,
    node: Node<'a>,
    frame: Frame,
    parts: Vec<OriginPart>,
  ) -> Option<DerivedOrigin> {
    let mut parent = Some(node);
    while let Some(current) = parent {
      let site = self.site(current, frame).or_else(|| {
        current
          .child_by_field_name("name")
          .or_else(|| current.child_by_field_name("word"))
          .and_then(|name| self.site(name, frame))
      });
      if let Some(registration) = site {
        return Some(DerivedOrigin {
          registration,
          argument: frame.location(node),
          parts,
        });
      }
      parent = current.parent();
    }
    None
  }

  fn expanded_arithmetic(
    &mut self,
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let Some(expression) = node.child_by_field_name("expression") else {
      return self.arithmetic_unknown(node, env, frame, ValueIssue::Recovery);
    };
    let mut pending = vec![expression];
    let mut expansions = Vec::new();
    while let Some(child) = pending.pop() {
      if matches!(
        child.kind(),
        "parameter_expansion"
          | "arithmetic_expansion"
          | "command_substitution"
          | "backquote_substitution"
      ) {
        expansions.push(child);
      } else {
        pending.extend(children(child).into_iter().rev());
      }
    }
    if expansions.is_empty() {
      return self.arithmetic_unknown(node, env, frame, ValueIssue::Arithmetic);
    }
    let input = ArithmeticSource {
      remaining: expansions.into_iter(),
      offset: expression.start_byte(),
      end: expression.end_byte(),
      body: b": $((".to_vec(),
      parts: vec![OriginPart::new(0..5, None)],
      issue: None,
    };
    self.arithmetic_source(node, input, env, frame)
  }

  fn arithmetic_source(
    &mut self,
    node: Node<'a>,
    mut input: ArithmeticSource<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    if let Some(expansion) = input.remaining.next() {
      self.arithmetic_literal(&mut input, expansion.start_byte(), frame);
      input.offset = expansion.end_byte();
      return Task::expand(expansion, env, frame).then_word(
        move |resolver, result| {
          match result.word.scalar() {
            Value::Literal(bytes) if input.issue.is_none() => {
              let offset = input.body.len();
              if bytes.len()
                > resolver
                  .result
                  .request
                  .limits
                  .value_bytes
                  .saturating_sub(offset)
              {
                input.issue = Some(ValueIssue::SizeLimit);
              } else {
                input
                  .parts
                  .push(OriginPart::new(offset..offset + bytes.len(), None));
                input.body.extend(bytes);
              }
            }
            value @ (Value::Unknown(_) | Value::Partial(_)) => {
              let issue = value.issue().expect("unknown arithmetic input");
              input.issue =
                Some(input.issue.map_or(issue, |old| old.combine(issue)));
            }
            Value::Literal(_) => {}
          }
          resolver.arithmetic_source(node, input, result.env, frame)
        },
      );
    }
    let end = input.end;
    self.arithmetic_literal(&mut input, end, frame);
    if let Some(issue) = input.issue {
      return self.arithmetic_unknown(node, env, frame, issue);
    }
    if !arithmetic_text(&input.body[5..]) {
      return self.arithmetic_unknown(node, env, frame, ValueIssue::Arithmetic);
    }
    let offset = input.body.len();
    input.body.extend(b"))");
    input.parts.push(OriginPart::new(offset..offset + 2, None));
    let Some(origin) = self.origin(node, frame, input.parts) else {
      return self.arithmetic_unknown(node, env, frame, ValueIssue::Dynamic);
    };
    let key = self.derive_body(origin, &input.body);
    let Some(snapshot) = self.workspace.derived_keys.get(&key).copied() else {
      return self.arithmetic_unknown(node, env, frame, ValueIssue::Dynamic);
    };
    let document = self
      .workspace
      .document(snapshot.document)
      .expect("derived document");
    if document.has_syntax_errors() {
      return self.arithmetic_unknown(node, env, frame, ValueIssue::Arithmetic);
    }
    let mut nodes = vec![document.tree.root_node()];
    while let Some(arithmetic) = nodes.pop() {
      if arithmetic.kind() == "arithmetic_expansion" {
        let inner = self.execution(
          node,
          frame,
          ExecutionKind::Arithmetic(snapshot),
          snapshot,
          false,
        );
        return Task::arithmetic(arithmetic, env, inner);
      }
      nodes.extend(children(arithmetic));
    }
    self.arithmetic_unknown(node, env, frame, ValueIssue::Arithmetic)
  }

  fn arithmetic_literal(
    &self,
    input: &mut ArithmeticSource<'_>,
    end: usize,
    frame: Frame,
  ) {
    let source = self
      .workspace
      .document(frame.snapshot.document)
      .expect("snapshot")
      .source
      .as_bytes();
    let mut offset = input.offset;
    while offset < end && input.issue.is_none() {
      if input.body.len() >= self.result.request.limits.value_bytes {
        input.issue = Some(ValueIssue::SizeLimit);
        break;
      }
      if source.get(offset..offset + 2) == Some(b"\\\n") {
        offset += 2;
        continue;
      }
      let at = input.body.len();
      input.body.push(source[offset]);
      let part = OriginPart::new(at..at + 1, Some(offset..offset + 1));
      if !input.parts.last_mut().is_some_and(|last| last.merge(&part)) {
        input.parts.push(part);
      }
      offset += 1;
    }
    input.offset = end;
  }

  fn arithmetic_unknown(
    &mut self,
    node: Node<'a>,
    mut env: Environment,
    frame: Frame,
    issue: ValueIssue,
  ) -> Task<'a> {
    self.invalidate(
      node,
      &mut env,
      frame,
      if issue.causes().any(|cause| cause == ValueIssue::SizeLimit) {
        ResolutionIssue::ValueLimit
      } else {
        ResolutionIssue::UnknownEffect
      },
      [crate::interface::application::engine::EffectTarget::Variables],
    );
    Task::expanded(Expansion {
      word: Word::unknown(issue),
      env,
    })
  }
}

fn arithmetic_text(bytes: &[u8]) -> bool {
  let mut depth = 0usize;
  for c in bytes {
    match c {
      b'(' => depth += 1,
      b')' => {
        let Some(next) = depth.checked_sub(1) else {
          return false;
        };
        depth = next;
      }
      _ if c.is_ascii_alphanumeric()
        || b"_+-*/%<>=!&|^~?: \t\n".contains(c) => {}
      _ => return false,
    }
  }
  depth == 0 && !bytes.iter().all(u8::is_ascii_whitespace)
}

fn dynamic_arithmetic(root: Node<'_>) -> bool {
  let mut pending = children(root);
  while let Some(node) = pending.pop() {
    match node.kind() {
      "parameter_expansion"
      | "command_substitution"
      | "backquote_substitution"
      | "arithmetic_dynamic_expression"
      | "parenthesized_arithmetic_dynamic_source"
      | "arithmetic_expansion" => return true,
      _ => pending.extend(children(node)),
    }
  }
  false
}
