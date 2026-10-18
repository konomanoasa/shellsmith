use super::{
  deferred::InvariantInputs, execution, request::validate,
  sources::SourceTracking,
};
use crate::interface::application::engine;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;

use engine::check_cancelled;
use engine::workspace::derived::{
  DerivedCandidate, DerivedDocuments, DerivedKey,
};
use engine::{
  Access, Error, Location, OccurrenceId, OccurrenceKind, ReferenceId, Snapshot,
  SymbolId, SymbolKind, UseSite, Workspace,
};
use engine::{
  Analysis, AnalysisGap, AnalysisInputs, AnalysisRequest, DerivedOrigin,
  EffectTarget, ExecutionContext, ExecutionContextId, ExecutionKind,
  Resolution, ResolutionIssue, ResolvedUse,
};
use execution::Environment;
use execution::Task;

#[derive(Clone, Copy)]
pub(super) struct Frame {
  pub(super) snapshot: Snapshot,
  pub(super) context: ExecutionContextId,
  pub(super) loops: u32,
  pub(super) dynamic_loops: bool,
  pub(super) returnable: bool,
}

impl Frame {
  pub(super) fn location(self, node: Node<'_>) -> Location {
    Location {
      snapshot: self.snapshot,
      bytes: node.byte_range(),
    }
  }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ContextKey {
  pub(super) parent: ExecutionContextId,
  pub(super) kind: ExecutionKind,
  pub(super) snapshot: Snapshot,
  pub(super) start: usize,
  pub(super) end: usize,
}

pub(super) struct AnalysisWorkspace<'a> {
  pub(super) workspace: &'a mut Workspace,
  pub(super) arena: &'a typed_arena::Arena<Arc<engine::Document>>,
  pub(super) documents: BTreeMap<engine::DocumentId, &'a engine::Document>,
}

impl<'a> AnalysisWorkspace<'a> {
  pub(super) fn new(
    workspace: &'a mut Workspace,
    arena: &'a typed_arena::Arena<Arc<engine::Document>>,
  ) -> Self {
    let documents = workspace
      .documents
      .iter()
      .map(|(id, doc)| {
        let document: &'a engine::Document = arena.alloc(doc.clone());
        (*id, document)
      })
      .collect();
    Self {
      workspace,
      arena,
      documents,
    }
  }

  pub(super) fn document(
    &self,
    id: engine::DocumentId,
  ) -> Option<&'a engine::Document> {
    self.documents.get(&id).copied()
  }

  pub(super) fn occurrence(
    &self,
    id: OccurrenceId,
  ) -> Option<&'a engine::Occurrence> {
    self.document(id.snapshot.document)?.occurrence(id)
  }

  pub(super) fn symbol(&self, id: SymbolId) -> Option<&'a engine::Symbol> {
    self.document(id.snapshot.document)?.symbol(id)
  }

  pub(super) fn register(
    &mut self,
    snapshot: Snapshot,
  ) -> &'a engine::Document {
    let document: &'a engine::Document = self
      .arena
      .alloc(self.workspace.documents[&snapshot.document].clone());
    self.documents.insert(snapshot.document, document);
    document
  }
}

impl std::ops::Deref for AnalysisWorkspace<'_> {
  type Target = Workspace;
  fn deref(&self) -> &Workspace {
    self.workspace
  }
}

pub(super) struct Resolver<'a> {
  pub(super) workspace: AnalysisWorkspace<'a>,
  pub(super) inputs: Option<&'a mut dyn AnalysisInputs>,
  pub(super) error: Option<Error>,
  pub(super) sources: SourceTracking,
  pub(super) cancelled: &'a AtomicBool,
  pub(super) result: Analysis,
  pub(super) index: Index<'a>,
  pub(super) contexts: BTreeMap<ContextKey, ExecutionContextId>,
  pub(super) gaps:
    BTreeSet<(ExecutionContextId, Snapshot, usize, usize, ResolutionIssue)>,
  pub(super) invariants: InvariantInputs,
  pub(super) track_invariants: bool,
}

pub(in crate::interface::application::engine) fn analyze<'a>(
  workspace: &'a mut Workspace,
  request: AnalysisRequest,
  arena: &'a typed_arena::Arena<Arc<engine::Document>>,
  inputs: Option<&'a mut (dyn AnalysisInputs + 'a)>,
  cancelled: &'a AtomicBool,
) -> Result<Analysis, Error> {
  check_cancelled(cancelled)?;
  validate(&request)?;
  let workspace = AnalysisWorkspace::new(workspace, arena);
  let generation = workspace.generation.clone();
  let document = workspace
    .document(request.entry.document)
    .ok_or(Error::UnknownDocument(request.entry.document))?;
  if document.snapshot() != request.entry {
    return Err(Error::StaleSnapshot(request.entry));
  }
  let root = document.tree.root_node();
  let track_invariants =
    document.has_body_candidates() || !document.references().is_empty();
  let context = ExecutionContextId(0);
  let initial = Environment::new(&request.initial);
  let frame = Frame {
    snapshot: request.entry,
    context,
    loops: 0,
    dynamic_loops: false,
    returnable: false,
  };
  let mut index = Index::new(workspace.documents.values().copied(), cancelled)?;
  let readable = std::mem::take(&mut index.readable);
  let mut resolver = Resolver {
    workspace,
    inputs,
    error: None,
    sources: SourceTracking::new(request.entry),
    cancelled,
    index,
    contexts: BTreeMap::new(),
    gaps: BTreeSet::new(),
    invariants: InvariantInputs::default(),
    track_invariants,
    result: Analysis {
      argument_roles: BTreeMap::new(),
      argument_definitions: BTreeMap::new(),
      effects: BTreeMap::new(),
      queries: BTreeSet::new(),
      derived: DerivedDocuments::default(),
      attributes: BTreeMap::new(),
      generation,
      request,
      contexts: vec![ExecutionContext {
        id: context,
        parent: None,
        environment: context,
        kind: ExecutionKind::Entry,
        location: frame.location(root),
      }],
      uses: BTreeMap::new(),
      writes: BTreeSet::new(),
      temporary: BTreeMap::new(),
      gaps: Vec::new(),
      snapshots: Vec::new(),
      source_bindings: BTreeMap::new(),
      readable,
      references: BTreeMap::new(),
      source_inputs: BTreeMap::new(),
      source_operands: BTreeMap::new(),
      values: BTreeMap::new(),
    },
  };
  resolver.run(Task::eval(root, initial, frame))?;
  resolver.finish()
}

impl<'a> Resolver<'a> {
  fn finish(mut self) -> Result<Analysis, Error> {
    self.analyze_deferred()?;
    let mut derived = std::mem::take(&mut self.result.derived);
    let roots = self
      .result
      .contexts
      .iter()
      .map(|context| context.location.snapshot)
      .chain(
        derived
          .candidates
          .keys()
          .filter_map(|key| self.workspace.derived_keys.get(key).copied()),
      )
      .collect();
    derived.collect(
      self.workspace.workspace,
      roots,
      Some(&self.result),
      self.result.request.limits.value_bytes,
      self.cancelled,
    )?;
    self.result.derived = derived;
    self.collect_deferred_sources()?;
    self.finish_sources();
    if let Some(error) = self.error.take() {
      return Err(error);
    }
    self.commit_sources()?;
    for (key, site) in &mut self.result.uses {
      if matches!(site.site, UseSite::Argument { .. })
        && self
          .result
          .argument_roles
          .get(&(site.site, site.context))
          .is_none_or(|role| !role.is_reference())
      {
        site
          .resolution
          .issues
          .insert(ResolutionIssue::UnknownEffect);
      }
      check_cancelled(self.cancelled)?;
      for candidate in &site.resolution.candidates {
        self
          .result
          .references
          .entry(*candidate)
          .or_default()
          .insert(*key);
      }
    }
    check_cancelled(self.cancelled)?;
    Ok(self.result)
  }

  pub(in crate::interface::application::engine) fn ancestors(
    &self,
    frame: Frame,
  ) -> impl Iterator<Item = &ExecutionContext> {
    std::iter::successors(Some(frame.context), |id| {
      self.result.contexts[id.0].parent
    })
    .map(|id| &self.result.contexts[id.0])
  }

  pub(super) fn expansion_targets(
    &self,
    nodes: impl IntoIterator<Item = Node<'a>>,
    frame: Frame,
  ) -> BTreeSet<EffectTarget> {
    let mut pending: Vec<_> = nodes.into_iter().collect();
    let mut targets = BTreeSet::new();
    while let Some(node) = pending.pop() {
      match node.kind() {
        "command_substitution"
        | "backquote_substitution"
        | "subshell"
        | "function_definition" => continue,
        "arithmetic_dynamic_expression"
        | "parenthesized_arithmetic_dynamic_source" => {
          targets.insert(EffectTarget::Variables);
        }
        "io_here" => {
          pending.extend(
            self
              .index
              .heredocs
              .get(&(frame.snapshot, node.start_byte()))
              .copied(),
          );
        }
        _ => {}
      }
      if let Some(site) = self
        .site(node, frame)
        .and_then(|id| self.workspace.occurrence(id))
        && site.kind != OccurrenceKind::Command
        && site.access != Access::Read
      {
        targets.insert(match &site.name {
          engine::Value::Literal(name) => EffectTarget::Variable(name.clone()),
          engine::Value::Unknown(_) | engine::Value::Partial(_) => {
            EffectTarget::Variables
          }
        });
      }
      pending.extend(execution::children(node));
    }
    targets
  }

  pub(in crate::interface::application::engine) fn derive_body(
    &mut self,
    origin: DerivedOrigin,
    body: &[u8],
  ) -> DerivedKey {
    let (key, origin) = DerivedKey::new(origin, body, &self.workspace.derived);
    let candidate = DerivedCandidate {
      key: key.clone(),
      origin,
    };
    let result = self
      .result
      .derived
      .materialize(self.workspace.workspace, candidate, self.cancelled)
      .and_then(|snapshot| {
        if !self.workspace.documents.contains_key(&snapshot.document) {
          self.register_document(snapshot)?;
        }
        Ok(())
      });
    if let Err(error) = result {
      self.error = Some(error);
    }
    key
  }

  pub(super) fn input(
    &mut self,
    query: engine::InputQuery,
  ) -> engine::InputResult {
    if !self.result.request.inputs.contains_key(&query)
      && let Some(inputs) = &mut self.inputs
    {
      match inputs.evaluate(&query) {
        Ok(value) => {
          self.result.request.inputs.insert(query.clone(), value);
        }
        Err(error) => {
          self.error = Some(error);
        }
      }
    }
    let result = self
      .result
      .request
      .inputs
      .get(&query)
      .cloned()
      .unwrap_or(engine::InputResult::Unknown(engine::ValueIssue::Dynamic));
    self.result.queries.insert(query);
    result
  }

  pub(super) fn register_document(
    &mut self,
    snapshot: Snapshot,
  ) -> Result<(), Error> {
    let document = self.workspace.register(snapshot);
    self.index.add(document, self.cancelled)?;
    self.result.readable.append(&mut self.index.readable);
    Ok(())
  }

  pub(super) fn gap(
    &mut self,
    node: Node<'_>,
    frame: Frame,
    issue: ResolutionIssue,
  ) {
    let gap = AnalysisGap {
      context: frame.context,
      location: frame.location(node),
      issue,
    };
    if self.gaps.insert((
      frame.context,
      frame.snapshot,
      node.start_byte(),
      node.end_byte(),
      issue,
    )) {
      self.result.gaps.push(gap);
    }
  }

  pub(super) fn record_attribute(
    &mut self,
    id: OccurrenceId,
    context: ExecutionContextId,
    env: &Environment,
  ) {
    use engine::{AttributeState, Value};
    let state =
      self
        .workspace
        .occurrence(id)
        .map_or(AttributeState::Unknown, |site| {
          let Value::Literal(name) = &site.name else {
            return AttributeState::Unknown;
          };
          env.readonly_attribute(name)
        });
    self
      .result
      .attributes
      .entry((id, context))
      .and_modify(|old| *old = old.join(state))
      .or_insert(state);
  }

  pub(super) fn record(
    &mut self,
    id: OccurrenceId,
    context: ExecutionContextId,
    mut resolution: Resolution,
    environment: &Environment,
  ) {
    if self.workspace.occurrence(id).is_some_and(|site|
      site.kind != OccurrenceKind::Command && matches!(&site.name, engine::Value::Literal(name) if environment.uncertain_variable(name))) {
      resolution.issues.insert(ResolutionIssue::EvaluationOrder);
    }
    self.record_use(UseSite::Syntax(id), context, resolution);
  }

  pub(super) fn record_use(
    &mut self,
    site: UseSite,
    context: ExecutionContextId,
    resolution: Resolution,
  ) {
    self
      .result
      .uses
      .entry((site, context))
      .and_modify(|old| old.resolution.join(&resolution))
      .or_insert(ResolvedUse {
        site,
        context,
        resolution,
      });
  }

  pub(super) fn site(
    &self,
    node: Node<'_>,
    frame: Frame,
  ) -> Option<OccurrenceId> {
    self
      .index
      .sites
      .get(&(frame.snapshot, node.start_byte(), node.end_byte()))
      .copied()
  }

  pub(super) fn execution(
    &mut self,
    node: Node<'_>,
    frame: Frame,
    kind: ExecutionKind,
    snapshot: Snapshot,
    separate: bool,
  ) -> Frame {
    let boundary =
      matches!(kind, ExecutionKind::Function(_) | ExecutionKind::Source(_));
    let frame = Frame {
      loops: if separate || boundary { 0 } else { frame.loops },
      dynamic_loops: !separate
        && (frame.dynamic_loops || (boundary && frame.loops > 0)),
      returnable: boundary || frame.returnable,
      ..frame
    };
    let key = ContextKey {
      parent: frame.context,
      kind: kind.clone(),
      snapshot: frame.snapshot,
      start: node.start_byte(),
      end: node.end_byte(),
    };
    if let Some(context) = self.contexts.get(&key) {
      return Frame {
        snapshot,
        context: *context,
        ..frame
      };
    }
    let id = ExecutionContextId(self.result.contexts.len());
    let environment = if separate {
      id
    } else {
      self.result.contexts[frame.context.0].environment
    };
    self.result.contexts.push(ExecutionContext {
      id,
      parent: Some(frame.context),
      environment,
      kind,
      location: frame.location(node),
    });
    self.contexts.insert(key, id);
    Frame {
      snapshot,
      context: id,
      ..frame
    }
  }
}

pub(super) struct Index<'a> {
  pub sites: BTreeMap<(Snapshot, usize, usize), OccurrenceId>,
  pub definitions: BTreeMap<OccurrenceId, SymbolId>,
  pub functions: BTreeMap<SymbolId, Node<'a>>,
  pub heredocs: BTreeMap<(Snapshot, usize), Node<'a>>,
  pub sources: BTreeMap<(Snapshot, usize, usize), ReferenceId>,
  pub readable: BTreeSet<OccurrenceId>,
}

impl<'a> Index<'a> {
  pub fn new(
    documents: impl Iterator<Item = &'a engine::Document>,
    cancelled: &AtomicBool,
  ) -> Result<Self, Error> {
    let mut index = Self {
      sites: BTreeMap::new(),
      definitions: BTreeMap::new(),
      functions: BTreeMap::new(),
      heredocs: BTreeMap::new(),
      sources: BTreeMap::new(),
      readable: BTreeSet::new(),
    };
    for document in documents {
      index.add(document, cancelled)?;
    }
    Ok(index)
  }

  pub(super) fn add(
    &mut self,
    document: &'a engine::Document,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check_cancelled(cancelled)?;
    super::command::index_heredocs(document, &mut self.heredocs, cancelled)?;
    for reference in document.references() {
      self.sources.insert(
        (
          document.snapshot(),
          reference.command.bytes.start,
          reference.command.bytes.end,
        ),
        reference.id,
      );
    }
    for site in document.occurrences() {
      check_cancelled(cancelled)?;
      self.sites.insert(
        (
          document.snapshot(),
          site.location.bytes.start,
          site.location.bytes.end,
        ),
        site.id,
      );
      if !matches!(site.access, Access::Definition | Access::Write) {
        self.readable.insert(site.id);
      }
    }
    for symbol in document.symbols() {
      check_cancelled(cancelled)?;
      self.definitions.insert(symbol.definition, symbol.id);
      if symbol.kind == SymbolKind::Function
        && let Some(site) = document.occurrence(symbol.definition)
      {
        let mut node = document.tree.root_node().descendant_for_byte_range(
          site.location.bytes.start,
          site.location.bytes.end,
        );
        while let Some(current) = node {
          if current.kind() == "function_definition" {
            if let Some(body) = current.child_by_field_name("body") {
              self.functions.insert(symbol.id, body);
            }
            break;
          }
          node = current.parent();
        }
      }
    }
    Ok(())
  }
}
