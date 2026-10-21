use super::execution::{Binding, Environment, Names};
use super::{Frame, Resolver, Task};
use crate::interface::application::engine::{
  self, Alternative, DerivedOrigin, Error, ExecutionContextId, ExecutionKind,
  InitialEnvironment, Location, OccurrenceId, Resolution, ResolutionIssue,
  Snapshot, Value, ValueIssue, check_cancelled,
};
use imbl::{OrdMap, ordmap::DiffItem};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

fn needs_environment(document: &engine::Document) -> bool {
  document.occurrences().iter().any(|site| {
    site.kind != engine::OccurrenceKind::Command
      || super::invariant_command(&site.name).is_none()
  }) || document.has_body_candidates()
    || !document.references().is_empty()
    || document.argument_roles().iter().any(|role| {
      !matches!(role.kind, None | Some(engine::ArgumentKind::Number))
    })
}

impl Resolver<'_> {
  pub(super) fn defer_body(
    &mut self,
    node: Node<'_>,
    command: OccurrenceId,
    action: Result<Option<(DerivedOrigin, engine::Value)>, ()>,
    env: &Environment,
    frame: Frame,
  ) -> Result<(), Error> {
    let Some(snapshot) = self.result.derived.body(
      self.workspace.workspace,
      command,
      action,
      self.result.request.limits.value_bytes,
      self.cancelled,
    )?
    else {
      return Ok(());
    };
    let registration = frame.location(node);
    if self.recursive_registration(snapshot, &registration, frame) {
      self
        .result
        .derived
        .bodies
        .get_mut(&command)
        .expect("body registration")
        .incomplete = true;
      self.gap(node, frame, ResolutionIssue::Recursion);
    } else if needs_environment(
      self
        .workspace
        .workspace
        .document(snapshot.document)
        .expect("body document"),
    ) {
      let environment = self.result.contexts[frame.context.0].environment;
      self
        .invariants
        .capture(snapshot, registration, frame, environment, env);
    }
    Ok(())
  }

  fn recursive_registration(
    &self,
    body: Snapshot,
    registration: &Location,
    frame: Frame,
  ) -> bool {
    let origin = self
      .workspace
      .workspace
      .derived_origin(body.document)
      .expect("body origin");
    let source = self
      .workspace
      .document(registration.snapshot.document)
      .expect("registration document");
    let text = source.text(registration);
    self.ancestors(frame).any(|context| {
      let ExecutionKind::Deferred(ancestor) = context.kind else {
        return false;
      };
      self
        .workspace
        .workspace
        .derived_origin(ancestor.document)
        .is_some_and(|previous| previous.argument == origin.argument)
        && self
          .workspace
          .document(context.location.snapshot.document)
          .and_then(|document| document.text(&context.location))
          == text
    })
  }

  pub(super) fn analyze_deferred(&mut self) -> Result<(), Error> {
    loop {
      let pending = std::mem::take(&mut self.invariants.pending);
      if pending.is_empty() {
        return Ok(());
      }
      let incomplete =
        self.result.derived.incomplete_bodies(Some(&self.result));
      for input in pending.into_values() {
        check_cancelled(self.cancelled)?;
        if incomplete.contains(&input.snapshot) {
          continue;
        }
        let env = self.invariants.input(&input);
        self.register_document(input.snapshot)?;
        let document = self
          .workspace
          .document(input.snapshot.document)
          .expect("body document");
        let source = self
          .workspace
          .document(input.registration.snapshot.document)
          .expect("registration document");
        let node = source
          .command_at(&input.registration)
          .expect("registration syntax");
        let frame = self.execution(
          node,
          input.frame,
          ExecutionKind::Deferred(input.snapshot),
          input.snapshot,
        );
        self.sources.start_root(input.snapshot);
        self.run(Task::eval(document.tree.root_node(), env, frame))?;
      }
    }
  }
}

struct CapturedState {
  variables: Names,
  functions: Names,
  readonly: OrdMap<Vec<u8>, bool>,
  unknown_attributes: BTreeSet<Value>,
  cwd: Value,
  noglob: Option<bool>,
  directory_variables_modified: bool,
}

impl From<&Environment> for CapturedState {
  fn from(env: &Environment) -> Self {
    Self {
      variables: env.variables.clone(),
      functions: env.functions.clone(),
      readonly: env.readonly.clone(),
      unknown_attributes: env.unknown_attributes.clone(),
      cwd: env.cwd.clone(),
      noglob: env.noglob,
      directory_variables_modified: env.directory_variables_modified,
    }
  }
}

#[derive(Default)]
struct NameChanges {
  values: BTreeMap<Vec<u8>, usize>,
  patterns: BTreeMap<Value, usize>,
  fallback: usize,
}

impl NameChanges {
  fn observe(&mut self, before: &mut Names, after: &Names, revision: usize) {
    let mut changed = false;
    for (name, left, right) in before.differences(after) {
      if left != right {
        self.values.insert(name.clone(), revision);
        changed = true;
      }
    }
    if before.fallback != after.fallback {
      self.fallback = revision;
      changed = true;
    }
    if before.patterns != after.patterns || before.uncertain != after.uncertain
    {
      self.patterns.extend(
        before
          .patterns
          .union(&after.patterns)
          .map(|pattern| (pattern.clone(), revision)),
      );
      changed = true;
    }
    if changed {
      before.clone_from(after);
    }
  }

  fn invalidate(&self, names: &mut Names, since: usize, unknown: &Binding) {
    for (name, revision) in &self.values {
      if *revision > since {
        names.values.insert(name.clone(), unknown.clone());
      }
    }
    if self.fallback > since {
      names.fallback = unknown.clone();
      names.patterns.clear();
      names.uncertain = None;
    } else {
      for (pattern, revision) in &self.patterns {
        if *revision > since {
          names.restrict(
            pattern.clone(),
            ResolutionIssue::UnknownEffect,
            pattern.issue().unwrap_or(ValueIssue::Dynamic),
          );
        }
      }
    }
  }
}

#[derive(Default)]
struct StateChanges {
  variables: NameChanges,
  functions: NameChanges,
  readonly: BTreeMap<Vec<u8>, usize>,
  attributes: usize,
  cwd: usize,
  noglob: usize,
  directory_variables: usize,
}

struct ObservedState {
  state: CapturedState,
  changes: StateChanges,
}

struct DeferredInput {
  snapshot: Snapshot,
  registration: Location,
  frame: Frame,
  environment: ExecutionContextId,
  state: CapturedState,
  revision: usize,
}

#[derive(Default)]
pub(super) struct InvariantInputs {
  observed: BTreeMap<ExecutionContextId, ObservedState>,
  pending: BTreeMap<(Snapshot, ExecutionContextId), DeferredInput>,
  revision: usize,
}

impl InvariantInputs {
  fn capture(
    &mut self,
    snapshot: Snapshot,
    registration: Location,
    frame: Frame,
    environment: ExecutionContextId,
    env: &Environment,
  ) {
    self.observe(environment, env);
    self
      .observed
      .entry(environment)
      .or_insert_with(|| ObservedState {
        state: CapturedState::from(env),
        changes: StateChanges::default(),
      });
    self
      .pending
      .entry((snapshot, frame.context))
      .or_insert_with(|| DeferredInput {
        snapshot,
        registration,
        frame,
        environment,
        state: CapturedState::from(env),
        revision: self.revision,
      });
  }

  pub(super) fn observe(
    &mut self,
    environment: ExecutionContextId,
    env: &Environment,
  ) {
    let Some(observed) = self.observed.get_mut(&environment) else {
      return;
    };
    self.revision += 1;
    let revision = self.revision;
    observed.changes.variables.observe(
      &mut observed.state.variables,
      &env.variables,
      revision,
    );
    observed.changes.functions.observe(
      &mut observed.state.functions,
      &env.functions,
      revision,
    );
    for difference in observed.state.readonly.diff(&env.readonly) {
      let name = match difference {
        DiffItem::Add(name, _)
        | DiffItem::Remove(name, _)
        | DiffItem::Update { new: (name, _), .. } => name,
      };
      observed.changes.readonly.insert(name.clone(), revision);
    }
    observed.state.readonly.clone_from(&env.readonly);
    if observed.state.unknown_attributes != env.unknown_attributes {
      observed.changes.attributes = revision;
      observed
        .state
        .unknown_attributes
        .clone_from(&env.unknown_attributes);
    }
    if observed.state.cwd != env.cwd {
      observed.changes.cwd = revision;
      observed.state.cwd.clone_from(&env.cwd);
    }
    if observed.state.directory_variables_modified
      != env.directory_variables_modified
    {
      observed.changes.directory_variables = revision;
      observed.state.directory_variables_modified =
        env.directory_variables_modified;
    }
    if observed.state.noglob != env.noglob {
      observed.changes.noglob = revision;
      observed.state.noglob = env.noglob;
    }
  }

  fn input(&self, input: &DeferredInput) -> Environment {
    let changes = &self.observed[&input.environment].changes;
    let mut env = Environment {
      variables: input.state.variables.clone(),
      functions: input.state.functions.clone(),
      readonly: input.state.readonly.clone(),
      unknown_attributes: if changes.attributes > input.revision {
        BTreeSet::from([Value::Unknown(ValueIssue::Dynamic)])
      } else {
        input.state.unknown_attributes.clone()
      },
      cwd: if changes.cwd > input.revision {
        Value::Unknown(ValueIssue::Dynamic)
      } else {
        input.state.cwd.clone()
      },
      noglob: if changes.noglob > input.revision {
        None
      } else {
        input.state.noglob
      },
      directory_variables_modified: input.state.directory_variables_modified
        || changes.directory_variables > input.revision,
      ..Environment::new(&InitialEnvironment::default())
    };
    let unknown = Binding::new(Resolution {
      alternatives: [Alternative::External, Alternative::Unset].into(),
      issues: [ResolutionIssue::UnknownEffect].into(),
      ..Resolution::default()
    });
    changes
      .variables
      .invalidate(&mut env.variables, input.revision, &unknown);
    changes
      .functions
      .invalidate(&mut env.functions, input.revision, &unknown);
    for (name, revision) in &changes.readonly {
      if *revision > input.revision {
        env.readonly.insert(name.clone(), false);
      }
    }
    env
  }
}

#[cfg(test)]
#[path = "../../../../../tests/implementation/deferred.rs"]
mod tests;
