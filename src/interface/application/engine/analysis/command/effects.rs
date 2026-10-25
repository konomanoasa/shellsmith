use super::super::execution::children;
use super::super::execution::{Environment, Flow};
use super::super::expansion::Argument;
use super::super::{Frame, Resolver};
use super::arguments::{
  argument_roles, classify_arguments, declaration_operands, getopts_operands,
  read_options, syntax_arguments, syntax_command, trap_action, unset_options,
};
use super::dispatch::{
  Search, builtin_candidates, builtin_names, command_options, only_builtin,
  special_name, unknown_utility,
};
use crate::interface::application::engine::workspace::parser;
use crate::interface::application::engine::{
  Access, Alternative, ArgumentKind, ArgumentRole, EffectTarget,
  OccurrenceKind, OriginPart, ResolutionIssue, Snapshot, UnknownEffect, Value,
  ValueIssue,
};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

fn all() -> BTreeSet<EffectTarget> {
  [
    EffectTarget::Variables,
    EffectTarget::Functions,
    EffectTarget::Attributes,
    EffectTarget::Sources,
    EffectTarget::Directory,
    EffectTarget::Positionals,
    EffectTarget::Options,
    EffectTarget::SpecialParameters,
  ]
  .into()
}

impl<'a> Resolver<'a> {
  pub(in crate::interface::application::engine::analysis) fn expansion_targets(
    &self,
    nodes: impl IntoIterator<Item = Node<'a>>,
    frame: Frame,
  ) -> BTreeSet<EffectTarget> {
    let mut targets = BTreeSet::new();
    for node in self.expansion_nodes(nodes, frame) {
      self.node_writes(node, frame, &mut targets);
    }
    targets
  }

  /// Excludes writes by nested commands.
  fn node_writes(
    &self,
    node: Node<'_>,
    frame: Frame,
    targets: &mut BTreeSet<EffectTarget>,
  ) {
    if node.kind() == "arithmetic_expansion" && parser::dynamic_arithmetic(node)
    {
      targets.insert(EffectTarget::Variables);
    }
    if let Some(site) = self
      .site(node, frame)
      .and_then(|id| self.workspace.occurrence(id))
      && site.kind != OccurrenceKind::Command
      && site.access != Access::Read
    {
      targets.insert(match (&site.kind, &site.name) {
        (OccurrenceKind::FunctionDefinition, Value::Literal(name)) => {
          EffectTarget::Function(name.clone())
        }
        (OccurrenceKind::FunctionDefinition, _) => EffectTarget::Functions,
        (_, Value::Literal(name)) => EffectTarget::Variable(name.clone()),
        _ => EffectTarget::Variables,
      });
    }
  }
}

impl Resolver<'_> {
  pub(super) fn potential_effects(
    &self,
    body: Node<'_>,
    snapshot: Snapshot,
    env: &Environment,
    frame: Frame,
  ) -> BTreeSet<EffectTarget> {
    self
      .effect_targets(body, snapshot, env, frame)
      .unwrap_or_else(all)
  }

  fn effect_targets<'a>(
    &'a self,
    body: Node<'a>,
    snapshot: Snapshot,
    env: &Environment,
    frame: Frame,
  ) -> Option<BTreeSet<EffectTarget>> {
    let mut targets = BTreeSet::new();
    let mut pending = vec![(body, snapshot)];
    let mut visited = BTreeSet::new();
    while let Some((node, snapshot)) = pending.pop() {
      if !visited.insert((snapshot, node.id())) {
        continue;
      }
      let frame = Frame { snapshot, ..frame };
      if parser::damaged(node) {
        return None;
      }
      if parser::subshell(node.kind()) {
        continue;
      }
      if parser::asynchronous(node) {
        targets.insert(EffectTarget::SpecialParameters);
        continue;
      }
      if node.kind() == "io_here"
        && let Some(body) = self.index.heredoc(snapshot, node)
      {
        pending.push((body, snapshot));
      }
      self.node_writes(node, frame, &mut targets);
      if node.kind() == "simple_command" {
        self.command_targets(
          node,
          snapshot,
          env,
          &mut pending,
          &mut targets,
        )?;
        targets.insert(EffectTarget::SpecialParameters);
      }
      pending.extend(children(node).into_iter().map(|node| (node, snapshot)));
    }
    Some(targets)
  }

  fn command_targets<'a>(
    &'a self,
    node: Node<'a>,
    snapshot: Snapshot,
    env: &Environment,
    pending: &mut Vec<(Node<'a>, Snapshot)>,
    targets: &mut BTreeSet<EffectTarget>,
  ) -> Option<()> {
    let source = self.workspace.source(snapshot.document);
    let Some(command) = parser::command_word(node) else {
      return Some(());
    };
    let (mut name, mut args) = syntax_command(node, command, source);
    let mut suppress = false;
    while name == Value::Literal(b"command".to_vec()) {
      let resolution = Self::command_resolution(&name, env, suppress);
      if !only_builtin(&resolution) {
        break;
      }
      let options = command_options(&args).ok()?;
      if options.query {
        return Some(());
      }
      args.drain(..options.operands);
      if args.is_empty() {
        return Some(());
      }
      name = args.remove(0).value;
      suppress = true;
    }
    let search = if suppress {
      Search::SuppressFunctions
    } else {
      Self::name_search(command, source, &name, env)
    };
    let resolution = Self::search_resolution(&name, env, search);
    if unknown_utility(&resolution) {
      return None;
    }
    for candidate in resolution.candidates {
      let body = self.index.function_body(candidate)?;
      pending.push((body, candidate.snapshot));
    }
    if resolution.alternatives.contains(&Alternative::Builtin) {
      for candidate in builtin_names(&name, env, suppress) {
        builtin_targets(&candidate, &args, source, targets)?;
      }
    }
    Some(())
  }
}

impl Resolver<'_> {
  pub(in crate::interface::application::engine::analysis) fn invalidate(
    &mut self,
    node: Node<'_>,
    env: &mut Environment,
    frame: Frame,
    issue: ResolutionIssue,
    value_issue: ValueIssue,
    targets: impl IntoIterator<Item = EffectTarget>,
  ) {
    let targets: BTreeSet<_> = targets.into_iter().collect();
    if targets.is_empty() {
      return;
    }
    self.gap(node, frame, issue);
    self
      .result
      .effects
      .entry(frame.effect_key(node, issue))
      .or_insert_with(|| UnknownEffect {
        context: frame.context,
        location: frame.location(node),
        issue,
        targets: BTreeSet::new(),
      })
      .targets
      .extend(targets.iter().cloned());
    for target in targets {
      env.invalidate(
        target,
        self.result.binding_context(frame.context),
        issue,
        value_issue,
      );
    }
  }

  pub(in crate::interface::application::engine::analysis) fn opaque(
    &mut self,
    node: Node<'_>,
    mut env: Environment,
    frame: Frame,
    issue: ResolutionIssue,
    value_issue: Option<ValueIssue>,
  ) -> Flow {
    let value_issue = value_issue
      .or((issue == ResolutionIssue::Recovery).then_some(ValueIssue::Recovery));
    env.io.consume_input(None);
    if let Some(output) = &mut env.io.output {
      output.fail(value_issue.unwrap_or(ValueIssue::UnsupportedExpansion));
    }
    self.invalidate(
      node,
      &mut env,
      frame,
      issue,
      value_issue.unwrap_or(ValueIssue::Dynamic),
      all(),
    );
    Flow::next(env)
  }
}

#[derive(Clone, Debug, Default)]
pub(in crate::interface::application::engine) struct UnattemptedCommand {
  pub arguments: Vec<ArgumentRole>,
  pub dependencies: BTreeSet<EffectTarget>,
  pub targets: BTreeSet<EffectTarget>,
  pub bodies: BTreeMap<(usize, usize), (Value, Vec<OriginPart>)>,
  pub incomplete_body: bool,
}

impl UnattemptedCommand {
  /// A command that `command` runs but syntax cannot identify.
  fn unknown_command(&mut self) {
    self
      .dependencies
      .extend([EffectTarget::Variables, EffectTarget::Functions]);
    self.targets.extend([
      EffectTarget::Variables,
      EffectTarget::Functions,
      EffectTarget::Sources,
    ]);
  }
}

pub(in crate::interface::application::engine) fn unattempted_command(
  snapshot: Snapshot,
  node: Node<'_>,
  source: &[u8],
) -> UnattemptedCommand {
  let (name, mut args) = match parser::command_word(node) {
    Some(command) => {
      let (name, args) = syntax_command(node, command, source);
      (Some(name), args)
    }
    None => (None, syntax_arguments(node, source)),
  };
  let mut result = UnattemptedCommand::default();
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
  if !name.may_be_name() || special_name(name) {
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
    for candidate in builtin_candidates(name) {
      if candidate == b"command" {
        let Ok(options) = command_options(args) else {
          result.unknown_command();
          continue;
        };
        let operands = &args[options.operands..];
        if options.query {
          for arg in operands {
            if arg.count.boundary().is_ok() {
              result.dependencies.extend(command_dependency(&arg.value));
            } else {
              result.dependencies.insert(EffectTarget::Functions);
            }
          }
        } else if let Some((command, rest)) = operands.split_first() {
          result
            .dependencies
            .extend(command_dependency(&command.value));
          if command.count.boundary().is_ok() {
            pending.push((&command.value, rest));
          } else {
            result.unknown_command();
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
        if builtin_targets(candidate, args, source, &mut targets).is_none() {
          targets.extend([
            EffectTarget::Variables,
            EffectTarget::Functions,
            EffectTarget::Sources,
          ]);
        }
        result.dependencies.extend(targets.iter().cloned());
        result.targets.extend(targets);
      }
    }
  }
}

fn builtin_targets(
  name: &[u8],
  args: &[Argument<'_>],
  source: &[u8],
  targets: &mut BTreeSet<EffectTarget>,
) -> Option<()> {
  let mut args = args.to_vec();
  classify_arguments(name, &mut args, None, source);
  let args = args.as_slice();
  match name {
    b"." | b"fc" | b"newgrp" | b"command" => return None,
    b"read" => {
      if let Some(options) = read_options(args) {
        named_targets(
          &args[options.operands..],
          NameTarget::Variable,
          false,
          targets,
        );
      } else {
        targets.insert(EffectTarget::Variables);
      }
    }
    b"getopts" => {
      match getopts_operands(args).map(|start| &args[start..]) {
        Some(args)
          if args.iter().take(2).all(|arg| arg.count.boundary().is_ok()) =>
        {
          named_targets(
            args.get(1..2).unwrap_or(&[]),
            NameTarget::Variable,
            false,
            targets,
          );
        }
        _ => {
          targets.insert(EffectTarget::Variables);
        }
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
      if let Some(start) = declaration_operands(args) {
        named_targets(
          &args[start..],
          NameTarget::Declaration,
          name == b"readonly",
          targets,
        );
      } else {
        targets.insert(EffectTarget::Variables);
        if name == b"readonly" {
          targets.insert(EffectTarget::Attributes);
        }
      }
    }
    b"cd" => {
      targets.insert(EffectTarget::Directory);
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
      _ if arg.count.boundary().is_err() || arg.value.issue().is_some() => {
        let target = argument_target(arg, kind);
        if attributes {
          targets.insert(attribute_target(&target));
        }
        targets.insert(target);
      }
      _ => {}
    }
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
  match (value, arg.count.boundary().is_ok(), kind) {
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

pub(super) fn attribute_target(target: &EffectTarget) -> EffectTarget {
  match target {
    EffectTarget::Variable(name) => EffectTarget::Attribute(name.clone()),
    EffectTarget::VariablePattern(pattern) => {
      EffectTarget::AttributePattern(pattern.clone())
    }
    _ => EffectTarget::Attributes,
  }
}
