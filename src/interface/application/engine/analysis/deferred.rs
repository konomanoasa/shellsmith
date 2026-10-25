use super::execution::{Binding, Environment, Names};
use super::{Frame, Resolver, Task};
use crate::interface::application::engine::{
  self, Alternative, DerivedOrigin, Error, ExecutionContextId, ExecutionKind,
  InitialEnvironment, Location, OccurrenceId, Resolution, ResolutionIssue,
  Snapshot, Value, ValueIssue, check_cancelled,
};
use imbl::{OrdMap, ordmap::DiffItem};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;
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
    let generation = self.trace_generator(command, &action, frame.context);
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
    let recursive = generation.as_ref().map_or_else(
      || self.recursive_registration(snapshot, &registration, frame),
      |(_, key)| {
        self.ancestors(frame).any(|context| {
          let ExecutionKind::Deferred(ancestor) = context.kind else {
            return false;
          };
          context.parent.is_some_and(|parent| {
            self
              .invariants
              .generators
              .get(&(ancestor, parent))
              .and_then(Option::as_ref)
              == Some(key)
          })
        })
      },
    );
    let (trace, key) = generation.unzip();
    self
      .invariants
      .traces
      .entry((snapshot, frame.context))
      .and_modify(|old| {
        if *old != trace {
          *old = None;
        }
      })
      .or_insert(trace);
    self
      .invariants
      .generators
      .entry((snapshot, frame.context))
      .and_modify(|old| {
        if *old != key {
          *old = None;
        }
      })
      .or_insert(key);
    if recursive {
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
        && self
          .workspace
          .workspace
          .document(ancestor.document)
          .map(engine::Document::source)
          == self
            .workspace
            .workspace
            .document(body.document)
            .map(engine::Document::source)
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

  fn invalidate(
    &self,
    names: &mut Names,
    since: usize,
    environment: ExecutionContextId,
  ) {
    for (name, revision) in &self.values {
      if *revision > since {
        names
          .values
          .insert(name.clone(), Self::unknown(names.get(name), environment));
      }
    }
    if self.fallback > since {
      let mut previous = names.fallback.clone();
      if let Some(uncertain) = &names.uncertain {
        previous.join(uncertain);
      }
      names.fallback = Self::unknown(&previous, environment);
      names.patterns.clear();
      names.uncertain = None;
    } else {
      for (pattern, revision) in &self.patterns {
        if *revision > since {
          names.restrict(
            pattern.clone(),
            engine::BindingOrigin::Context(environment),
            ResolutionIssue::UnknownEffect,
            pattern.issue().unwrap_or(ValueIssue::Dynamic),
          );
        }
      }
    }
  }

  fn unknown(previous: &Binding, environment: ExecutionContextId) -> Binding {
    let origins = previous
      .resolution
      .origins
      .iter()
      .copied()
      .chain(
        previous
          .resolution
          .candidates
          .iter()
          .copied()
          .map(engine::BindingOrigin::Definition),
      )
      .chain([engine::BindingOrigin::Context(environment)])
      .collect();
    Binding::new(Resolution {
      alternatives: [Alternative::External, Alternative::Unset].into(),
      issues: [ResolutionIssue::UnknownEffect].into(),
      origins,
      ..Resolution::default()
    })
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
  traces: BTreeMap<(Snapshot, ExecutionContextId), Option<BodyTrace>>,
  generators: BTreeMap<(Snapshot, ExecutionContextId), Option<Generator>>,
  substitutions: BTreeMap<TraceKey, Option<OutputTrace>>,
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
    changes.variables.invalidate(
      &mut env.variables,
      input.revision,
      input.environment,
    );
    changes.functions.invalidate(
      &mut env.functions,
      input.revision,
      input.environment,
    );
    for (name, revision) in &changes.readonly {
      if *revision > input.revision {
        env.readonly.insert(name.clone(), false);
      }
    }
    env
  }
}

type TemplateByte = (Snapshot, usize, usize);
type TemplateOrigin = Vec<TemplateByte>;
type Generator = (TemplateOrigin, TemplateOrigin);

#[derive(Clone, Debug, Eq, PartialEq)]
struct TracePart<Source> {
  bytes: Range<usize>,
  source: Source,
  range: Range<usize>,
  linear: bool,
}

fn trace_origins(parts: &[TracePart<Snapshot>]) -> TemplateOrigin {
  let ranges: BTreeSet<_> = parts
    .iter()
    .map(|part| (part.source, part.range.start, part.range.end))
    .collect();
  let mut merged: TemplateOrigin = Vec::new();
  for (snapshot, start, end) in ranges {
    if let Some(last) = merged.last_mut()
      && last.0 == snapshot
      && start <= last.2
    {
      last.2 = last.2.max(end);
    } else {
      merged.push((snapshot, start, end));
    }
  }
  merged
}

pub(super) type BodyTrace = Trace<Snapshot>;
pub(super) type OutputTrace = Trace<OutputSource>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Trace<Source> {
  bytes: Vec<u8>,
  origins: Vec<TracePart<Source>>,
}

impl<Source> Default for Trace<Source> {
  fn default() -> Self {
    Self {
      bytes: Vec::new(),
      origins: Vec::new(),
    }
  }
}

// Resolve output dependencies only when a derived body needs them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct OutputSource {
  key: TraceKey,
  expected: Arc<[u8]>,
}

impl Resolver<'_> {
  fn trace_body(
    &self,
    location: &Location,
    context: ExecutionContextId,
    assignment: bool,
  ) -> Option<BodyTrace> {
    let root = TraceKey::word(location, context, assignment);
    let mut pending = vec![(root.clone(), None)];
    let mut results = BTreeMap::new();
    while let Some((key, recipe)) = pending.pop() {
      check_cancelled(self.cancelled).ok()?;
      if let Some(recipe) = recipe {
        results.insert(key, evaluate_trace(recipe, &results));
      } else if !results.contains_key(&key) {
        // A dependency on a pending key has no established provenance.
        results.insert(key.clone(), None);
        if let Some(recipe) = self.trace_recipe(&key) {
          let dependencies: Vec<_> = recipe
            .parts
            .iter()
            .filter_map(|part| match part {
              TraceTerm::Input(key) => Some((key.clone(), None)),
              TraceTerm::Project { input, .. } => {
                Some((input.key.clone(), None))
              }
              TraceTerm::Literal(_) => None,
            })
            .collect();
          pending.push((key, Some(recipe)));
          pending.extend(dependencies.into_iter().rev());
        }
      }
    }
    results.remove(&root).flatten()
  }

  fn trace_recipe(&self, key: &TraceKey) -> Option<TraceRecipe> {
    match key {
      TraceKey::Word(snapshot, start, end, context, assignment) => self
        .word_trace_recipe(
          &Location {
            snapshot: *snapshot,
            bytes: *start..*end,
          },
          *context,
          *assignment,
        ),
      TraceKey::Source(snapshot, start, end, context) => Some(TraceRecipe {
        parts: vec![TraceTerm::Literal(self.source_trace(
          &Location {
            snapshot: *snapshot,
            bytes: *start..*end,
          },
          *context,
        )?)],
        operation: TraceOperation::Concat,
      }),
      TraceKey::Substitution(..) => {
        let trace = self.invariants.substitutions.get(key)?.as_ref()?;
        Some(TraceRecipe {
          parts: trace
            .origins
            .iter()
            .map(|part| TraceTerm::Project {
              input: part.source.clone(),
              range: part.range.clone(),
              bytes: trace.bytes[part.bytes.clone()].to_vec(),
            })
            .collect(),
          operation: TraceOperation::Concat,
        })
      }
      TraceKey::Parameter(site, context) => {
        let value = self.result.value(*site, *context)?.literal().ok()?;
        let resolution = self
          .result
          .resolution(engine::UseSite::Syntax(*site), *context)?;
        if !resolution.alternatives.is_empty()
          || !resolution.issues.is_empty()
          || resolution.candidates.is_empty()
        {
          return None;
        }
        Some(TraceRecipe {
          parts: resolution
            .candidates
            .iter()
            .map(|symbol| {
              TraceTerm::Input(TraceKey::Definition(*symbol, value.to_vec()))
            })
            .collect(),
          operation: TraceOperation::Agree(None),
        })
      }
      TraceKey::Definition(id, expected) => {
        let symbol = self.workspace.symbol(*id)?;
        let definition = self.workspace.occurrence(symbol.definition)?;
        let document = self.workspace.document(id.snapshot.document)?;
        let name = document.tree.root_node().named_descendant_for_byte_range(
          definition.location.bytes.start,
          definition.location.bytes.end,
        )?;
        let assignment =
          std::iter::successors(Some(name), Node::parent).find(|node| {
            node.child_by_field_name("name").is_some_and(|name| {
              name.byte_range() == definition.location.bytes
            })
          })?;
        let value = assignment.child_by_field_name("value")?;
        let location = Location {
          snapshot: id.snapshot,
          bytes: value.byte_range(),
        };
        Some(TraceRecipe {
          parts: self
            .result
            .writes
            .range(
              (*id, ExecutionContextId(0))
                ..=(*id, ExecutionContextId(usize::MAX)),
            )
            .map(|(_, context)| {
              TraceTerm::Input(TraceKey::word(&location, *context, true))
            })
            .collect(),
          operation: TraceOperation::Agree(Some(expected.clone())),
        })
      }
    }
  }

  fn word_trace_recipe(
    &self,
    location: &Location,
    context: ExecutionContextId,
    assignment: bool,
  ) -> Option<TraceRecipe> {
    let document = self.workspace.document(location.snapshot.document)?;
    let node = document.tree.root_node().named_descendant_for_byte_range(
      location.bytes.start,
      location.bytes.end,
    )?;
    if node.byte_range() != location.bytes || node.has_error() {
      return None;
    }
    let mut parts = Vec::new();
    for part in
      engine::word::Parts::new(node, false, document.source().as_bytes())
    {
      check_cancelled(self.cancelled).ok()?;
      let term = match part {
        engine::word::Part::Fragment(fragment) => {
          if fragment.bytes.is_empty() {
            continue;
          }
          if !assignment
            && !fragment.quoted
            && fragment.bytes.iter().any(|byte| b"*?[".contains(byte))
          {
            return None;
          }
          let location = Location {
            snapshot: location.snapshot,
            bytes: fragment.source?,
          };
          let mut trace = self.source_trace(&location, context)?;
          trace.replace_bytes(&fragment.bytes);
          TraceTerm::Literal(trace)
        }
        engine::word::Part::Expansion(expansion, quoted) => {
          if !assignment && !quoted {
            return None;
          }
          if matches!(
            expansion.kind(),
            "command_substitution" | "backquote_substitution"
          ) {
            TraceTerm::Input(TraceKey::substitution(
              &Location {
                snapshot: location.snapshot,
                bytes: expansion.byte_range(),
              },
              context,
            ))
          } else if expansion.kind() == "parameter_expansion"
            && expansion.child_by_field_name("operator").is_none()
          {
            let parameter = expansion.child_by_field_name("parameter")?;
            let site = self.index.site(location.snapshot, parameter)?;
            TraceTerm::Input(TraceKey::Parameter(site, context))
          } else {
            return None;
          }
        }
        _ => return None,
      };
      parts.push(term);
    }
    Some(TraceRecipe {
      parts,
      operation: TraceOperation::Concat,
    })
  }

  pub(super) fn source_trace(
    &self,
    location: &Location,
    context: ExecutionContextId,
  ) -> Option<BodyTrace> {
    if self
      .workspace
      .workspace
      .derived_origin(location.snapshot.document)
      .is_none()
    {
      return Some(BodyTrace {
        bytes: self
          .workspace
          .document(location.snapshot.document)?
          .text(location)?
          .as_bytes()
          .to_vec(),
        origins: vec![TracePart {
          bytes: 0..location.bytes.len(),
          source: location.snapshot,
          range: location.bytes.clone(),
          linear: true,
        }],
      });
    }
    let ancestor = self.result.context_ancestors(context).find(|ancestor| {
      matches!(ancestor.kind, ExecutionKind::Deferred(snapshot) if snapshot == location.snapshot)
    })?;
    self
      .invariants
      .traces
      .get(&(location.snapshot, ancestor.parent?))?
      .as_ref()
      .map(|trace| trace.slice(location.bytes.clone()))
  }

  pub(super) fn argument_output_trace(
    argument: &super::expansion::Argument<'_>,
    frame: Frame,
  ) -> Option<OutputTrace> {
    let value = argument.literal()?;
    let assignment = argument.node.child_by_field_name("name").is_some();
    let node = if assignment {
      argument.node.child_by_field_name("value")?
    } else {
      argument.node
    };
    Some(OutputTrace::input(
      TraceKey::word(&frame.location(node), frame.context, assignment),
      value,
    ))
  }

  pub(super) fn literal_output_trace(
    &self,
    location: &Location,
    context: ExecutionContextId,
  ) -> Option<OutputTrace> {
    let bytes = self
      .workspace
      .document(location.snapshot.document)?
      .text(location)?
      .as_bytes();
    Some(OutputTrace::input(
      TraceKey::Source(
        location.snapshot,
        location.bytes.start,
        location.bytes.end,
        context,
      ),
      bytes,
    ))
  }

  pub(super) fn substitution_trace(
    &mut self,
    location: &Location,
    context: ExecutionContextId,
    trace: Option<OutputTrace>,
  ) {
    self
      .invariants
      .substitutions
      .entry(TraceKey::substitution(location, context))
      .and_modify(|old| {
        if *old != trace {
          *old = None;
        }
      })
      .or_insert(trace);
  }

  fn trace_generator(
    &self,
    command: OccurrenceId,
    action: &Result<Option<(DerivedOrigin, engine::Value)>, ()>,
    context: ExecutionContextId,
  ) -> Option<(BodyTrace, Generator)> {
    let (origin, Value::Literal(value)) = action.as_ref().ok()?.as_ref()?
    else {
      return None;
    };
    let trace = self.trace_body(&origin.argument, context, false)?;
    if &trace.bytes != value {
      return None;
    }
    let site = self.workspace.occurrence(command)?;
    let command =
      trace_origins(&self.source_trace(&site.location, context)?.origins);
    let material = trace_origins(&trace.origins);
    Some((trace, (command, material)))
  }
}

impl<Source: Clone + PartialEq> Trace<Source> {
  pub(super) fn matching(self, bytes: &[u8]) -> Option<Self> {
    (self.bytes == bytes).then_some(self)
  }

  pub(super) fn append(&mut self, mut other: Self) {
    let offset = self.bytes.len();
    for origin in &mut other.origins {
      origin.bytes.start += offset;
      origin.bytes.end += offset;
    }
    self.bytes.extend(other.bytes);
    for part in other.origins {
      if let Some(last) = self.origins.last_mut()
        && last.bytes.end == part.bytes.start
        && last.source == part.source
        && last.linear
        && part.linear
        && last.range.end == part.range.start
      {
        last.bytes.end = part.bytes.end;
        last.range.end = part.range.end;
      } else {
        self.origins.push(part);
      }
    }
  }

  pub(super) fn slice(&self, range: Range<usize>) -> Self {
    let origins = self
      .origins
      .iter()
      .filter_map(|part| {
        let start = part.bytes.start.max(range.start);
        let end = part.bytes.end.min(range.end);
        if start >= end {
          return None;
        }
        let input = if part.linear {
          part.range.start + start - part.bytes.start
            ..part.range.start + end - part.bytes.start
        } else {
          part.range.clone()
        };
        Some(TracePart {
          bytes: start - range.start..end - range.start,
          source: part.source.clone(),
          range: input,
          linear: part.linear,
        })
      })
      .collect();
    Self {
      bytes: self.bytes[range].to_vec(),
      origins,
    }
  }

  pub(super) fn replace_bytes(&mut self, bytes: &[u8]) {
    if self.bytes.len() != bytes.len() {
      for part in &mut self.origins {
        part.bytes = 0..bytes.len();
        part.linear = false;
      }
    }
    self.bytes = bytes.to_vec();
  }
}

impl OutputTrace {
  fn input(key: TraceKey, bytes: &[u8]) -> Self {
    Self {
      bytes: bytes.to_vec(),
      origins: vec![TracePart {
        bytes: 0..bytes.len(),
        source: OutputSource {
          key,
          expected: Arc::from(bytes),
        },
        range: 0..bytes.len(),
        linear: true,
      }],
    }
  }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum TraceKey {
  Word(Snapshot, usize, usize, ExecutionContextId, bool),
  Source(Snapshot, usize, usize, ExecutionContextId),
  Substitution(Snapshot, usize, usize, ExecutionContextId),
  Parameter(OccurrenceId, ExecutionContextId),
  Definition(engine::SymbolId, Vec<u8>),
}

impl TraceKey {
  fn substitution(location: &Location, context: ExecutionContextId) -> Self {
    Self::Substitution(
      location.snapshot,
      location.bytes.start,
      location.bytes.end,
      context,
    )
  }

  fn word(
    location: &Location,
    context: ExecutionContextId,
    assignment: bool,
  ) -> Self {
    Self::Word(
      location.snapshot,
      location.bytes.start,
      location.bytes.end,
      context,
      assignment,
    )
  }
}

enum TraceTerm {
  Literal(BodyTrace),
  Input(TraceKey),
  Project {
    input: OutputSource,
    range: Range<usize>,
    bytes: Vec<u8>,
  },
}

enum TraceOperation {
  Concat,
  Agree(Option<Vec<u8>>),
}

struct TraceRecipe {
  parts: Vec<TraceTerm>,
  operation: TraceOperation,
}

fn evaluate_trace(
  recipe: TraceRecipe,
  results: &BTreeMap<TraceKey, Option<BodyTrace>>,
) -> Option<BodyTrace> {
  let mut output: Option<BodyTrace> = None;
  for part in recipe.parts {
    let next = match part {
      TraceTerm::Literal(trace) => trace,
      TraceTerm::Input(key) => results.get(&key)?.clone()?,
      TraceTerm::Project {
        input,
        range,
        bytes,
      } => {
        let mut trace = results
          .get(&input.key)?
          .clone()?
          .matching(&input.expected)?
          .slice(range);
        trace.replace_bytes(&bytes);
        trace
      }
    };
    match &recipe.operation {
      TraceOperation::Concat => output.get_or_insert_default().append(next),
      TraceOperation::Agree(expected) => {
        if expected
          .as_ref()
          .is_some_and(|expected| expected != &next.bytes)
        {
          continue;
        }
        if output.as_ref().is_some_and(|old| old != &next) {
          return None;
        }
        output = Some(next);
      }
    }
  }
  match recipe.operation {
    TraceOperation::Concat => Some(output.unwrap_or_default()),
    TraceOperation::Agree(_) => output,
  }
}

#[cfg(test)]
#[path = "../../../../../tests/implementation/deferred.rs"]
mod tests;
