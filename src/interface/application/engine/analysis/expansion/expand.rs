use super::super::execution::{Environment, Transfer};
use super::super::{Frame, Resolver, Task};
use crate::interface::application::engine::word::{
  self, Part, Parts, Word, WordContext,
};
use crate::interface::application::engine::{
  Alternative, OccurrenceId, OriginPart, ResolutionIssue, Value, ValueIssue,
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
      self.input(crate::interface::application::engine::InputQuery::Encoding(
        locale.to_vec()
      )),
      crate::interface::application::engine::InputResult::Encoding(
        crate::interface::application::engine::CharacterEncoding::Utf8
      )
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
      || site.access == crate::interface::application::engine::Access::Unknown
      || matches!(
        site.kind,
        crate::interface::application::engine::OccurrenceKind::SpecialParameter
          | crate::interface::application::engine::OccurrenceKind::PositionalParameter
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
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let document = self
      .workspace
      .document(frame.snapshot.document)
      .expect("current snapshot");
    let mut initial = Word::default();
    if node.has_error() || node.is_missing() {
      initial.fail(ValueIssue::Recovery);
      self.gap(node, frame, ResolutionIssue::Recovery);
    }
    if word::transformed(node, document.source.as_bytes())
      && std::iter::successors(node.parent(), tree_sitter::Node::parent)
        .any(|parent| parent.kind() == "backquote_substitution")
    {
      initial.fail(ValueIssue::TransformedSource);
    }
    self.expand_parts(
      Parts::new(node, document.source.as_bytes()),
      Expansion { word: initial, env },
      frame,
    )
  }

  pub(in crate::interface::application::engine::analysis) fn expand_here(
    &mut self,
    node: Node<'a>,
    strip: bool,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let source = self
      .workspace
      .document(frame.snapshot.document)
      .expect("document")
      .source
      .as_bytes();
    self.expand_parts(
      Parts::new(node, source).strip_tabs(strip),
      Expansion {
        word: Word::default(),
        env,
      },
      frame,
    )
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
      return self.expand_parameter(node, quoted, env, frame);
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
      let source = self
        .workspace
        .document(frame.snapshot.document)
        .expect("snapshot")
        .source
        .as_bytes();
      let value = if let Some(user) = node.child_by_field_name("user") {
        let user = word::lexical(user, source);
        if let Some(home) = self.result.request.initial.user_homes.get(&user) {
          Value::Literal(home.clone())
        } else {
          match self.input(
            crate::interface::application::engine::InputQuery::Home(user),
          ) {
            crate::interface::application::engine::InputResult::Path(home) => {
              Value::Literal(home)
            }
            crate::interface::application::engine::InputResult::Unknown(
              issue,
            ) => Value::Unknown(issue),
            _ => Value::Unknown(ValueIssue::Unspecified),
          }
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
        env: flow.0.remove(&Transfer::Next).unwrap_or(before),
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
    if word.issue == Some(ValueIssue::SizeLimit) {
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
      if field.constraint.is_some() {
        if field
          .bytes
          .iter()
          .zip(&field.quoted)
          .any(|(byte, quoted)| !quoted && b"*?[".contains(byte))
          && env.noglob != Some(true)
        {
          return Err(ValueIssue::PathnameExpansion);
        }
        expanded.push(field);
        continue;
      }
      if !field
        .bytes
        .iter()
        .zip(&field.quoted)
        .any(|(c, q)| !q && b"*?[".contains(c))
      {
        expanded.push(field);
        continue;
      }
      let pattern = word::Pattern::compile(
        &field.bytes,
        &field.quoted,
        utf8,
        env.portable_collation(),
      )?;
      if pattern.is_literal() {
        expanded.push(field);
        continue;
      }
      if env.noglob != Some(false) {
        return Err(ValueIssue::PathnameExpansion);
      }
      let cwd = if field.bytes.starts_with(b"/") {
        Vec::new()
      } else {
        let Value::Literal(cwd) = &env.cwd else {
          return Err(ValueIssue::PathnameExpansion);
        };
        cwd.clone()
      };
      let query = crate::interface::application::engine::InputQuery::Glob {
        cwd,
        pattern: field.bytes.clone(),
        quoted: field.quoted.clone(),
        utf8,
        portable_collation: env.portable_collation(),
      };
      match self.input(query) {
        crate::interface::application::engine::InputResult::Paths(paths)
          if paths.is_empty() =>
        {
          expanded.push(field);
        }
        crate::interface::application::engine::InputResult::Paths(paths) => {
          expanded.extend(paths.into_iter().map(|bytes| word::Field {
            quoted: vec![true; bytes.len()],
            bytes,
            origins: Vec::new(),
            ..word::Field::default()
          }));
        }
        crate::interface::application::engine::InputResult::Unknown(
          ValueIssue::Dynamic,
        ) => {
          return Err(ValueIssue::PathnameExpansion);
        }
        crate::interface::application::engine::InputResult::Unknown(issue) => {
          return Err(issue);
        }
        _ => return Err(ValueIssue::PathnameExpansion),
      }
    }
    Ok(expanded)
  }
}

#[derive(Clone)]
pub(in crate::interface::application::engine::analysis) struct Argument<'a> {
  pub node: Node<'a>,
  pub value: Value,
  pub origins: Vec<OriginPart>,
  pub boundary_known: bool,
  pub kind: Option<super::super::ArgumentKind>,
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
      Task::expand(target, env, frame)
    } else {
      Task::expanded(super::Expansion {
        word: crate::interface::application::engine::word::Word::default(),
        env,
      })
    };
    task.then_word(move |resolver, result| {
      match resolver.fields(node, &result.word, &result.env, frame, context) {
        Ok(fields) => values.extend(fields.into_iter().map(|field| Argument {
          node,
          value: field.value(),
          origins: field.origins,
          boundary_known: true,
          kind: None,
        })),
        Err(issue) => values.push(Argument {
          node,
          value: Value::Unknown(issue),
          origins: Vec::new(),
          boundary_known: context == WordContext::Assignment
            || (!result.word.unknown_fields && !result.word.ambiguous_empty),
          kind: None,
        }),
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
  condition: Option<bool>,
}

impl<'a> Resolver<'a> {
  fn expand_parameter(
    &mut self,
    node: Node<'a>,
    quoted: bool,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let parameter = node.child_by_field_name("parameter");
    let id = parameter.and_then(|p| self.site(p, frame));
    if let Some(id) = id {
      self.read(id, &env, frame);
    }
    let source = self
      .workspace
      .document(frame.snapshot.document)
      .expect("current snapshot")
      .source
      .as_bytes();
    let name = parameter
      .map(|p| word::lexical(p, source))
      .unwrap_or_default();
    let (value, _) = env.parameter(&name);
    let Some(operator) = node.child_by_field_name("operator") else {
      return self.plain_parameter(node, quoted, env, frame);
    };
    let op = word::lexical(operator, source);
    let condition =
      self.parameter_condition(id, &value, &env, op.starts_with(b":"));
    let operation = op.last().copied();
    if operator.kind() == "parameter_length_operator" {
      let result = if parameter.is_some_and(|p| {
        matches!(word::lexical(p, source).as_slice(), b"@" | b"*")
      }) {
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
      let undefined = parameter.is_some_and(|p| {
        matches!(word::lexical(p, source).as_slice(), b"@" | b"*" | b"#")
      });
      let pattern = node.child_by_field_name("pattern");
      let task = pattern.map_or_else(
        || {
          Task::expanded(Expansion {
            word: Word::default(),
            env: env.clone(),
          })
        },
        |pattern| Task::expand(pattern, env.clone(), frame),
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
          .map_or_else(Value::Unknown, |pattern| {
            pattern.remove(
              &value,
              op.starts_with(b"%"),
              op.len() == 2,
              utf8,
              resolver.cancelled,
            )
          })
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
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let source = self
      .workspace
      .document(frame.snapshot.document)
      .expect("snapshot")
      .source
      .as_bytes();
    let name = node
      .child_by_field_name("parameter")
      .map(|p| word::lexical(p, source))
      .unwrap_or_default();
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
      && env.positional.as_ref().is_some_and(Vec::is_empty)
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
    if choose == Some(false) {
      return Task::expanded(Expansion {
        word: Word::value(skipped, quoted),
        env,
      });
    }
    let before = env.clone();
    let task = if let Some(body) = node.child_by_field_name("word") {
      Task::expand(body, env, frame)
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
        result.word = Word::value(Value::Unknown(ValueIssue::Dynamic), quoted);
      } else {
        for fragment in &mut result.word.fragments {
          fragment.quoted |= quoted;
          fragment.split = !fragment.quoted;
        }
      }
      if choose.is_none() {
        result.env.join(&before);
        let mut joined = result.word.scalar();
        joined.join(&skipped);
        result.word = Word::value(joined, quoted);
        if !quoted {
          result.word.field_issue = Some(ValueIssue::FieldSplitting);
          result.word.unknown_fields = true;
        }
      }
      Task::expanded(result)
    })
  }

  fn parameter_condition(
    &self,
    id: Option<OccurrenceId>,
    value: &Value,
    env: &Environment,
    colon: bool,
  ) -> Option<bool> {
    let absent =
      id.and_then(|id| self.workspace.occurrence(id))
        .and_then(|site| match &site.name {
          Value::Literal(name) => env.parameter(name).1,
          Value::Unknown(_) | Value::Partial(_) => None,
        });
    let empty = value.empty();
    if colon {
      match (absent, empty) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
      }
    } else {
      absent
    }
  }
}

impl Environment {
  pub(in crate::interface::application::engine::analysis) fn parameter(
    &self,
    name: &[u8],
  ) -> (Value, Option<bool>) {
    if name == b"#" {
      return (
        self
          .positional
          .as_ref()
          .map_or(Value::Unknown(ValueIssue::Dynamic), |args| {
            Value::Literal(args.len().to_string().into_bytes())
          }),
        Some(false),
      );
    }
    if name != b"0" && name.iter().all(u8::is_ascii_digit) && !name.is_empty() {
      let index = std::str::from_utf8(name)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .and_then(|n| n.checked_sub(1));
      return self.positional.as_ref().map_or(
        (Value::Unknown(ValueIssue::Dynamic), None),
        |args| {
          index
            .and_then(|i| args.get(i))
            .map_or((Value::Literal(Vec::new()), Some(true)), |v| {
              (v.clone(), Some(false))
            })
        },
      );
    }
    if matches!(name, b"?" | b"-" | b"$" | b"!" | b"0") {
      return (
        self
          .special
          .get(name)
          .cloned()
          .unwrap_or(Value::Unknown(ValueIssue::Dynamic)),
        if name == b"!" { None } else { Some(false) },
      );
    }
    if matches!(name, b"@" | b"*") {
      return (Value::Unknown(ValueIssue::Unspecified), None);
    }
    if self.uncertain_variable(name) {
      return (Value::Unknown(ValueIssue::Dynamic), None);
    }
    let binding = self.variables.get(name);
    let resolution = &binding.resolution;
    let absent = if !resolution.issues.is_empty() {
      None
    } else if resolution.candidates.is_empty()
      && resolution.alternatives == [Alternative::Unset].into()
    {
      Some(true)
    } else if !resolution.alternatives.contains(&Alternative::Unset) {
      Some(false)
    } else {
      None
    };
    (binding.value.as_ref().clone(), absent)
  }

  pub(in crate::interface::application::engine::analysis) fn ifs(
    &self,
  ) -> Value {
    let (value, unset) = self.parameter(b"IFS");
    if unset == Some(true) {
      Value::Literal(b" \t\n".to_vec())
    } else {
      value
    }
  }

  fn positional_word(
    &self,
    name: &[u8],
    quoted: bool,
    limit: usize,
    utf8: bool,
  ) -> Word {
    let Some(args) = &self.positional else {
      return Word::unknown(ValueIssue::Dynamic);
    };
    let separator = if name == b"*" {
      let Value::Literal(ifs) = self.ifs() else {
        return Word::unknown(ValueIssue::FieldSplitting);
      };
      let Ok(chars) =
        crate::interface::application::engine::word::characters(&ifs, utf8)
      else {
        return Word::unknown(ValueIssue::Locale);
      };
      Some(
        chars
          .first()
          .map_or(Vec::new(), |c| c.to_string().into_bytes()),
      )
    } else {
      None
    };
    let word = Word::parameters(args, quoted, separator.as_deref(), limit);
    if name == b"*" && quoted {
      Word::value(word.scalar(), true)
    } else {
      word
    }
  }
}

pub(in crate::interface::application::engine::analysis) fn positional_arguments(
  args: &[Argument<'_>],
) -> Option<Vec<Value>> {
  args
    .iter()
    .all(|arg| arg.boundary_known)
    .then(|| args.iter().map(|arg| arg.value.clone()).collect())
}
