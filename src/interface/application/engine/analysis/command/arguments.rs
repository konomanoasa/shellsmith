use super::super::execution::Environment;
use super::super::expansion::Argument;
use super::super::{Frame, Resolver};
use crate::interface::application::engine::word::{self, WordContext};
use crate::interface::application::engine::{
  Access, ArgumentKind, ArgumentRole, AttributeState, EffectTarget,
  ExecutionContextId, Location, Resolution, ResolutionIssue, Snapshot, UseSite,
  Value, ValueIssue, valid_name,
};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

pub(super) fn uses_arguments(name: &[u8]) -> bool {
  numeric_argument(name)
    || matches!(
      name,
      b"export"
        | b"readonly"
        | b"unset"
        | b"set"
        | b"cd"
        | b"command"
        | b"."
        | b"read"
        | b"getopts"
        | b"trap"
    )
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
    if !first.boundary_known {
      return Err(());
    }
    if first.value == Value::Literal(b"--".to_vec()) {
      index = 1;
    } else if matches!(&first.value, Value::Literal(value) if value.starts_with(b"-") && value != b"-")
    {
      return Ok(None);
    }
  }
  let Some(action) = args.get(index) else {
    return Ok(None);
  };
  if !action.boundary_known {
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
  snapshot: crate::interface::application::engine::Snapshot,
  node: Node<'_>,
  source: &[u8],
) -> Vec<ArgumentRole> {
  let Some(command) = node
    .child_by_field_name("name")
    .or_else(|| node.child_by_field_name("word"))
  else {
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
  argument_candidates(snapshot, node, source)
}

fn argument_candidates(
  snapshot: crate::interface::application::engine::Snapshot,
  node: Node<'_>,
  source: &[u8],
) -> Vec<ArgumentRole> {
  let Some(command) = node
    .child_by_field_name("name")
    .or_else(|| node.child_by_field_name("word"))
  else {
    return Vec::new();
  };
  let Value::Literal(name) = word::literal_value(command, source) else {
    return Vec::new();
  };
  let mut args = syntax_arguments(node, source);
  classify_arguments(&name, &mut args, None, source);
  argument_roles(args, snapshot, source)
}

fn argument_roles(
  args: Vec<Argument<'_>>,
  snapshot: crate::interface::application::engine::Snapshot,
  source: &[u8],
) -> Vec<ArgumentRole> {
  args
    .into_iter()
    .filter(|arg| arg.kind.is_some())
    .map(|arg| arg.role(arg.kind.clone(), snapshot, source, None))
    .collect()
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
  let value = word::literal_value(node, source);
  let mut partial = word::Word::default();
  for part in word::Parts::new(node, source) {
    match part {
      word::Part::Fragment(fragment) => {
        partial.append(fragment, usize::MAX);
      }
      word::Part::Invalid(issue) => partial.fail(issue),
      word::Part::Expansion(expansion, quoted) => {
        let scalar = quoted
          && (expansion.kind() != "parameter_expansion"
            || (expansion.child_by_field_name("operator").is_none()
              && expansion.child_by_field_name("parameter").is_none_or(
                |parameter| word::lexical(parameter, source) != b"@",
              )));
        partial.extend(
          word::Word::value(Value::Unknown(ValueIssue::Dynamic), quoted),
          usize::MAX,
        );
        partial.unknown_fields |= !scalar;
      }
    }
  }
  let fields = partial
    .fields(
      WordContext::Argument,
      &Value::Unknown(ValueIssue::Dynamic),
      false,
    )
    .ok();
  let field =
    fields.and_then(|mut fields| (fields.len() == 1).then(|| fields.remove(0)));
  let boundary_known = !partial.unknown_fields
    && field.as_ref().is_some_and(|field| {
      word::Pattern::compile(&field.bytes, &field.quoted, true, true)
        .is_ok_and(|pattern| pattern.is_literal())
    });
  let value = if boundary_known {
    field.as_ref().map_or(value.clone(), word::Field::value)
  } else {
    value
  };
  Argument {
    node,
    boundary_known,
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
      let first = usize::from(args.first().is_some_and(|arg| {
        arg.boundary_known && arg.value == Value::Literal(b"--".to_vec())
      }));
      if let Some(arg) = args.get_mut(first)
        && arg.boundary_known
        && matches!(&arg.value, Value::Literal(name) if !name.is_empty() && (first > 0 || !name.starts_with(b"-")))
      {
        arg.kind = Some(ArgumentKind::Command {
          name: arg.value.clone(),
        });
      }
    }
    b"export" | b"readonly" => {
      declaration_roles(name, args, env, source);
    }
    b"unset" => {
      if let Some((index, functions, _)) = unset_options(args) {
        for arg in &mut args[index..] {
          if let Value::Literal(name) = &arg.value
            && arg.boundary_known
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
      let index = read_options(args);
      for arg in &mut args[index..] {
        if let Value::Literal(name) = &arg.value
          && arg.boundary_known
          && valid_name(name)
        {
          arg.kind = Some(variable_role(name, Access::Write, false, env));
        }
      }
    }
    b"getopts" => {
      let known_prefix = args.iter().take(2).all(|arg| arg.boundary_known);
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

#[derive(Clone, Debug, Default)]
pub(in crate::interface::application::engine) struct UnattemptedCommand {
  pub arguments: Vec<ArgumentRole>,
  pub dependencies: BTreeSet<EffectTarget>,
  pub targets: BTreeSet<EffectTarget>,
  pub bodies: BTreeMap<
    (usize, usize),
    (
      Value,
      Vec<crate::interface::application::engine::OriginPart>,
    ),
  >,
  pub incomplete_body: bool,
}

pub(in crate::interface::application::engine) fn unattempted_command(
  snapshot: crate::interface::application::engine::Snapshot,
  node: Node<'_>,
  source: &[u8],
) -> UnattemptedCommand {
  let name = node
    .child_by_field_name("name")
    .or_else(|| node.child_by_field_name("word"))
    .map(|command| syntax_argument(command, source).value);
  let mut result = UnattemptedCommand::default();
  let mut args = syntax_arguments(node, source);
  if let Some(name) = &name {
    if let Some(dependency) = command_dependency(name) {
      result.dependencies.insert(dependency);
      result
        .targets
        .extend([EffectTarget::Variables, EffectTarget::Functions]);
    }
    unattempted_utilities(name, &args, source, &mut result);
    if let Value::Literal(name) = name {
      classify_arguments(name, &mut args, None, source);
    }
  }
  result.arguments = argument_roles(args, snapshot, source);
  result
}

fn command_dependency(name: &Value) -> Option<EffectTarget> {
  if !name.may_be_name()
    || matches!(name, Value::Literal(name) if super::dispatch::special_builtin(name))
  {
    return None;
  }
  Some(match name {
    Value::Literal(name) => EffectTarget::Function(name.clone()),
    Value::Partial(_) => EffectTarget::FunctionPattern(name.clone()),
    Value::Unknown(_) => EffectTarget::Functions,
  })
}

fn unattempted_utilities(
  name: &Value,
  args: &[Argument<'_>],
  source: &[u8],
  result: &mut UnattemptedCommand,
) {
  let mut pending = vec![(name, args)];
  while let Some((name, args)) = pending.pop() {
    for candidate in super::dispatch::builtin_candidates(name) {
      if candidate == b"command" {
        let Some((operands, query)) = forwarded_arguments(args) else {
          result
            .dependencies
            .extend([EffectTarget::Variables, EffectTarget::Functions]);
          result
            .targets
            .extend([EffectTarget::Variables, EffectTarget::Functions]);
          continue;
        };
        if query {
          for arg in operands {
            if arg.boundary_known {
              result.dependencies.extend(command_dependency(&arg.value));
            } else {
              result.dependencies.insert(EffectTarget::Functions);
            }
          }
        } else if let Some((command, rest)) = operands.split_first() {
          result
            .dependencies
            .extend(command_dependency(&command.value));
          if command.boundary_known {
            pending.push((&command.value, rest));
          } else {
            result
              .dependencies
              .extend([EffectTarget::Variables, EffectTarget::Functions]);
            result
              .targets
              .extend([EffectTarget::Variables, EffectTarget::Functions]);
          }
        }
      } else {
        if candidate == b"trap" {
          match trap_action(args) {
            Ok(Some(index)) => {
              let argument = &args[index];
              result.bodies.insert(
                (argument.node.start_byte(), argument.node.end_byte()),
                (argument.value.clone(), argument.origins.clone()),
              );
            }
            Err(()) => result.incomplete_body = true,
            Ok(None) => {}
          }
        }
        let mut targets = BTreeSet::new();
        if builtin_targets(candidate, &mut args.to_vec(), source, &mut targets)
          .is_none()
        {
          targets.extend([EffectTarget::Variables, EffectTarget::Functions]);
        }
        result.dependencies.extend(targets.iter().cloned());
        result.targets.extend(targets);
      }
    }
  }
}

fn forwarded_arguments<'a, 'tree>(
  args: &'a [Argument<'tree>],
) -> Option<(&'a [Argument<'tree>], bool)> {
  use super::dispatch::{ForwardOption, forward_option};
  let mut query = false;
  for (index, argument) in args.iter().enumerate() {
    match forward_option(argument) {
      ForwardOption::Name => return Some((&args[index..], query)),
      ForwardOption::End => return Some((&args[index + 1..], query)),
      ForwardOption::Search => {}
      ForwardOption::Query => query = true,
      ForwardOption::Invalid | ForwardOption::Unknown => return None,
    }
  }
  Some((&[], query))
}

pub(super) fn builtin_targets(
  name: &[u8],
  args: &mut [Argument<'_>],
  source: &[u8],
  targets: &mut BTreeSet<EffectTarget>,
) -> Option<()> {
  classify_arguments(name, args, None, source);
  match name {
    b"." | b"fc" | b"newgrp" | b"command" => return None,
    b"read" => {
      named_targets(
        &args[read_options(args)..],
        NameTarget::Variable,
        false,
        targets,
      );
    }
    b"getopts" => {
      if args.iter().take(2).all(|arg| arg.boundary_known) {
        named_targets(
          args.get(1..2).unwrap_or(&[]),
          NameTarget::Variable,
          false,
          targets,
        );
      } else {
        targets.insert(EffectTarget::Variables);
      }
      targets.extend([
        EffectTarget::Variable(b"OPTIND".to_vec()),
        EffectTarget::Variable(b"OPTARG".to_vec()),
      ]);
    }
    b"unset" => {
      if let Some((index, functions, explicit)) = unset_options(args) {
        named_targets(
          &args[index..],
          if functions {
            NameTarget::Function
          } else {
            NameTarget::Variable
          },
          false,
          targets,
        );
        if !explicit {
          named_targets(&args[index..], NameTarget::Function, false, targets);
        }
      } else {
        targets.extend([EffectTarget::Variables, EffectTarget::Functions]);
      }
    }
    b"readonly" | b"export" => {
      named_targets(
        args,
        NameTarget::Declaration,
        name == b"readonly",
        targets,
      );
    }
    b"cd" => {
      targets.extend([
        EffectTarget::Directory,
        EffectTarget::Variable(b"PWD".to_vec()),
        EffectTarget::Variable(b"OLDPWD".to_vec()),
      ]);
    }
    b"set" => {
      targets.extend([EffectTarget::Options, EffectTarget::Positionals]);
    }
    b"shift" => {
      targets.insert(EffectTarget::Positionals);
    }
    _ => {}
  }
  Some(())
}

fn named_targets(
  args: &[Argument<'_>],
  kind: NameTarget,
  attributes: bool,
  targets: &mut BTreeSet<EffectTarget>,
) {
  for arg in args {
    match &arg.kind {
      Some(
        ArgumentKind::Variable { name, .. } | ArgumentKind::Function { name },
      ) => {
        targets.insert(if matches!(kind, NameTarget::Function) {
          EffectTarget::Function(name.clone())
        } else {
          EffectTarget::Variable(name.clone())
        });
        if attributes {
          targets.insert(EffectTarget::Attribute(name.clone()));
        }
      }
      _ if !arg.boundary_known || arg.value.issue().is_some() => {
        targets.insert(argument_target(arg, kind));
        if attributes {
          targets.insert(EffectTarget::Attributes);
        }
      }
      _ => {}
    }
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
      } else if arg.boundary_known {
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
  let mut functions = false;
  let mut explicit = false;
  let mut index = 0;
  while let Some(arg) = args.get(index) {
    match &arg.value {
      Value::Literal(value) if value == b"-f" || value == b"-v" => {
        functions = value == b"-f";
        explicit = true;
        index += 1;
      }
      Value::Literal(value) if value == b"--" => {
        index += 1;
        break;
      }
      Value::Literal(value) if !value.starts_with(b"-") => break,
      Value::Unknown(_) | Value::Partial(_)
        if arg.boundary_known && !arg.value.may_start_with(b"-") =>
      {
        break;
      }
      _ => return None,
    }
  }
  Some((index, functions, explicit))
}

pub(super) fn read_options(args: &[Argument<'_>]) -> usize {
  let mut index = 0;
  while let Some(arg) = args.get(index) {
    match &arg.value {
      Value::Literal(value) if value == b"-r" => index += 1,
      Value::Literal(value) if value == b"--" => return index + 1,
      _ => break,
    }
  }
  index
}

pub(super) fn decimal_operand<'a>(
  args: &'a [Argument<'_>],
) -> Option<&'a [u8]> {
  let [arg] = args else {
    return None;
  };
  let Value::Literal(bytes) = &arg.value else {
    return None;
  };
  if !arg.boundary_known
    || bytes.is_empty()
    || !bytes.iter().all(u8::is_ascii_digit)
  {
    return None;
  }
  Some(bytes)
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

#[derive(Clone, Copy)]
pub(super) enum NameTarget {
  Variable,
  Function,
  Declaration,
}

pub(super) fn argument_target(
  arg: &Argument<'_>,
  kind: NameTarget,
) -> EffectTarget {
  let value = if matches!(kind, NameTarget::Declaration) {
    arg.value.before(b'=')
  } else {
    arg.value.clone()
  };
  match (value, arg.boundary_known, kind) {
    (Value::Literal(name), true, NameTarget::Function) => {
      EffectTarget::Function(name)
    }
    (Value::Literal(name), true, _) => EffectTarget::Variable(name),
    (value @ Value::Partial(_), true, NameTarget::Function) => {
      EffectTarget::FunctionPattern(value)
    }
    (value @ Value::Partial(_), true, _) => {
      EffectTarget::VariablePattern(value)
    }
    (_, _, NameTarget::Function) => EffectTarget::Functions,
    _ => EffectTarget::Variables,
  }
}

impl Resolver<'_> {
  pub(super) fn record_argument_use(
    &mut self,
    argument: &Argument<'_>,
    mut resolution: Resolution,
    env: &Environment,
    frame: Frame,
  ) {
    if let Some(ArgumentKind::Variable { name, .. }) = &argument.kind
      && env.uncertain_variable(name)
    {
      resolution.issues.insert(ResolutionIssue::EvaluationOrder);
    }
    self.record_use(
      UseSite::argument(&frame.location(argument.node)),
      frame.context,
      resolution,
    );
  }

  pub(super) fn argument_role(
    &mut self,
    argument: &Argument<'_>,
    kind: Option<ArgumentKind>,
    frame: Frame,
  ) {
    let doc = self
      .workspace
      .document(frame.snapshot.document)
      .expect("current document");
    let role = argument.role(
      kind,
      frame.snapshot,
      doc.source.as_bytes(),
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
  pub(super) fn role(
    &self,
    kind: Option<ArgumentKind>,
    snapshot: Snapshot,
    source: &[u8],
    context: Option<ExecutionContextId>,
  ) -> ArgumentRole {
    let spelled = self.boundary_known
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
