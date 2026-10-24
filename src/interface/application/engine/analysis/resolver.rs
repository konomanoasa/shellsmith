use super::expansion::ArithmeticAccess;
use super::result::EffectKey;
use super::{deferred::InvariantInputs, execution, sources::SourceTracking};
use crate::interface::application::engine;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;

use engine::check_cancelled;
use engine::{
  Access, Error, Location, OccurrenceId, ReferenceId, Snapshot, SymbolId,
  SymbolKind, UseSite, Workspace,
};
use engine::{
  Analysis, AnalysisGap, AnalysisInputs, AnalysisRequest, DerivedOrigin,
  ExecutionContext, ExecutionContextId, ExecutionKind, Resolution,
  ResolutionIssue, ResolvedUse,
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
  const fn entry(snapshot: Snapshot) -> Self {
    Self {
      snapshot,
      context: ExecutionContextId(0),
      loops: 0,
      dynamic_loops: false,
      returnable: false,
    }
  }

  pub(super) fn location(self, node: Node<'_>) -> Location {
    Location {
      snapshot: self.snapshot,
      bytes: node.byte_range(),
    }
  }

  pub(super) fn effect_key(
    self,
    node: Node<'_>,
    issue: ResolutionIssue,
  ) -> EffectKey {
    (
      self.context,
      self.snapshot,
      node.start_byte(),
      node.end_byte(),
      issue,
    )
  }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ContextKey {
  parent: ExecutionContextId,
  kind: ExecutionKind,
  snapshot: Snapshot,
  start: usize,
  end: usize,
}

pub(super) struct AnalysisWorkspace<'a> {
  pub(super) workspace: &'a mut Workspace,
  arena: &'a typed_arena::Arena<Arc<engine::Document>>,
  documents: BTreeMap<engine::DocumentId, &'a engine::Document>,
}

impl<'a> AnalysisWorkspace<'a> {
  pub(super) fn new(
    workspace: &'a mut Workspace,
    arena: &'a typed_arena::Arena<Arc<engine::Document>>,
  ) -> Self {
    let documents = workspace
      .shared_documents()
      .map(|document| {
        let document: &'a engine::Document = arena.alloc(document);
        (document.snapshot().document, document)
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

  pub(super) fn source(&self, id: engine::DocumentId) -> &'a [u8] {
    self
      .document(id)
      .expect("analyzed document")
      .source()
      .as_bytes()
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

  fn register(&mut self, snapshot: Snapshot) -> Option<&'a engine::Document> {
    if self.documents.contains_key(&snapshot.document) {
      return None;
    }
    let document: &'a engine::Document = self.arena.alloc(
      self
        .workspace
        .shared_document(snapshot.document)
        .expect("workspace document"),
    );
    self.documents.insert(snapshot.document, document);
    Some(document)
  }
}

pub(super) struct Resolver<'a> {
  pub(super) workspace: AnalysisWorkspace<'a>,
  pub(super) inputs: &'a mut dyn AnalysisInputs,
  pub(super) error: Option<Error>,
  pub(super) sources: SourceTracking,
  pub(super) cancelled: &'a AtomicBool,
  pub(super) result: Analysis,
  pub(super) index: Index<'a>,
  contexts: BTreeMap<ContextKey, ExecutionContextId>,
  gaps: BTreeSet<EffectKey>,
  pub(super) invariants: InvariantInputs,
  pub(super) track_invariants: bool,
  pub(super) arithmetic: ArithmeticAccess,
}

pub(in crate::interface::application::engine) fn analyze<'a>(
  workspace: &'a mut Workspace,
  request: AnalysisRequest,
  arena: &'a typed_arena::Arena<Arc<engine::Document>>,
  inputs: &'a mut dyn AnalysisInputs,
  cancelled: &'a AtomicBool,
) -> Result<Analysis, Error> {
  check_cancelled(cancelled)?;
  let workspace = AnalysisWorkspace::new(workspace, arena);
  let mut resolver = Resolver::new(workspace, request, inputs, cancelled)?;
  let snapshot = resolver.result.request.entry;
  let root = resolver
    .workspace
    .document(snapshot.document)
    .expect("analysis entry")
    .tree
    .root_node();
  let initial = Environment::new(&resolver.result.request.initial);
  resolver.run(Task::eval(root, initial, Frame::entry(snapshot)))?;
  resolver.finish()
}

impl<'a> Resolver<'a> {
  pub(super) fn new(
    workspace: AnalysisWorkspace<'a>,
    request: AnalysisRequest,
    inputs: &'a mut dyn AnalysisInputs,
    cancelled: &'a AtomicBool,
  ) -> Result<Self, Error> {
    let document = workspace
      .document(request.entry.document)
      .expect("analysis entry");
    let root = document.tree.root_node();
    let track_invariants =
      document.has_body_candidates() || !document.references().is_empty();
    let frame = Frame::entry(request.entry);
    let mut readable = BTreeSet::new();
    let index = Index::new(
      workspace.documents.values().copied(),
      &mut readable,
      cancelled,
    )?;
    Ok(Self {
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
      arithmetic: ArithmeticAccess::default(),
      result: Analysis::new(
        request,
        ExecutionContext {
          id: frame.context,
          parent: None,
          environment: frame.context,
          kind: ExecutionKind::Entry,
          location: frame.location(root),
        },
        readable,
      ),
    })
  }
}

impl Resolver<'_> {
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
          .materialized(self.workspace.workspace)
          .map(|(_, snapshot)| snapshot),
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
    self.finish_sources();
    if let Some(error) = self.error.take() {
      return Err(error);
    }
    self.commit_sources()?;
    self.result.index_uses(self.cancelled)?;
    Ok(self.result)
  }

  pub(super) fn ancestors(
    &self,
    frame: Frame,
  ) -> impl Iterator<Item = &ExecutionContext> {
    self.result.context_ancestors(frame.context)
  }

  pub(super) fn derive_body(
    &mut self,
    origin: DerivedOrigin,
    body: &[u8],
  ) -> Option<Snapshot> {
    let snapshot = self
      .result
      .derived
      .derive(self.workspace.workspace, origin, body, self.cancelled)
      .map_err(|error| self.error = Some(error))
      .ok()?;
    if let Err(error) = self.register_document(snapshot) {
      self.error = Some(error);
    }
    Some(snapshot)
  }

  pub(super) fn input(
    &mut self,
    query: &engine::InputQuery,
  ) -> engine::InputResult {
    match self.inputs.evaluate(query) {
      Ok(result) => result,
      Err(error) => {
        self.error = Some(error);
        engine::InputResult::Unknown(engine::ValueIssue::Dynamic)
      }
    }
  }

  pub(super) fn register_document(
    &mut self,
    snapshot: Snapshot,
  ) -> Result<(), Error> {
    if let Some(document) = self.workspace.register(snapshot) {
      self
        .index
        .add(document, &mut self.result.readable, self.cancelled)?;
    }
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
    if self.gaps.insert(frame.effect_key(node, issue)) {
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

  pub(super) fn record_use(
    &mut self,
    site: UseSite,
    context: ExecutionContextId,
    mut resolution: Resolution,
    variable: Option<&[u8]>,
    env: &Environment,
  ) {
    if variable.is_some_and(|name| env.uncertain_variable(name)) {
      resolution.issues.insert(ResolutionIssue::EvaluationOrder);
    }
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
    self.index.site(frame.snapshot, node)
  }

  pub(super) fn execution(
    &mut self,
    node: Node<'_>,
    frame: Frame,
    kind: ExecutionKind,
    snapshot: Snapshot,
  ) -> Frame {
    let separate = kind.separate();
    let boundary =
      matches!(kind, ExecutionKind::Function(_) | ExecutionKind::Source(_));
    let frame = Frame {
      loops: if separate || boundary { 0 } else { frame.loops },
      dynamic_loops: !separate
        && (frame.dynamic_loops || (boundary && frame.loops > 0)),
      returnable: !matches!(kind, ExecutionKind::Deferred(_))
        && (boundary || frame.returnable),
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
  sites: BTreeMap<(Snapshot, usize, usize), OccurrenceId>,
  definitions: BTreeMap<OccurrenceId, SymbolId>,
  functions: BTreeMap<SymbolId, Node<'a>>,
  heredocs: BTreeMap<(Snapshot, usize), Node<'a>>,
  sources: BTreeMap<(Snapshot, usize, usize), ReferenceId>,
  damaged: BTreeSet<(Snapshot, usize)>,
}

impl<'a> Index<'a> {
  pub fn damaged(&self, snapshot: Snapshot, node: Node<'_>) -> bool {
    self.damaged.contains(&(snapshot, node.id()))
  }

  pub fn site(
    &self,
    snapshot: Snapshot,
    node: Node<'_>,
  ) -> Option<OccurrenceId> {
    self
      .sites
      .get(&(snapshot, node.start_byte(), node.end_byte()))
      .copied()
  }

  pub fn defined_symbol(&self, id: OccurrenceId) -> Option<SymbolId> {
    self.definitions.get(&id).copied()
  }

  pub fn function_body(&self, id: SymbolId) -> Option<Node<'a>> {
    self.functions.get(&id).copied()
  }

  pub fn heredoc(
    &self,
    snapshot: Snapshot,
    node: Node<'_>,
  ) -> Option<Node<'a>> {
    self.heredocs.get(&(snapshot, node.start_byte())).copied()
  }

  pub fn source(
    &self,
    snapshot: Snapshot,
    node: Node<'_>,
  ) -> Option<ReferenceId> {
    self
      .sources
      .get(&(snapshot, node.start_byte(), node.end_byte()))
      .copied()
  }

  pub fn new(
    documents: impl Iterator<Item = &'a engine::Document>,
    readable: &mut BTreeSet<OccurrenceId>,
    cancelled: &AtomicBool,
  ) -> Result<Self, Error> {
    let mut index = Self {
      sites: BTreeMap::new(),
      definitions: BTreeMap::new(),
      functions: BTreeMap::new(),
      heredocs: BTreeMap::new(),
      sources: BTreeMap::new(),
      damaged: BTreeSet::new(),
    };
    for document in documents {
      index.add(document, readable, cancelled)?;
    }
    Ok(index)
  }

  fn add(
    &mut self,
    document: &'a engine::Document,
    readable: &mut BTreeSet<OccurrenceId>,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check_cancelled(cancelled)?;
    super::command::index_heredocs(document, &mut self.heredocs, cancelled)?;
    self.index_damage(document, cancelled)?;
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
        readable.insert(site.id);
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

  fn index_damage(
    &mut self,
    document: &'a engine::Document,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    if !document.has_syntax_errors() {
      return Ok(());
    }
    let snapshot = document.snapshot();
    let mut pending = vec![(document.tree.root_node(), None)];
    while let Some((node, mut owner)) = pending.pop() {
      check_cancelled(cancelled)?;
      if matches!(node.kind(), "command" | "function_body")
        || engine::workspace::parser::subshell(node.kind())
      {
        owner = Some(node.id());
      }
      if engine::workspace::parser::damaged(node)
        || (matches!(node.kind(), "here_end" | "tilde_expansion")
          && node.has_error())
      {
        if let Some(owner) = owner {
          self.damaged.insert((snapshot, owner));
        }
        continue;
      }
      if matches!(
        node.kind(),
        "here_document" | "here_end" | "tilde_expansion"
      ) {
        continue;
      }
      if node.kind() == "io_here" {
        // Here-document bodies live outside their owning command's subtree.
        if let Some(body) = self.heredoc(snapshot, node) {
          pending.extend(
            body.children(&mut body.walk()).map(|child| (child, owner)),
          );
        } else if let Some(owner) = owner {
          self.damaged.insert((snapshot, owner));
        }
      }
      pending
        .extend(node.children(&mut node.walk()).map(|child| (child, owner)));
    }
    Ok(())
  }
}
