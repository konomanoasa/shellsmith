use super::command::operand_index;
use super::execution::Environment;
use super::{Frame, Resolver, Task};
use crate::interface::application::engine::{
  self, DependencyState, EffectTarget, Error, ExecutionContextId,
  ExecutionKind, ReferenceId, ResolutionIssue, Snapshot, Value, ValueIssue,
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
  pub(in crate::interface::application::engine) fn join(
    &mut self,
    other: &Self,
  ) {
    self.operand.join(&other.operand);
    join_condition(&mut self.cwd, other.cwd.as_ref());
    join_condition(&mut self.search_path, other.search_path.as_ref());
    if self.annotation != other.annotation {
      self.annotation = None;
    }
  }

  pub(in crate::interface::application) fn needs_annotation(&self) -> bool {
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

  fn normalized(mut self) -> Self {
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
    if let Some(reference) = self.index.source(frame.snapshot, node) {
      self.sources.non_sources.insert((reference, frame.context));
    }
  }

  pub(super) fn finish_sources(&mut self) {
    self.result.non_sources = self
      .sources
      .non_sources
      .iter()
      .map(|(reference, _)| *reference)
      .collect();
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
      self.sources.states.insert(
        key.0,
        DependencyState::Unresolved(
          "Source command differs between execution contexts".into(),
        ),
      );
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
    let Some(reference) = self.index.source(frame.snapshot, node) else {
      return Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::UnresolvedSource,
        None,
      ));
    };
    let operand = source_operand(args);
    let mut input = source_input(args, &env);
    if operand.is_some() && !node.has_error() {
      self.annotate_source(reference, &mut input);
    }
    let location = operand.map(|arg| frame.location(arg.node));
    self
      .result
      .source_operands
      .entry(reference)
      .and_modify(|old| {
        if *old != location {
          *old = None;
        }
      })
      .or_insert(location);
    self
      .result
      .source_inputs
      .entry((reference, Some(frame.context)))
      .and_modify(|old| old.join(&input))
      .or_insert_with(|| input.clone());
    let target = self.load_source(reference, &input).unwrap_or_else(|error| {
      self.error = Some(error);
      None
    });
    let Some(target) = target else {
      let issue = std::iter::once(&input.operand)
        .chain(input.cwd.as_ref())
        .chain(input.search_path.as_ref())
        .filter_map(Value::issue)
        .reduce(ValueIssue::combine);
      return Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::UnresolvedSource,
        issue,
      ));
    };
    self.source_document(node, args, env, frame, reference, target)
  }

  /// Evaluates a loaded `.` file in the current environment.
  fn source_document(
    &mut self,
    node: Node<'a>,
    args: &[super::expansion::Argument<'a>],
    mut env: Environment,
    frame: Frame,
    reference: ReferenceId,
    target: Snapshot,
  ) -> Task<'a> {
    if self.sources.active.contains(&target) {
      return Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::Recursion,
        None,
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
        None,
      ));
    };
    let root = document.tree.root_node();
    let empty =
      root.child_by_field_name("commands").is_none() && !root.has_error();
    // XCU dot defines only the file operand. Further operands may replace the
    // positional parameters (a KornShell extension), and restoring them
    // afterwards differs between shells.
    let arguments = args.len() > operand_index(args) + 1;
    if arguments {
      self.invalidate(
        node,
        &mut env,
        frame,
        ResolutionIssue::UnknownEffect,
        ValueIssue::Unspecified,
        [EffectTarget::Positionals],
      );
    }
    let inner =
      self.execution(node, frame, ExecutionKind::Source(reference), target);
    self.sources.active.push(target);
    Task::eval(root, env, inner).then(move |resolver, flow| {
      resolver.sources.active.pop();
      let mut flow = flow.returned();
      for env in flow.environments_mut() {
        if empty {
          env.set_status(Value::Literal(b"0".to_vec()));
        }
        if arguments {
          resolver.invalidate(
            node,
            env,
            frame,
            ResolutionIssue::UnknownEffect,
            ValueIssue::Unspecified,
            [EffectTarget::Positionals],
          );
        }
      }
      Task::Done(flow)
    })
  }

  fn load_source(
    &mut self,
    id: ReferenceId,
    input: &SourceInput,
  ) -> Result<Option<Snapshot>, Error> {
    let loaded = self.inputs.source(input)?;
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
        self.workspace.workspace.index(
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
      .and_then(|reference| reference.annotation.as_deref())
      .and_then(|path| self.annotation_path(path))
      .map(|path| SourceAnnotation {
        document: self
          .workspace
          .workspace
          .source_snapshot(id.snapshot)
          .document,
        path,
      });
  }

  /// Uses the initial HOME; relative paths are rejected.
  fn annotation_path(&self, path: &[u8]) -> Option<Vec<u8>> {
    let Some(rest) = path
      .strip_prefix(b"~")
      .filter(|rest| rest.is_empty() || rest.starts_with(b"/"))
    else {
      return Some(path.to_vec());
    };
    match Environment::new(&self.result.request.initial).parameter(b"HOME") {
      (Value::Literal(mut home), Some(false)) if home.starts_with(b"/") => {
        let rest = if home.ends_with(b"/") {
          rest.strip_prefix(b"/").unwrap_or(rest)
        } else {
          rest
        };
        home.extend_from_slice(rest);
        Some(home)
      }
      _ => None,
    }
  }
}

fn source_input(
  args: &[super::expansion::Argument<'_>],
  env: &Environment,
) -> SourceInput {
  let (path, absent) = env.parameter(b"PATH");
  // XBD 8.3 leaves the search implementation-defined when PATH is unset or
  // null or a prefix contains `%`.
  let search_path = match path {
    Value::Literal(path)
      if absent != Some(false) || path.is_empty() || path.contains(&b'%') =>
    {
      Value::Unknown(ValueIssue::Unspecified)
    }
    path => path,
  };
  SourceInput {
    operand: source_value(args),
    annotation: None,
    cwd: Some(env.cwd.clone()),
    search_path: Some(search_path),
  }
  .normalized()
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
    .all(|arg| arg.count.boundary().is_ok())
    .then_some(operand)
}

impl Resolver<'_> {
  pub(super) fn commit_sources(&mut self) -> Result<(), Error> {
    for (id, state) in &self.sources.states {
      self.workspace.workspace.set_reference(*id, state.clone())?;
    }
    Ok(())
  }
}
