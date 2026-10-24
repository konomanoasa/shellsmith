use super::super::execution::{
  CommandFailure, Environment, Positionals, Transfer, damage_outside,
};
use super::super::{Frame, Resolver, Task};
use crate::interface::application::engine::word::{
  self, FieldCount, Part, Parts, Word, WordContext,
};
use crate::interface::application::engine::{
  Access, CharacterEncoding, EffectTarget, InputQuery, InputResult,
  OccurrenceId, OccurrenceKind, OriginPart, ResolutionIssue, Value, ValueIssue,
};
use std::sync::Arc;
use tree_sitter::Node;

pub(in crate::interface::application::engine::analysis) struct Expansion {
  pub word: Word,
  pub env: Environment,
}

impl Resolver<'_> {
  pub(in crate::interface::application::engine::analysis) fn utf8(
    &mut self,
    env: &Environment,
  ) -> bool {
    let Some(locale) = env.locale(b"LC_CTYPE") else {
      return false;
    };
    if matches!(locale, b"C" | b"POSIX") {
      return false;
    }
    matches!(
      self.input(&InputQuery::Encoding(locale.to_vec())),
      InputResult::Encoding(CharacterEncoding::Utf8)
    )
  }

  pub(in crate::interface::application::engine::analysis) fn variable_value<
    'v,
  >(
    &self,
    id: OccurrenceId,
    env: &'v Environment,
  ) -> Option<&'v Arc<Value>> {
    let site = self.workspace.occurrence(id)?;
    let Value::Literal(name) = &site.name else {
      return None;
    };
    if env.uncertain_variable(name)
      || site.access == Access::Unknown
      || matches!(
        site.kind,
        OccurrenceKind::SpecialParameter | OccurrenceKind::PositionalParameter
      )
    {
      return None;
    }
    Some(&env.variables.get(name).value)
  }
}

impl<'a> Resolver<'a> {
  pub(in crate::interface::application::engine::analysis) fn expand_start(
    &mut self,
    node: Node<'a>,
    quoted: bool,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let source = self.workspace.source(frame.snapshot.document);
    let mut initial = self.source_word(node, frame);
    if word::transformed(node, source)
      && std::iter::successors(node.parent(), tree_sitter::Node::parent)
        .any(|parent| parent.kind() == "backquote_substitution")
    {
      initial.fail(ValueIssue::TransformedSource);
    }
    let strip = self.index.strips_tabs(frame.snapshot, node, source);
    self.expand_parts(
      Parts::new(node, quoted, source).strip_tabs(strip),
      Expansion { word: initial, env },
      frame,
    )
  }

  /// Damaged syntax leaves the word without a value.
  fn source_word(&mut self, node: Node<'a>, frame: Frame) -> Word {
    let mut word = Word::default();
    if node.has_error() {
      word.fail(ValueIssue::Recovery);
      self.gap(node, frame, ResolutionIssue::Recovery);
    }
    word
  }

  /// Damaged expansion syntax may hide substitutions and assignments, which
  /// read input, fail or assign any variable.
  pub(in crate::interface::application::engine::analysis) fn damaged_expansion(
    &mut self,
    node: Node<'_>,
    env: &mut Environment,
    frame: Frame,
  ) {
    env.failure.uncertain(ValueIssue::Recovery);
    env.io.consume_input(None);
    self.invalidate(
      node,
      env,
      frame,
      ResolutionIssue::Recovery,
      ValueIssue::Recovery,
      [EffectTarget::Variables],
    );
  }

  fn expand_parts(
    &mut self,
    mut parts: Parts<'a, 'a>,
    mut expansion: Expansion,
    frame: Frame,
  ) -> Task<'a> {
    let limit = self.result.request.limits.value_bytes;
    while let Some(part) = parts.next() {
      match part {
        Part::Fragment(fragment) => expansion.word.append(fragment, limit),
        Part::Damaged(node) => {
          expansion.word.fail(ValueIssue::Recovery);
          self.damaged_expansion(node, &mut expansion.env, frame);
        }
        Part::Invalid(issue) => expansion.word.fail(issue),
        Part::Expansion(node, quoted) => {
          let Expansion { mut word, env } = expansion;
          return self.expand_part(node, quoted, env, frame).then_word(
            move |resolver, next| {
              word.extend(next.word, limit);
              resolver.expand_parts(
                parts,
                Expansion {
                  word,
                  env: next.env,
                },
                frame,
              )
            },
          );
        }
      }
    }
    Task::expanded(expansion)
  }

  fn expand_part(
    &mut self,
    node: Node<'a>,
    quoted: bool,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    if node.kind() == "parameter_expansion" {
      let task = self.expand_parameter(node, quoted, env, frame);
      if damage_outside(node, &["word", "pattern"]).is_empty() {
        return task;
      }
      // The operator is unknown after the parameter and word expand.
      return task.then_word(move |resolver, mut result| {
        resolver.damaged_expansion(node, &mut result.env, frame);
        Task::expanded(result)
      });
    }
    if node.kind() == "arithmetic_expansion" {
      return Task::arithmetic(node, env, frame).then_word(
        move |_, mut result| {
          result.word = Word::value(result.word.scalar(), quoted);
          Task::expanded(result)
        },
      );
    }
    if matches!(
      node.kind(),
      "command_substitution" | "backquote_substitution"
    ) {
      return self.substitute(node, quoted, env, frame);
    }
    if node.kind() == "tilde_expansion" {
      let source = self.workspace.source(frame.snapshot.document);
      let value = if let Some(user) = node.child_by_field_name("user") {
        let user = word::lexical(user, source);
        match self.input(&InputQuery::Home(user)) {
          InputResult::Path(home) => Value::Literal(home),
          InputResult::Unknown(issue) => Value::Unknown(issue),
          _ => Value::Unknown(ValueIssue::Unspecified),
        }
      } else {
        let (home, absent) = env.parameter(b"HOME");
        if absent == Some(true) {
          Value::Unknown(ValueIssue::Unspecified)
        } else {
          home
        }
      };
      return Task::expanded(Expansion {
        word: Word::value(value, true),
        env,
      });
    }
    let before = env.clone();
    Task::eval(node, env, frame).then(move |_, mut flow| {
      Task::expanded(Expansion {
        word: Word::value(
          Value::Unknown(ValueIssue::UnsupportedExpansion),
          quoted,
        ),
        env: flow.take(Transfer::Next).unwrap_or(before),
      })
    })
  }

  pub(in crate::interface::application::engine::analysis) fn fields(
    &mut self,
    node: Node<'a>,
    word: &Word,
    env: &Environment,
    frame: Frame,
    context: WordContext,
  ) -> Result<Vec<word::Field>, ValueIssue> {
    if word
      .issue
      .is_some_and(|issue| issue.causes().any(|c| c == ValueIssue::SizeLimit))
    {
      self.gap(node, frame, ResolutionIssue::ValueLimit);
    }
    let ifs = env.ifs();
    let utf8 = self.utf8(env);
    let fields = word.fields(context, &ifs, utf8)?;
    if context == WordContext::Assignment || env.noglob == Some(true) {
      return Ok(fields);
    }
    let mut expanded = Vec::new();
    for field in fields {
      match self.pathname_fields(word, &field, env, utf8) {
        Ok(fields) => expanded.extend(fields),
        Err(issue) => expanded.push(word::Field::unknown(
          Value::Unknown(
            field
              .value()
              .issue()
              .map_or(issue, |value| value.combine(issue)),
          ),
          FieldCount::Unknown {
            minimum: field.count.minimum(),
            issue,
          },
        )),
      }
    }
    Ok(expanded)
  }

  fn pathname_fields(
    &mut self,
    word: &Word,
    field: &word::Field,
    env: &Environment,
    utf8: bool,
  ) -> Result<Vec<word::Field>, ValueIssue> {
    if field.count.boundary().is_err() {
      return Ok(vec![field.clone()]);
    }
    if !field
      .bytes
      .iter()
      .zip(&field.quoted)
      .any(|(byte, quoted)| !quoted && b"*?[".contains(byte))
    {
      return Ok(vec![field.clone()]);
    }
    let pattern =
      word::Pattern::field(word, field, utf8, env.portable_collation())?;
    if pattern.is_literal() {
      return Ok(vec![field.clone()]);
    }
    if env.noglob != Some(false) {
      return Err(ValueIssue::PathnameExpansion);
    }
    let cwd = if field.bytes.starts_with(b"/") {
      Vec::new()
    } else {
      let Value::Literal(cwd) = &env.cwd else {
        return Err(env.cwd.issue().expect("unknown cwd"));
      };
      cwd.clone()
    };
    let query = InputQuery::Glob {
      cwd,
      pattern: field.bytes.clone(),
      quoted: field.quoted.clone(),
      utf8,
      portable_collation: env.portable_collation(),
    };
    match self.input(&query) {
      InputResult::Paths(paths) if paths.is_empty() => Ok(vec![field.clone()]),
      InputResult::Paths(paths) => Ok(
        paths
          .into_iter()
          .map(|bytes| word::Field {
            quoted: vec![true; bytes.len()],
            bytes,
            origins: Vec::new(),
            ..word::Field::default()
          })
          .collect(),
      ),
      InputResult::Unknown(issue) => Err(issue),
      _ => Err(ValueIssue::PathnameExpansion),
    }
  }
}

#[derive(Clone)]
pub(in crate::interface::application::engine::analysis) struct Argument<'a> {
  pub node: Node<'a>,
  pub value: Value,
  pub origins: Vec<OriginPart>,
  pub count: FieldCount,
  pub kind: Option<super::super::ArgumentKind>,
}

impl Argument<'_> {
  pub fn take_first(args: &mut Vec<Self>) -> Value {
    if args[0].count.boundary().is_ok() {
      return args.remove(0).value;
    }
    let node = args[0].node;
    let mut values = positional_arguments(args);
    let name = values.parameter(0).0;
    if values.shift(1) {
      args.drain(..args.len() - values.fields.len());
      for (arg, (value, count)) in args.iter_mut().zip(values.fields) {
        arg.value = value;
        arg.count = count;
      }
      if let Some(first) = args.first_mut() {
        first.node = node;
        first.origins.clear();
      }
    } else {
      let issue = values.count().issue().expect("uncertain first argument");
      let value = values
        .fields
        .into_iter()
        .map(|(value, _)| value)
        .reduce(|mut left, right| {
          left.join(&right);
          left
        })
        .unwrap_or(Value::Unknown(issue));
      args.clear();
      args.push(Self {
        node,
        value,
        count: FieldCount::Unknown { minimum: 0, issue },
        origins: Vec::new(),
        kind: None,
      });
    }
    name
  }
}

type Ready<'a> = Box<
  dyn FnOnce(&mut Resolver<'a>, Vec<Argument<'a>>, Environment) -> Task<'a>
    + 'a,
>;

impl<'a> Resolver<'a> {
  pub(in crate::interface::application::engine::analysis) fn arguments(
    &mut self,
    mut nodes: std::vec::IntoIter<Node<'a>>,
    mut values: Vec<Argument<'a>>,
    env: Environment,
    frame: Frame,
    ready: Ready<'a>,
  ) -> Task<'a> {
    let Some(node) = nodes.next() else {
      return ready(self, values, env);
    };
    let assignment = node.child_by_field_name("name").is_some();
    let target = if assignment {
      node.child_by_field_name("value")
    } else {
      Some(node)
    };
    let context = if assignment {
      WordContext::Assignment
    } else {
      WordContext::Argument
    };
    let task = if let Some(target) = target {
      Task::expand(target, false, env, frame)
    } else {
      Task::expanded(super::Expansion {
        word: Word::default(),
        env,
      })
    };
    task.then_word(move |resolver, result| {
      match resolver.fields(node, &result.word, &result.env, frame, context) {
        Ok(fields) => values.extend(fields.into_iter().map(|field| Argument {
          node,
          value: field.value(),
          origins: field.origins,
          count: field.count,
          kind: None,
        })),
        Err(issue) => {
          let source = resolver.workspace.source(frame.snapshot.document);
          values.push(Argument {
            node,
            value: Value::Unknown(issue),
            origins: Vec::new(),
            count: if context == WordContext::Assignment
              || word::syntax_field(node, source).1
            {
              FieldCount::One
            } else {
              FieldCount::Unknown {
                minimum: usize::from(word::has_guaranteed_field(node, source)),
                issue: result.word.boundary_issue().unwrap_or(issue),
              }
            },
            kind: None,
          });
        }
      }
      resolver.arguments(nodes, values, result.env, frame, ready)
    })
  }
}

struct Conditional<'a> {
  parameter: Option<Node<'a>>,
  value: Value,
  quoted: bool,
  operation: Option<u8>,
  condition: Result<bool, ValueIssue>,
}

impl<'a> Resolver<'a> {
  fn expand_parameter(
    &mut self,
    node: Node<'a>,
    quoted: bool,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let parameter = node.child_by_field_name("parameter");
    let id = parameter.and_then(|p| self.site(p, frame));
    if let Some(id) = id {
      self.read(id, &env, frame);
    }
    let source = self.workspace.source(frame.snapshot.document);
    let name = parameter
      .map(|p| word::lexical(p, source))
      .unwrap_or_default();
    let Some(operator) = node.child_by_field_name("operator") else {
      return self.plain_parameter(node, quoted, env, frame);
    };
    let (value, _) = env.parameter(&name);
    let op = word::lexical(operator, source);
    let condition = if matches!(name.as_slice(), b"@" | b"*") {
      Err(ValueIssue::Unspecified)
    } else {
      Self::parameter_condition(&name, &value, &env, op.starts_with(b":"))
    };
    let operation = op.last().copied();
    if matches!(
      operator.kind(),
      "parameter_length_operator" | "parameter_pattern_operator"
    ) {
      env.require_parameter(&name);
    }
    if operator.kind() == "parameter_length_operator" {
      let result = if matches!(name.as_slice(), b"@" | b"*") {
        Value::Unknown(ValueIssue::Unspecified)
      } else if let Value::Literal(bytes) = value {
        word::characters(&bytes, self.utf8(&env))
          .map_or_else(Value::Unknown, |chars| {
            Value::Literal(chars.len().to_string().into_bytes())
          })
      } else {
        Value::Unknown(value.issue().expect("unknown parameter length"))
      };
      return Task::expanded(Expansion {
        word: Word::value(result, quoted),
        env,
      });
    }
    if operator.kind() == "parameter_pattern_operator" {
      let undefined = matches!(name.as_slice(), b"@" | b"*" | b"#");
      let pattern = node.child_by_field_name("pattern");
      let task = pattern.map_or_else(
        || {
          Task::expanded(Expansion {
            word: Word::default(),
            env: env.clone(),
          })
        },
        |pattern| Task::expand(pattern, false, env.clone(), frame),
      );
      return task.then_word(move |resolver, mut result| {
        let utf8 = resolver.utf8(&result.env);
        let output = if undefined {
          Value::Unknown(ValueIssue::Unspecified)
        } else {
          word::Pattern::word(
            &result.word,
            utf8,
            result.env.portable_collation(),
          )
          .map_or_else(
            |issue| {
              Value::Unknown(
                value.issue().map_or(issue, |value| value.combine(issue)),
              )
            },
            |pattern| {
              pattern.remove(
                &value,
                op.starts_with(b"%"),
                op.len() == 2,
                utf8,
                resolver.cancelled,
              )
            },
          )
        };
        result.word = Word::value(output, quoted);
        Task::expanded(result)
      });
    }

    Self::conditional_parameter(
      node,
      Conditional {
        parameter,
        value,
        quoted,
        operation,
        condition,
      },
      env,
      frame,
    )
  }

  fn plain_parameter(
    &mut self,
    node: Node<'a>,
    quoted: bool,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let source = self.workspace.source(frame.snapshot.document);
    let name = node
      .child_by_field_name("parameter")
      .map(|p| word::lexical(p, source))
      .unwrap_or_default();
    env.require_parameter(&name);
    let utf8 = self.utf8(&env);
    let mut word = if matches!(name.as_slice(), b"@" | b"*") {
      env.positional_word(
        &name,
        quoted,
        self.result.request.limits.value_bytes,
        utf8,
      )
    } else {
      Word::value(env.parameter(&name).0, quoted)
    };
    if name == b"@"
      && quoted
      && env.positional.fields.is_empty()
      && let Some(parent) =
        node.parent().filter(|p| p.kind() == "double_quoted")
    {
      word.ambiguous_empty =
        parent.named_children(&mut parent.walk()).any(|child| {
          child.id() != node.id()
            && child.kind() != "line_continuation"
            && !(child.kind() == "parameter_expansion"
              && child
                .child_by_field_name("parameter")
                .is_some_and(|p| word::lexical(p, source) == b"@"))
        });
    }
    Task::expanded(Expansion { word, env })
  }

  fn conditional_parameter(
    node: Node<'a>,
    conditional: Conditional<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let Conditional {
      parameter,
      value,
      quoted,
      operation,
      condition,
    } = conditional;
    let choose = condition.map(|condition| {
      if operation == Some(b'+') {
        !condition
      } else {
        condition
      }
    });
    let skipped = if operation == Some(b'+') {
      Value::Literal(Vec::new())
    } else {
      value
    };
    if choose == Ok(false) {
      return Task::expanded(Expansion {
        word: Word::value(skipped, quoted),
        env,
      });
    }
    let before = env.clone();
    let task = if let Some(body) = node.child_by_field_name("word") {
      Task::expand(body, quoted, env, frame)
    } else {
      Task::expanded(Expansion {
        word: Word::value(Value::Literal(Vec::new()), quoted),
        env,
      })
    };
    task.then_word(move |resolver, mut result| {
      let computed = result.word.scalar();
      let assignment_failed = operation == Some(b'=')
        && !parameter.is_some_and(|parameter| {
          parameter.kind() == "variable_name"
            && resolver.write(
              parameter,
              &mut result.env,
              frame,
              false,
              computed,
            )
        });
      if operation == Some(b'?') || assignment_failed {
        if operation == Some(b'?')
          || parameter
            .is_some_and(|parameter| parameter.kind() != "variable_name")
        {
          result.env.failure = CommandFailure::Definite;
        }
        result.word = Word::value(Value::Unknown(ValueIssue::Dynamic), quoted);
      } else {
        for fragment in &mut result.word.fragments {
          fragment.quoted |= quoted;
          fragment.split = !fragment.quoted;
        }
      }
      if let Err(cause) = choose {
        result.env.join(&before);
        let mut joined = result.word.scalar();
        if !matches!(joined, Value::Literal(_)) || joined != skipped {
          joined.join_choice(&skipped, Some(cause));
        }
        result.word = Word::value(joined, quoted);
      }
      Task::expanded(result)
    })
  }

  fn parameter_condition(
    name: &[u8],
    value: &Value,
    env: &Environment,
    colon: bool,
  ) -> Result<bool, ValueIssue> {
    let absent = env.parameter(name).1;
    let absence_issue = if name != b"0"
      && !name.is_empty()
      && name.iter().all(u8::is_ascii_digit)
    {
      env
        .positional
        .count()
        .issue()
        .unwrap_or(ValueIssue::Dynamic)
    } else {
      ValueIssue::Dynamic
    };
    if !colon {
      return absent.ok_or(absence_issue);
    }
    match (absent, value.empty()) {
      (Some(true), _) | (_, Some(true)) => Ok(true),
      (Some(false), Some(false)) => Ok(false),
      (_, None) => Err(value.issue().expect("undetermined emptiness")),
      (None, Some(false)) => Err(absence_issue),
    }
  }
}

impl Environment {
  fn positional_word(
    &self,
    name: &[u8],
    quoted: bool,
    limit: usize,
    utf8: bool,
  ) -> Word {
    let args = &self.positional.fields;
    let separator = if name == b"*"
      && args.len() <= 1
      && args.iter().all(|(_, count)| count.boundary().is_ok())
    {
      Some(Value::Literal(Vec::new()))
    } else if name == b"*" {
      let ifs = self.ifs();
      Some(match ifs {
        Value::Literal(bytes) => {
          word::characters(&bytes, utf8).map_or_else(Value::Unknown, |chars| {
            Value::Literal(
              chars
                .first()
                .map_or(Vec::new(), |c| c.to_string().into_bytes()),
            )
          })
        }
        value => Value::Unknown(value.issue().expect("unknown IFS")),
      })
    } else {
      None
    };
    let word = Word::parameters(args, quoted, separator.as_ref(), limit);
    if name == b"*" && quoted {
      Word::value(word.scalar(), true)
    } else {
      word
    }
  }
}

pub(in crate::interface::application::engine::analysis) fn positional_arguments(
  args: &[Argument<'_>],
) -> Positionals {
  Positionals {
    fields: args
      .iter()
      .map(|arg| (arg.value.clone(), arg.count))
      .collect(),
  }
}
