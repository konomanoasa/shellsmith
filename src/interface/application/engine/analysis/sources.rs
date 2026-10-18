use super::execution::{Environment, Transfer};
use super::{Frame, Resolver, Task};
use crate::interface::application::engine::{
  self, Alternative, DependencyState, Error, ExecutionContextId, ExecutionKind,
  ReferenceId, ResolutionIssue, Snapshot, Value, ValueIssue, check_cancelled,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tree_sitter::Node;

pub enum SourceDocument {
  Resolved(Arc<engine::Document>),
  Unresolved(String),
  Failed(String),
}

/// Unused search conditions are `None`; required but unknown conditions keep their reason.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourceInput {
  pub operand: Value,
  pub cwd: Option<Value>,
  pub search_path: Option<Value>,
  pub annotation: Option<SourceAnnotation>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourceAnnotation {
  pub document: engine::DocumentId,
  pub path: Vec<u8>,
}

impl SourceInput {
  pub(crate) fn join(&mut self, other: &Self) {
    self.operand.join(&other.operand);
    join_condition(&mut self.cwd, other.cwd.as_ref());
    join_condition(&mut self.search_path, other.search_path.as_ref());
    if self.annotation != other.annotation {
      self.annotation = None;
    }
  }

  pub(crate) fn needs_annotation(&self) -> bool {
    !matches!(self.operand, Value::Literal(_))
      || self
        .cwd
        .as_ref()
        .is_some_and(|value| !matches!(value, Value::Literal(_)))
      || self
        .search_path
        .as_ref()
        .is_some_and(|value| !matches!(value, Value::Literal(_)))
  }

  pub(crate) fn normalized(mut self) -> Self {
    if let Value::Literal(value) = &self.operand {
      let absolute_search = !value.contains(&b'/')
        && matches!(&self.search_path, Some(Value::Literal(path)) if path.split(|byte| *byte == b':').all(|entry| entry.starts_with(b"/")));
      if value.starts_with(b"/") || absolute_search {
        self.cwd = None;
      }
      if value.contains(&b'/') {
        self.search_path = None;
      }
    }
    self
  }
}

fn join_condition(value: &mut Option<Value>, other: Option<&Value>) {
  match (value.as_mut(), other) {
    (Some(value), Some(other)) => value.join(other),
    (None, None) => {}
    _ => {
      *value = Some(Value::Unknown(ValueIssue::Dynamic));
    }
  }
}

pub(super) struct SourceTracking {
  states: BTreeMap<ReferenceId, DependencyState>,
  active: Vec<Snapshot>,
  non_sources: BTreeSet<(ReferenceId, ExecutionContextId)>,
}

impl SourceTracking {
  pub(super) fn new(entry: Snapshot) -> Self {
    Self {
      states: BTreeMap::new(),
      active: vec![entry],
      non_sources: BTreeSet::new(),
    }
  }

  pub(super) fn start_root(&mut self, entry: Snapshot) {
    self.active.clear();
    self.active.push(entry);
  }
}

impl<'a> Resolver<'a> {
  pub(super) fn non_source(&mut self, node: Node<'_>, frame: Frame) {
    if let Some(reference) = self
      .index
      .sources
      .get(&(frame.snapshot, node.start_byte(), node.end_byte()))
      .copied()
    {
      self.sources.non_sources.insert((reference, frame.context));
    }
  }

  pub(super) fn finish_sources(&mut self) {
    let used: BTreeSet<_> = self
      .result
      .source_inputs
      .keys()
      .map(|(id, _)| *id)
      .collect();
    for key in &self.sources.non_sources {
      if !used.contains(&key.0) {
        continue;
      }
      if self.inputs.is_some() {
        self.sources.states.insert(
          key.0,
          DependencyState::Unresolved(
            "Source command differs between execution contexts".into(),
          ),
        );
      }
      let input = SourceInput {
        operand: Value::Unknown(ValueIssue::Dynamic),
        cwd: None,
        search_path: None,
        annotation: None,
      };
      self
        .result
        .source_inputs
        .entry((key.0, Some(key.1)))
        .and_modify(|old| old.join(&input))
        .or_insert(input);
    }
  }

  pub(super) fn source(
    &mut self,
    node: Node<'a>,
    args: &[super::expansion::Argument<'a>],
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let reference = self
      .index
      .sources
      .get(&(frame.snapshot, node.start_byte(), node.end_byte()))
      .copied();
    let input = reference.map(|id| {
      let mut input = source_input(args, &env);
      if source_operand(args).is_some() && !node.has_error() {
        self.annotate_source(id, &mut input);
      }
      input
    });
    if let Some(reference) = reference {
      let operand = source_operand(args).map(|arg| frame.location(arg.node));
      self
        .result
        .source_operands
        .entry(reference)
        .and_modify(|old| {
          if *old != operand {
            *old = None;
          }
        })
        .or_insert(operand);
    }
    if let (Some(reference), Some(input)) = (reference, &input) {
      self
        .result
        .source_inputs
        .entry((reference, Some(frame.context)))
        .and_modify(|old| old.join(input))
        .or_insert_with(|| input.clone());
    }
    let target = if self.inputs.is_some() {
      match (reference, input.as_ref()) {
        (Some(id), Some(input)) => match self.load_source(id, input) {
          Ok(target) => target.map(|target| (id, target)),
          Err(error) => {
            self.error = Some(error);
            None
          }
        },
        _ => None,
      }
    } else {
      reference.and_then(|id| match self.workspace.reference_state(id) {
        Some(DependencyState::Resolved(target))
          if self.source_matches(id, input.as_ref()) =>
        {
          Some((id, *target))
        }
        _ => None,
      })
    };
    let Some((reference, target)) = target else {
      return Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::UnresolvedSource,
      ));
    };
    if self.sources.active.contains(&target) {
      return Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::Recursion,
      ));
    }
    let Some(document) = self
      .workspace
      .document(target.document)
      .filter(|document| document.snapshot() == target)
    else {
      return Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::UnresolvedSource,
      ));
    };
    let inner = self.execution(
      node,
      frame,
      ExecutionKind::Source(reference),
      target,
      false,
    );
    self.sources.active.push(target);
    Task::eval(document.tree.root_node(), env, inner).then(
      move |resolver, mut flow| {
        resolver.sources.active.pop();
        if let Some(env) = flow.0.remove(&Transfer::Return) {
          flow.add(Transfer::Next, env);
        }
        Task::Done(flow)
      },
    )
  }

  fn load_source(
    &mut self,
    id: ReferenceId,
    input: &SourceInput,
  ) -> Result<Option<Snapshot>, Error> {
    let loaded = self
      .inputs
      .as_mut()
      .expect("input provider")
      .source(input)?;
    let state = match loaded {
      SourceDocument::Resolved(document) => {
        let snapshot = document.snapshot();
        if let Some(existing) = self.workspace.document(snapshot.document) {
          if existing.snapshot() != snapshot {
            return Err(Error::StaleSnapshot(snapshot));
          }
        } else {
          self.workspace.workspace.insert_shared(document)?;
          self.register_document(snapshot)?;
        }
        self.workspace.workspace.index_document(
          snapshot.document,
          self.result.request.limits,
          self.cancelled,
        )?;
        DependencyState::Resolved(snapshot)
      }
      SourceDocument::Unresolved(message) => {
        DependencyState::Unresolved(message)
      }
      SourceDocument::Failed(message) => DependencyState::Failed(message),
    };
    let target = match &state {
      DependencyState::Resolved(target) => Some(*target),
      _ => None,
    };
    self
      .sources
      .states
      .entry(id)
      .and_modify(|old| {
        if *old != state {
          *old = DependencyState::Unresolved(
            "Source target differs between execution contexts".into(),
          );
        }
      })
      .or_insert(state);
    Ok(target)
  }

  fn annotate_source(&self, id: ReferenceId, input: &mut SourceInput) {
    if input.operand.issue().is_some_and(|issue| {
      issue.causes().any(|cause| {
        matches!(
          cause,
          ValueIssue::EmptyOperand
            | ValueIssue::MissingOperand
            | ValueIssue::Recovery
            | ValueIssue::Nul
        )
      })
    }) {
      return;
    }
    input.annotation = self
      .workspace
      .document(id.snapshot.document)
      .and_then(|document| document.reference(id))
      .and_then(|reference| reference.annotation.as_ref())
      .map(|path| SourceAnnotation {
        document: self.workspace.source_snapshot(id.snapshot).document,
        path: path.clone(),
      });
  }

  fn source_matches(
    &self,
    id: ReferenceId,
    input: Option<&SourceInput>,
  ) -> bool {
    let Some(input) = input else {
      return false;
    };
    if !matches!(input.operand, Value::Literal(_)) && input.annotation.is_none()
    {
      return false;
    }
    if let Some(expected) = self.result.request.source_inputs.get(&id) {
      return expected == input;
    }
    let Value::Literal(value) = &input.operand else {
      return false;
    };
    let Some(reference) = self
      .workspace
      .document(id.snapshot.document)
      .and_then(|doc| doc.reference(id))
    else {
      return false;
    };
    match &reference.value {
      Value::Literal(expected) => expected == value,
      Value::Unknown(_) | Value::Partial(_) => false,
    }
  }
}

fn source_input(
  args: &[super::expansion::Argument<'_>],
  env: &Environment,
) -> SourceInput {
  let path = env.variables.get(b"PATH");
  let unset = path.resolution.candidates.is_empty()
    && path.resolution.alternatives == [Alternative::Unset].into();
  SourceInput {
    operand: source_value(args),
    annotation: None,
    cwd: Some(env.cwd.clone()),
    search_path: Some(if unset {
      Value::Unknown(ValueIssue::Dynamic)
    } else {
      path.value.as_ref().clone()
    }),
  }
  .normalized()
}

pub(in crate::interface::application::engine) fn deferred_source_input(
  node: Node<'_>,
  source: &[u8],
) -> Option<SourceInput> {
  let command = node
    .child_by_field_name("name")
    .or_else(|| node.child_by_field_name("word"))?;
  if node.has_error()
    || engine::word::literal_value(command, source)
      != Value::Literal(b".".to_vec())
  {
    return None;
  }
  let args = super::command::syntax_arguments(node, source);
  source_operand(&args)?;
  Some(
    SourceInput {
      operand: source_value(&args),
      annotation: None,
      cwd: Some(Value::Unknown(ValueIssue::Dynamic)),
      search_path: Some(Value::Unknown(ValueIssue::Dynamic)),
    }
    .normalized(),
  )
}

fn source_value(args: &[super::expansion::Argument<'_>]) -> Value {
  let first = operand_index(args);
  match args.get(first) {
    Some(arg) if arg.value.issue().is_some() => arg.value.clone(),
    Some(_) if source_operand(args).is_none() => {
      Value::Unknown(ValueIssue::Dynamic)
    }
    Some(arg) if arg.value == Value::Literal(Vec::new()) => {
      Value::Unknown(ValueIssue::EmptyOperand)
    }
    Some(arg) => arg.value.clone(),
    None => Value::Unknown(ValueIssue::MissingOperand),
  }
}

fn source_operand<'a, 'b>(
  args: &'b [super::expansion::Argument<'a>],
) -> Option<&'b super::expansion::Argument<'a>> {
  let first = operand_index(args);
  let operand = args.get(first)?;
  args[..=first]
    .iter()
    .all(|arg| arg.boundary_known)
    .then_some(operand)
}

fn operand_index(args: &[super::expansion::Argument<'_>]) -> usize {
  usize::from(args.first().is_some_and(|arg| {
    arg.boundary_known && arg.value == Value::Literal(b"--".to_vec())
  }))
}

impl Resolver<'_> {
  pub(super) fn collect_deferred_sources(&mut self) -> Result<(), Error> {
    let mut pending: BTreeSet<_> = self
      .result
      .contexts
      .iter()
      .map(|context| context.location.snapshot)
      .chain(
        self
          .result
          .derived
          .bodies
          .values()
          .flat_map(|body| body.documents.keys().copied()),
      )
      .collect();
    let mut visited = BTreeSet::new();
    let mut excluded =
      self.result.derived.incomplete_bodies(Some(&self.result));
    let mut observed: BTreeSet<_> = self
      .result
      .source_inputs
      .keys()
      .map(|(id, _)| *id)
      .chain(self.sources.non_sources.iter().map(|(id, _)| *id))
      .collect();
    while let Some(snapshot) = pending.pop_first() {
      if !visited.insert(snapshot) || excluded.contains(&snapshot) {
        continue;
      }
      check_cancelled(self.cancelled)?;
      let document = self
        .workspace
        .workspace
        .documents
        .get(&snapshot.document)
        .ok_or(Error::UnknownDocument(snapshot.document))?
        .clone();
      for (reference, mut input) in document.deferred_sources() {
        if !observed.insert(reference.id) {
          continue;
        }
        check_cancelled(self.cancelled)?;
        self.annotate_source(reference.id, &mut input);
        self
          .result
          .source_operands
          .insert(reference.id, reference.operand.clone());
        self
          .result
          .source_inputs
          .insert((reference.id, None), input.clone());
        let target = if self.inputs.is_some() {
          self.load_source(reference.id, &input)?
        } else {
          self
            .workspace
            .reference_state(reference.id)
            .and_then(|state| match state {
              DependencyState::Resolved(target)
                if self.result.request.source_inputs.get(&reference.id)
                  == Some(&input) =>
              {
                Some(*target)
              }
              _ => None,
            })
        };
        if let Some(target) = target {
          if visited.contains(&target) {
            continue;
          }
          pending.insert(target);
          let mut derived = std::mem::take(&mut self.result.derived);
          derived.collect(
            self.workspace.workspace,
            [target].into(),
            Some(&self.result),
            self.result.request.limits.value_bytes,
            self.cancelled,
          )?;
          pending.extend(
            derived
              .bodies
              .values()
              .flat_map(|body| body.documents.keys().copied())
              .filter(|snapshot| !visited.contains(snapshot)),
          );
          self.result.derived = derived;
          excluded = self.result.derived.incomplete_bodies(Some(&self.result));
        }
      }
    }
    Ok(())
  }

  pub(super) fn commit_sources(&mut self) -> Result<(), Error> {
    if self.inputs.is_some() {
      self.result.request.source_inputs.clear();
      for ((id, _), input) in &self.result.source_inputs {
        self
          .result
          .request
          .source_inputs
          .entry(*id)
          .and_modify(|old| old.join(input))
          .or_insert_with(|| input.clone());
      }
      for (id, state) in &self.sources.states {
        self.workspace.workspace.set_reference(*id, state.clone())?;
      }
    }
    Ok(())
  }
}
