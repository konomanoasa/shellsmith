use super::super::execution::Environment;
use super::super::expansion::Argument;
use super::super::{Frame, Resolver};
use crate::interface::application::engine::word;
use crate::interface::application::engine::workspace::parser;
use crate::interface::application::engine::{
  Access, ArgumentKind, ArgumentRole, AttributeState, ExecutionContextId,
  Location, Resolution, Snapshot, UseSite, Value, ValueIssue, valid_name,
};
use tree_sitter::Node;

pub(super) fn argument_issue(args: &[Argument<'_>]) -> Option<ValueIssue> {
  args
    .iter()
    .flat_map(|arg| {
      arg
        .value
        .issue()
        .into_iter()
        .chain(arg.count.boundary().err())
    })
    .reduce(ValueIssue::combine)
}

pub(super) fn numeric_argument(name: &[u8]) -> bool {
  matches!(
    name,
    b"exit" | b"return" | b"shift" | b"break" | b"continue"
  )
}

pub(super) fn trap_action(args: &[Argument<'_>]) -> Result<Option<usize>, ()> {
  let mut index = 0;
  if let Some(first) = args.first() {
    if first.count.boundary().is_err() {
      return Err(());
    }
    match &first.value {
      Value::Literal(value) if value == b"--" => index = 1,
      Value::Literal(value) if value.starts_with(b"-") && value != b"-" => {
        return Ok(None);
      }
      Value::Literal(_) => {}
      value if value.may_start_with(b"-") => return Err(()),
      _ => {}
    }
  }
  let Some(action) = args.get(index) else {
    return Ok(None);
  };
  if action.count.boundary().is_err() {
    return Err(());
  }
  if matches!(&action.value, Value::Literal(body)
    if body.is_empty() || body == b"-" || body.iter().all(u8::is_ascii_digit))
  {
    return Ok(None);
  }
  Ok(Some(index))
}

pub(in crate::interface::application::engine) fn invariant_arguments(
  snapshot: Snapshot,
  node: Node<'_>,
  source: &[u8],
) -> Vec<ArgumentRole> {
  let Some(command) = parser::command_word(node) else {
    return Vec::new();
  };
  let Value::Literal(name) = word::literal_value(command, source) else {
    return Vec::new();
  };
  if (!numeric_argument(&name)
    && !matches!(
      name.as_slice(),
      b"exec" | b"export" | b"readonly" | b"unset"
    ))
    || node.has_error()
  {
    return Vec::new();
  }
  let mut args = syntax_arguments(node, source);
  classify_arguments(&name, &mut args, None, source);
  argument_roles(args, snapshot, source)
}

pub(super) fn argument_roles(
  args: Vec<Argument<'_>>,
  snapshot: Snapshot,
  source: &[u8],
) -> Vec<ArgumentRole> {
  args
    .into_iter()
    .filter(|arg| arg.kind.is_some())
    .map(|arg| arg.role(arg.kind.clone(), snapshot, source, None))
    .collect()
}

/// The index of the first operand of a utility whose only option is a
/// leading `--` (XCU 1.4).
pub(in crate::interface::application::engine::analysis) fn operand_index(
  args: &[Argument<'_>],
) -> usize {
  usize::from(args.first().is_some_and(|arg| {
    arg.count.boundary().is_ok() && arg.value == Value::Literal(b"--".to_vec())
  }))
}

pub(in crate::interface::application::engine::analysis) fn syntax_arguments<
  'a,
>(
  node: Node<'a>,
  source: &[u8],
) -> Vec<Argument<'a>> {
  node
    .child_by_field_name("suffix")
    .into_iter()
    .flat_map(super::super::execution::children)
    .filter(|node| node.kind() == "word")
    .map(|node| syntax_argument(node, source))
    .collect()
}

pub(super) fn syntax_argument<'a>(
  node: Node<'a>,
  source: &[u8],
) -> Argument<'a> {
  let (field, boundary_known) = word::syntax_field(node, source);
  let value = match &field {
    Some(field) if boundary_known => field.value(),
    _ => word::literal_value(node, source),
  };
  Argument {
    node,
    count: if boundary_known {
      word::FieldCount::One
    } else {
      word::FieldCount::Unknown {
        minimum: usize::from(word::has_guaranteed_field(node, source)),
        issue: value.issue().unwrap_or(ValueIssue::Dynamic),
      }
    },
    origins: field.map_or_else(Vec::new, |field| field.origins),
    value,
    kind: None,
  }
}

pub(super) fn classify_arguments(
  name: &[u8],
  args: &mut [Argument<'_>],
  env: Option<&Environment>,
  source: &[u8],
) {
  match name {
    b"trap" => {
      if let Ok(Some(index)) = trap_action(args) {
        args[index].kind = Some(ArgumentKind::ShellBody);
      }
    }
    name if numeric_argument(name) => {
      if decimal_operand(args).is_some() {
        args[0].kind = Some(ArgumentKind::Number);
      }
    }
    b"exec" => {
      let first = operand_index(args);
      if let Some(arg) = args.get_mut(first)
        && arg.count.boundary().is_ok()
        && matches!(&arg.value, Value::Literal(name) if !name.is_empty() && (first > 0 || !name.starts_with(b"-")))
      {
        arg.kind = Some(ArgumentKind::Command {
          name: arg.value.clone(),
        });
      }
    }
    b"export" | b"readonly" => {
      if let Some(start) = declaration_operands(args) {
        declaration_roles(name, &mut args[start..], env, source);
      }
    }
    b"unset" => {
      if let Some((index, functions, _)) = unset_options(args) {
        for arg in &mut args[index..] {
          if let Value::Literal(name) = &arg.value
            && arg.count.boundary().is_ok()
            && valid_name(name)
          {
            arg.kind = Some(if functions {
              ArgumentKind::Function { name: name.clone() }
            } else {
              variable_role(name, Access::NameReference, false, env)
            });
          }
        }
      }
    }
    b"read" => {
      let Some(options) = read_options(args) else {
        return;
      };
      for arg in &mut args[options.operands..] {
        if let Value::Literal(name) = &arg.value
          && arg.count.boundary().is_ok()
          && valid_name(name)
        {
          arg.kind = Some(variable_role(name, Access::Write, false, env));
        }
      }
    }
    b"getopts" => {
      let Some(start) = getopts_operands(args) else {
        return;
      };
      let args = &mut args[start..];
      let known_prefix =
        args.iter().take(2).all(|arg| arg.count.boundary().is_ok());
      if let Some(arg) = args.get_mut(1)
        && known_prefix
        && let Value::Literal(name) = &arg.value
        && valid_name(name)
      {
        arg.kind = Some(variable_role(name, Access::Write, false, env));
      }
    }
    _ => {}
  }
}

pub(super) fn getopts_operands(args: &[Argument<'_>]) -> Option<usize> {
  match args.first() {
    Some(first) if first.count.boundary().is_err() => None,
    Some(first) if first.value == Value::Literal(b"--".to_vec()) => Some(1),
    Some(first) if first.value.can_equal(b"--") => None,
    _ => Some(0),
  }
}

fn declaration_roles(
  utility: &[u8],
  args: &mut [Argument<'_>],
  env: Option<&Environment>,
  source: &[u8],
) {
  for arg in args {
    let declaration =
      if let Some(variable) = arg.node.child_by_field_name("name") {
        Some((word::lexical(variable, source), true))
      } else if arg.count.boundary().is_ok() {
        declaration_operand(&arg.value)
          .map(|(name, value)| (name, value.is_some()))
      } else {
        None
      };
    if let Some((variable, assigned)) = declaration
      && valid_name(&variable)
    {
      let mut kind = variable_role(
        &variable,
        if assigned {
          Access::Write
        } else {
          Access::NameReference
        },
        true,
        env,
      );
      if utility == b"readonly"
        && let ArgumentKind::Variable { readonly, .. } = &mut kind
      {
        *readonly = AttributeState::Known(true);
      }
      arg.kind = Some(kind);
    }
  }
}

pub(super) fn declaration_operand(
  value: &Value,
) -> Option<(Vec<u8>, Option<Value>)> {
  match value {
    Value::Literal(bytes) => {
      match bytes.iter().position(|byte| *byte == b'=') {
        Some(index) => Some((
          bytes[..index].to_vec(),
          Some(Value::Literal(bytes[index + 1..].to_vec())),
        )),
        None => Some((bytes.clone(), None)),
      }
    }
    Value::Partial(_) => {
      let prefix = value.known_prefix();
      let index = prefix.iter().position(|byte| *byte == b'=')?;
      Some((
        prefix[..index].to_vec(),
        Some(value.remove_prefix(index + 1)),
      ))
    }
    Value::Unknown(_) => None,
  }
}

pub(super) fn unset_options(
  args: &[Argument<'_>],
) -> Option<(usize, bool, bool)> {
  let mut namespace = None;
  let mut index = 0;
  while let Some(arg) = args.get(index) {
    match &arg.value {
      Value::Literal(value) if value == b"-f" || value == b"-v" => {
        // POSIX does not specify `-f` together with `-v`.
        if namespace
          .replace(value[1])
          .is_some_and(|other| other != value[1])
        {
          return None;
        }
        index += 1;
      }
      Value::Literal(value) if value == b"--" => {
        index += 1;
        break;
      }
      Value::Literal(value) if !value.starts_with(b"-") => break,
      Value::Unknown(_) | Value::Partial(_)
        if arg.count.boundary().is_ok() && !arg.value.may_start_with(b"-") =>
      {
        break;
      }
      _ => return None,
    }
  }
  Some((index, namespace == Some(b'f'), namespace.is_some()))
}

/// Rejects unknown options and `-p` followed by operands.
pub(super) fn declaration_operands(args: &[Argument<'_>]) -> Option<usize> {
  let mut print = false;
  let mut index = 0;
  while let Some(arg) = args.get(index) {
    if arg.node.child_by_field_name("name").is_some() {
      break;
    }
    if arg.count.boundary().is_err() {
      return None;
    }
    match &arg.value {
      Value::Literal(value) if value == b"--" => {
        index += 1;
        break;
      }
      Value::Literal(value) if value.len() > 1 && value.starts_with(b"-") => {
        if value[1..].iter().any(|letter| *letter != b'p') {
          return None;
        }
        print = true;
        index += 1;
      }
      Value::Literal(_) => break,
      value if value.may_start_with(b"-") => return None,
      _ => break,
    }
  }
  (!print || index == args.len()).then_some(index)
}

pub(super) struct ReadOptions {
  pub operands: usize,
  pub raw: bool,
  /// Whether `-d` replaces the newline delimiter.
  pub delimiter: bool,
}

/// Returns `None` if options leave operand positions uncertain.
pub(super) fn read_options(args: &[Argument<'_>]) -> Option<ReadOptions> {
  let mut options = ReadOptions {
    operands: 0,
    raw: false,
    delimiter: false,
  };
  while let Some(arg) = args.get(options.operands) {
    if arg.count.boundary().is_err() {
      return None;
    }
    let Value::Literal(value) = &arg.value else {
      return (!arg.value.may_start_with(b"-")).then_some(options);
    };
    if value == b"--" {
      options.operands += 1;
      break;
    }
    if !value.starts_with(b"-") || value == b"-" {
      break;
    }
    options.operands += 1;
    for (index, letter) in value.iter().enumerate().skip(1) {
      match letter {
        b'r' => options.raw = true,
        b'd' => {
          options.delimiter = true;
          if index + 1 == value.len() {
            if args.get(options.operands)?.count.boundary().is_err() {
              return None;
            }
            options.operands += 1;
          }
          break;
        }
        _ => return None,
      }
    }
  }
  Some(options)
}

fn decimal_operand<'a>(args: &'a [Argument<'_>]) -> Option<&'a [u8]> {
  let [arg] = args else {
    return None;
  };
  let Value::Literal(bytes) = &arg.value else {
    return None;
  };
  if arg.count.boundary().is_err()
    || bytes.is_empty()
    || !bytes.iter().all(u8::is_ascii_digit)
  {
    return None;
  }
  Some(bytes)
}

pub(super) fn decimal_value<T: std::str::FromStr>(
  args: &[Argument<'_>],
) -> Option<T> {
  std::str::from_utf8(decimal_operand(args)?)
    .ok()?
    .parse()
    .ok()
}

fn variable_role(
  name: &[u8],
  access: Access,
  declaration: bool,
  env: Option<&Environment>,
) -> ArgumentKind {
  let readonly =
    env.map_or(AttributeState::Unknown, |env| env.readonly_attribute(name));
  ArgumentKind::Variable {
    name: name.to_vec(),
    access,
    declaration,
    readonly,
  }
}

impl Resolver<'_> {
  pub(super) fn record_argument_use(
    &mut self,
    argument: &Argument<'_>,
    resolution: Resolution,
    env: &Environment,
    frame: Frame,
  ) {
    let variable = match &argument.kind {
      Some(ArgumentKind::Variable { name, .. }) => Some(name.as_slice()),
      _ => None,
    };
    self.record_use(
      UseSite::argument(&frame.location(argument.node)),
      frame.context,
      resolution,
      variable,
      env,
    );
  }

  pub(super) fn argument_role(
    &mut self,
    argument: &Argument<'_>,
    kind: Option<ArgumentKind>,
    frame: Frame,
  ) {
    let role = argument.role(
      kind,
      frame.snapshot,
      self.workspace.source(frame.snapshot.document),
      Some(frame.context),
    );
    self
      .result
      .argument_roles
      .entry((UseSite::argument(&role.location), frame.context))
      .and_modify(|old| old.join(&role))
      .or_insert(role);
  }
}

impl Argument<'_> {
  fn role(
    &self,
    kind: Option<ArgumentKind>,
    snapshot: Snapshot,
    source: &[u8],
    context: Option<ExecutionContextId>,
  ) -> ArgumentRole {
    let spelled = self.count.boundary().is_ok()
      && matches!(word::literal_value(self.node, source), Value::Literal(_));
    let location = Location {
      snapshot,
      bytes: self.node.byte_range(),
    };
    let end = match &kind {
      Some(ArgumentKind::Variable { name, .. }) => name.len(),
      _ => usize::MAX,
    };
    let spelling = if matches!(&kind, Some(ArgumentKind::Variable { .. }))
      && let Some(name) = self.node.child_by_field_name("name")
    {
      vec![Location {
        snapshot,
        bytes: name.byte_range(),
      }]
    } else if spelled {
      self
        .origins
        .iter()
        .filter_map(|part| {
          let end = part.bytes.end.min(end);
          (part.bytes.start < end)
            .then(|| part.project(part.bytes.start..end))
            .flatten()
        })
        .flatten()
        .map(|bytes| Location { snapshot, bytes })
        .collect()
    } else {
      Vec::new()
    };
    ArgumentRole {
      context,
      location,
      spelling,
      kind,
    }
  }
}
