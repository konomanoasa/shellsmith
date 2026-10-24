use imbl::OrdSet;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

use super::super::execution::{
  CommandFailure, Environment, Transfer, children,
};
use super::super::{Frame, Resolver, Task};
use super::Expansion;
use crate::interface::application::engine::word::{self, Word};
use crate::interface::application::engine::workspace::parser;
use crate::interface::application::engine::{
  Access, DerivedOrigin, EffectTarget, ExecutionKind, OriginPart,
  ResolutionIssue, Snapshot, Value, ValueIssue,
};

type Number = Result<i64, ValueIssue>;

pub(super) fn integer(value: &Value) -> Number {
  let Value::Literal(bytes) = value else {
    let Some(issue) = value.issue() else {
      unreachable!()
    };
    return Err(issue);
  };
  let text = std::str::from_utf8(bytes).map_err(|_| ValueIssue::Arithmetic)?;
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
  if !digits.chars().all(|digit| digit.is_digit(radix)) {
    return Err(ValueIssue::Arithmetic);
  }
  let n =
    i128::from_str_radix(digits, radix).map_err(|_| ValueIssue::Arithmetic)?;
  let n = if negative { -n } else { n };
  i64::try_from(n).map_err(|_| ValueIssue::Arithmetic)
}

pub(super) fn value(number: Number) -> Value {
  let number = number.and_then(|n| {
    std::os::raw::c_long::try_from(n)
      .map(|_| n)
      .map_err(|_| ValueIssue::Arithmetic)
  });
  number.map_or_else(Value::Unknown, |n| {
    Value::Literal(n.to_string().into_bytes())
  })
}

fn result<'a>(number: Number, mut env: Environment) -> Task<'a> {
  let value = value(number);
  if let Some(issue) = value.issue() {
    env.failure.uncertain(issue);
  }
  Task::expanded(Expansion {
    word: Word::value(value, false),
    env,
  })
}

pub(super) fn binary(operator: &[u8], left: Number, right: Number) -> Number {
  let (a, b) = match (left, right) {
    (Ok(a), Ok(b)) => (a, b),
    (Err(left), Err(right)) => return Err(left.combine(right)),
    (Err(issue), Ok(_)) | (Ok(_), Err(issue)) => return Err(issue),
  };
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

pub(super) fn unary(operator: &[u8], operand: Number) -> Number {
  operand.and_then(|n| {
    match operator {
      b"+" => Some(n),
      b"-" => n.checked_neg(),
      b"!" => Some(i64::from(n == 0)),
      b"~" => Some(!n),
      _ => None,
    }
    .ok_or(ValueIssue::Arithmetic)
  })
}

#[derive(Clone, Default)]
struct VariableAccess {
  reads: OrdSet<Vec<u8>>,
  writes: OrdSet<Vec<u8>>,
}

impl VariableAccess {
  fn join(&mut self, other: &Self) {
    union(&mut self.reads, &other.reads);
    union(&mut self.writes, &other.writes);
  }

  /// Unsequenced access conflicts (C17 6.5p2).
  fn conflicts(&self, other: &Self) -> bool {
    intersects(&self.writes, &other.reads)
      || intersects(&other.writes, &self.reads)
      || intersects(&self.writes, &other.writes)
  }
}

fn union(into: &mut OrdSet<Vec<u8>>, other: &OrdSet<Vec<u8>>) {
  if into.len() < other.len() {
    let smaller = std::mem::replace(into, other.clone());
    into.extend(smaller);
  } else {
    into.extend(other.iter().cloned());
  }
}

fn intersects(left: &OrdSet<Vec<u8>>, right: &OrdSet<Vec<u8>>) -> bool {
  let (smaller, larger) = if left.len() <= right.len() {
    (left, right)
  } else {
    (right, left)
  };
  smaller.iter().any(|name| larger.contains(name))
}

/// Caching avoids repeatedly walking operands of nested operators.
#[derive(Default)]
pub(in crate::interface::application::engine::analysis) struct ArithmeticAccess(
  BTreeMap<(Snapshot, usize), VariableAccess>,
);

impl<'a> Resolver<'a> {
  pub(in crate::interface::application::engine::analysis) fn arithmetic(
    &mut self,
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    if node.kind() == "arithmetic_expansion" {
      if node.has_error() {
        let parts = arithmetic_parts(children(node)).into_iter();
        return self.damaged_arithmetic(node, parts, env, frame);
      }
      let source = self.workspace.source(frame.snapshot.document);
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
    let source = self.workspace.source(frame.snapshot.document);
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
          let number = unary(&operator, integer(&expansion.word.scalar()));
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
              flow.take(Transfer::Next).unwrap_or(before),
            )
          })
      }
    }
  }

  /// Dynamic arithmetic may write any variable.
  pub(in crate::interface::application::engine::analysis) fn runtime_arithmetic(
    &mut self,
    node: Node<'a>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    self.gap(node, frame, ResolutionIssue::UnknownEffect);
    let previous_order = env.order.clone();
    env.order.insert(EffectTarget::Variables);
    Task::sequence(children(node), env, frame).then(
      move |resolver, mut flow| {
        for state in flow.environments_mut() {
          state.order.clone_from(&previous_order);
          resolver.invalidate(
            node,
            state,
            frame,
            ResolutionIssue::UnknownEffect,
            ValueIssue::Dynamic,
            [EffectTarget::Variables],
          );
        }
        Task::Done(flow)
      },
    )
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
    let source = self.workspace.source(frame.snapshot.document);
    let op = word::lexical(operator, source);
    let short = matches!(op.as_slice(), b"&&" | b"||");
    let conflict = !short && self.arithmetic_conflict(left, right, frame);
    let order = conflict.then(|| self.unordered(node, &mut env, frame));
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
          let b = integer(&right.word.scalar());
          if !conflict && matches!(op.as_slice(), b"/" | b"%") && b == Ok(0) {
            right.env.failure = CommandFailure::Definite;
          }
          let mut number = binary(&op, a, b);
          if short && a.is_err() {
            right.env.join(&before);
          }
          if let Some(order) = order {
            resolver.ordered(node, &mut right.env, frame, order);
            number = Err(ValueIssue::Unspecified);
          }
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
    let source = self.workspace.source(frame.snapshot.document);
    let op = word::lexical(operator, source);
    let id = self.site(left, frame);
    let name =
      id.and_then(|id| self.workspace.occurrence(id))
        .and_then(|site| match &site.name {
          Value::Literal(name) => Some(name.clone()),
          Value::Partial(_) | Value::Unknown(_) => None,
        });
    let conflict = name.as_ref().is_some_and(|name| {
      self.variable_access(right, frame).writes.contains(name)
    });
    let order = conflict.then(|| {
      self.gap(node, frame, ResolutionIssue::EvaluationOrder);
      self.unordered(node, &mut env, frame)
    });
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
      if !conflict && matches!(op.as_slice(), b"/=" | b"%=") && rhs == Ok(0) {
        right.env.failure = CommandFailure::Definite;
      }
      let readonly = name
        .as_ref()
        .is_some_and(|name| right.env.readonly(name).is_some());
      let number = if readonly {
        Err(ValueIssue::Dynamic)
      } else if conflict {
        Err(ValueIssue::Unspecified)
      } else if op == b"=" {
        rhs
      } else {
        binary(&op[..op.len().saturating_sub(1)], old, rhs)
      };
      resolver.write(left, &mut right.env, frame, false, value(number));
      if let Some(order) = order {
        resolver.ordered(node, &mut right.env, frame, order);
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
      let cause = match integer(&condition.word.scalar()) {
        Ok(n) => {
          return Task::arithmetic(
            if n == 0 { no } else { yes },
            condition.env,
            frame,
          );
        }
        Err(cause) => cause,
      };
      let before = condition.env.clone();
      Task::arithmetic(yes, condition.env, frame).then_word(
        move |_, mut yes| {
          Task::arithmetic(no, before, frame).then_word(move |_, no| {
            yes.env.join(&no.env);
            let mut v = yes.word.scalar();
            v.join_choice(&no.word.scalar(), Some(cause));
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
    let conflict = self
      .variable_access(left, frame)
      .conflicts(&self.variable_access(right, frame));
    if conflict {
      self.gap(left, frame, ResolutionIssue::EvaluationOrder);
    }
    conflict
  }

  /// Returns the previous order for restoration after evaluation.
  fn unordered(
    &mut self,
    node: Node<'_>,
    env: &mut Environment,
    frame: Frame,
  ) -> BTreeSet<EffectTarget> {
    let order = env.order.clone();
    let writes = self.variable_access(node, frame).writes;
    env
      .order
      .extend(writes.into_iter().map(EffectTarget::Variable));
    order
  }

  fn ordered(
    &mut self,
    node: Node<'_>,
    env: &mut Environment,
    frame: Frame,
    order: BTreeSet<EffectTarget>,
  ) {
    env.order = order;
    for name in self.variable_access(node, frame).writes {
      env.invalidate(
        EffectTarget::Variable(name),
        ResolutionIssue::EvaluationOrder,
        ValueIssue::Dynamic,
      );
    }
  }

  fn variable_access(
    &mut self,
    node: Node<'_>,
    frame: Frame,
  ) -> VariableAccess {
    let key = |node: Node<'_>| (frame.snapshot, node.id());
    let mut pending = vec![(node, false)];
    while let Some((current, expanded)) = pending.pop() {
      if self.arithmetic.0.contains_key(&key(current)) {
        continue;
      }
      if !expanded {
        pending.push((current, true));
        pending
          .extend(children(current).into_iter().map(|child| (child, false)));
        continue;
      }
      let mut access = VariableAccess::default();
      for child in children(current) {
        access.join(&self.arithmetic.0[&key(child)]);
      }
      if let Some(site) = self
        .site(current, frame)
        .and_then(|id| self.workspace.occurrence(id))
        && let Value::Literal(name) = &site.name
      {
        if !matches!(site.access, Access::Write | Access::Definition) {
          access.reads.insert(name.clone());
        }
        if !matches!(site.access, Access::Read) {
          access.writes.insert(name.clone());
        }
      }
      self.arithmetic.0.insert(key(current), access);
    }
    self.arithmetic.0[&key(node)].clone()
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
  /// Damaged arithmetic expands its intact expansions in order, and its
  /// evaluation has unknown effects.
  fn damaged_arithmetic(
    &mut self,
    node: Node<'a>,
    mut parts: std::vec::IntoIter<Node<'a>>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    while let Some(part) = parts.next() {
      if parser::damaged(part) {
        self.damaged_expansion(part, &mut env, frame);
      } else {
        return Task::expand(part, false, env, frame).then_word(
          move |resolver, result| {
            resolver.damaged_arithmetic(node, parts, result.env, frame)
          },
        );
      }
    }
    self.damaged_expansion(node, &mut env, frame);
    Task::expanded(Expansion {
      word: Word::unknown(ValueIssue::Recovery),
      env,
    })
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
    let expansions = arithmetic_parts(vec![expression]);
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
      return Task::expand(expansion, false, env, frame).then_word(
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
      let mut env = env;
      if !input.body[5..].iter().all(u8::is_ascii_whitespace) {
        env.failure = CommandFailure::Definite;
      }
      return self.arithmetic_unknown(node, env, frame, ValueIssue::Arithmetic);
    }
    let offset = input.body.len();
    input.body.extend(b"))");
    input.parts.push(OriginPart::new(offset..offset + 2, None));
    let origin = DerivedOrigin {
      argument: frame.location(node),
      parts: input.parts,
    };
    let Some(snapshot) = self.derive_body(origin, &input.body) else {
      return self.arithmetic_unknown(node, env, frame, ValueIssue::Dynamic);
    };
    let document = self
      .workspace
      .document(snapshot.document)
      .expect("derived document");
    if document.has_syntax_errors() {
      let mut env = env;
      env.failure = CommandFailure::Definite;
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
        );
        return Task::arithmetic(arithmetic, env, inner);
      }
      nodes.extend(children(arithmetic));
    }
    let mut env = env;
    env.failure = CommandFailure::Definite;
    self.arithmetic_unknown(node, env, frame, ValueIssue::Arithmetic)
  }

  fn arithmetic_literal(
    &self,
    input: &mut ArithmeticSource<'_>,
    end: usize,
    frame: Frame,
  ) {
    let source = self.workspace.source(frame.snapshot.document);
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
    env.failure.uncertain(issue);
    self.invalidate(
      node,
      &mut env,
      frame,
      if issue.causes().any(|cause| cause == ValueIssue::SizeLimit) {
        ResolutionIssue::ValueLimit
      } else {
        ResolutionIssue::UnknownEffect
      },
      issue,
      [EffectTarget::Variables],
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

/// Expansions and damaged syntax within `nodes` in source order, without their
/// contents.
fn arithmetic_parts(nodes: Vec<Node<'_>>) -> Vec<Node<'_>> {
  let mut pending: Vec<_> = nodes.into_iter().rev().collect();
  let mut parts = Vec::new();
  while let Some(node) = pending.pop() {
    if parser::damaged(node)
      || matches!(
        node.kind(),
        "parameter_expansion"
          | "arithmetic_expansion"
          | "command_substitution"
          | "backquote_substitution"
      )
    {
      parts.push(node);
    } else {
      pending.extend(children(node).into_iter().rev());
    }
  }
  parts
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
