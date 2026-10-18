use super::super::execution::children;
use super::super::execution::{Environment, Flow};
use super::super::{Frame, Resolver};
use super::dispatch::{ForwardOption, builtin_names, forward_option};
use crate::interface::application::engine::{
  Access, Alternative, EffectTarget, OccurrenceKind, ResolutionIssue, Snapshot,
  Value, ValueIssue,
};
use std::collections::BTreeSet;
use tree_sitter::Node;

fn all() -> BTreeSet<EffectTarget> {
  [
    EffectTarget::Variables,
    EffectTarget::Functions,
    EffectTarget::Attributes,
    EffectTarget::Directory,
    EffectTarget::Positionals,
    EffectTarget::Options,
    EffectTarget::SpecialParameters,
  ]
  .into()
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
      if crate::interface::application::engine::workspace::parser::damaged(node)
      {
        return None;
      }
      if matches!(
        node.kind(),
        "subshell" | "command_substitution" | "backquote_substitution"
      ) {
        continue;
      }
      if node.kind() == "io_here"
        && let Some(body) =
          self.index.heredocs.get(&(snapshot, node.start_byte()))
      {
        pending.push((*body, snapshot));
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
          (_, Value::Literal(name)) => EffectTarget::Variable(name.clone()),
          _ => EffectTarget::Variables,
        });
      }
      if matches!(
        node.kind(),
        "arithmetic_dynamic_expression"
          | "parenthesized_arithmetic_dynamic_source"
      ) {
        targets.insert(EffectTarget::Variables);
      }
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
    let document = self
      .workspace
      .document(snapshot.document)
      .expect("summary document");
    let Some(command) = node
      .child_by_field_name("name")
      .or_else(|| node.child_by_field_name("word"))
    else {
      return Some(());
    };
    let mut name =
      super::arguments::syntax_argument(command, document.source.as_bytes())
        .value;
    let mut args =
      super::arguments::syntax_arguments(node, document.source.as_bytes());
    let mut suppress = false;
    while name == Value::Literal(b"command".to_vec()) {
      let resolution = Self::command_resolution(&name, env, suppress);
      if resolution.alternatives != [Alternative::Builtin].into()
        || !resolution.candidates.is_empty()
        || !resolution.issues.is_empty()
      {
        break;
      }
      while let Some(arg) = args.first() {
        match forward_option(arg) {
          ForwardOption::Query => return Some(()),
          ForwardOption::Search => {
            args.remove(0);
          }
          ForwardOption::End => {
            args.remove(0);
            break;
          }
          ForwardOption::Name => break,
          ForwardOption::Invalid | ForwardOption::Unknown => return None,
        }
      }
      if args.is_empty() {
        return Some(());
      }
      name = args.remove(0).value;
      suppress = true;
    }
    let resolution = Self::command_resolution(&name, env, suppress);
    if !resolution.issues.is_empty()
      || resolution.alternatives.contains(&Alternative::External)
    {
      return None;
    }
    for candidate in resolution.candidates {
      let body = self.index.functions.get(&candidate)?;
      pending.push((*body, candidate.snapshot));
    }
    if resolution.alternatives.contains(&Alternative::Builtin) {
      for candidate in builtin_names(&name, env, suppress) {
        super::arguments::builtin_targets(
          &candidate,
          &mut args,
          document.source.as_bytes(),
          targets,
        )?;
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
    targets: impl IntoIterator<
      Item = crate::interface::application::engine::EffectTarget,
    >,
  ) {
    use crate::interface::application::engine::{EffectTarget, UnknownEffect};
    let targets: BTreeSet<_> = targets.into_iter().collect();
    if targets.is_empty() {
      return;
    }
    self.gap(node, frame, issue);
    self
      .result
      .effects
      .entry((
        frame.context,
        frame.snapshot,
        node.start_byte(),
        node.end_byte(),
        issue,
      ))
      .or_insert_with(|| UnknownEffect {
        context: frame.context,
        location: frame.location(node),
        issue,
        targets: BTreeSet::new(),
      })
      .targets
      .extend(targets.iter().cloned());
    for target in targets {
      let functions = matches!(target, EffectTarget::FunctionPattern(_));
      match target {
        EffectTarget::VariablePattern(pattern)
        | EffectTarget::FunctionPattern(pattern) => {
          env.invalidate_pattern(pattern, functions, issue);
        }
        EffectTarget::Function(name) => {
          env.alternatives.forget_function(&name);
          let mut binding = env.functions.get(&name).clone();
          binding.taint(issue);
          env.functions.values.insert(name, binding);
        }
        EffectTarget::Attribute(name) => {
          if !env.readonly.contains_key(&name) {
            env.readonly.insert(name, false);
          }
        }
        EffectTarget::Variable(name) => {
          env.alternatives.forget(&name);
          if env.readonly(&name) == Some(true) {
            continue;
          }
          env.variable_changed(&name, None);
          let mut binding = env.variables.get(&name).clone();
          binding.taint(issue);
          env.variables.values.insert(name, binding);
        }
        EffectTarget::Variables => {
          env.variable_changed(b"OPTIND", None);
          env.alternatives.clear();
          for (name, mut binding) in env.variables.values.clone() {
            if env.readonly.get(&name) != Some(&true) {
              binding.taint(issue);
              env.variables.values.insert(name, binding);
            }
          }
          env.variables.fallback.taint(issue);
          env.variables.patterns.clear();
          env.variables.uncertain = None;
        }
        EffectTarget::Functions => {
          env.functions.taint(issue);
          env.alternatives.forget_functions();
        }
        EffectTarget::Attributes => {
          env.unknown_attributes = true;
        }
        EffectTarget::Directory => {
          env.cwd = Value::Unknown(ValueIssue::Dynamic);
        }
        EffectTarget::Positionals => {
          env.positional = None;
          env.alternatives.forget_positionals();
        }
        EffectTarget::Options => {
          env.errexit = None;
          env.nounset = None;
          env.noglob = None;
          env.special.remove(b"-".as_slice());
        }
        EffectTarget::SpecialParameters => env
          .special
          .retain(|name, _| matches!(name.as_slice(), b"0" | b"$")),
      }
    }
  }

  pub(in crate::interface::application::engine::analysis) fn unknown(
    &mut self,
    node: Node<'_>,
    mut env: Environment,
    frame: Frame,
    issue: ResolutionIssue,
  ) -> Flow {
    use crate::interface::application::engine::EffectTarget;
    env.io.consume_input(None);
    if let Some(output) = &mut env.io.output {
      output.fail(ValueIssue::UnsupportedExpansion);
    }
    self.invalidate(
      node,
      &mut env,
      frame,
      issue,
      [
        EffectTarget::Variables,
        EffectTarget::Functions,
        EffectTarget::Directory,
        EffectTarget::Positionals,
        EffectTarget::Options,
        EffectTarget::SpecialParameters,
      ],
    );
    Flow::next(env)
  }
}

impl Resolver<'_> {
  pub(in crate::interface::application::engine::analysis) fn opaque(
    &mut self,
    node: Node<'_>,
    mut env: Environment,
    frame: Frame,
    issue: ResolutionIssue,
  ) -> Flow {
    use crate::interface::application::engine::EffectTarget;
    self.invalidate(node, &mut env, frame, issue, [EffectTarget::Attributes]);
    self.unknown(node, env, frame, issue)
  }
}
