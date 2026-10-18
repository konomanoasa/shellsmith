use super::{
  Document,
  derived::{DerivedDocuments, DerivedKey, DerivedUnit, reserved},
};
use crate::interface::application::engine;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use engine::check_cancelled;
use engine::{Analysis, AnalysisRequest};
use engine::{
  Change, DependencyState, DocumentId, Error, ReferenceId, Revision, Snapshot,
  SourceReference, Value,
};
use engine::{Occurrence, OccurrenceId, Symbol, SymbolId};

#[derive(Clone, Debug, Default)]
pub struct Workspace {
  pub(in crate::interface::application::engine) documents:
    BTreeMap<DocumentId, Arc<Document>>,
  revisions: BTreeMap<DocumentId, Revision>,
  dependencies: BTreeMap<ReferenceId, DependencyState>,
  incoming: BTreeMap<DocumentId, BTreeSet<ReferenceId>>,
  symbols: BTreeMap<Vec<u8>, BTreeSet<SymbolId>>,
  pub(in crate::interface::application::engine) generation: Arc<()>,
  analysis_request: Option<AnalysisRequest>,
  analysis: Option<Arc<Analysis>>,
  pub(in crate::interface::application::engine) derived:
    BTreeMap<DocumentId, DerivedUnit>,
  pub(in crate::interface::application::engine) derived_keys:
    BTreeMap<DerivedKey, Snapshot>,
  pub(super) programs: BTreeMap<Arc<[u8]>, Arc<Document>>,
  pub(super) derived_entries: BTreeMap<DocumentId, BTreeSet<DocumentId>>,
  pub(super) indexed: BTreeMap<DocumentId, (usize, Arc<DerivedDocuments>)>,
  pub(super) next_derived: u64,
}

impl Workspace {
  pub fn clear_analysis(&mut self) {
    self.analysis_request = None;
    self.changed();
  }

  pub fn set_analysis_request(
    &mut self,
    request: AnalysisRequest,
  ) -> Result<(), Error> {
    self.check_current(request.entry)?;
    engine::analysis::validate(&request)?;
    if self.analysis_request.as_ref() != Some(&request) {
      self.analysis_request = Some(request);
      self.changed();
    }
    Ok(())
  }

  pub fn publish_analysis(&mut self, analysis: Analysis) -> Result<(), Error> {
    if !Arc::ptr_eq(&self.generation, &analysis.generation) {
      return Err(Error::StaleAnalysis);
    }
    self.analysis = Some(Arc::new(analysis));
    Ok(())
  }

  #[must_use]
  pub fn analysis(&self) -> Option<&Analysis> {
    self.analysis.as_deref()
  }

  pub(super) fn changed(&mut self) {
    self.generation = Arc::new(());
    self.analysis = None;
  }
  #[must_use]
  pub fn document(&self, id: DocumentId) -> Option<&Document> {
    self.documents.get(&id).map(AsRef::as_ref)
  }

  pub(crate) fn shared_documents(
    &self,
  ) -> impl Iterator<Item = Arc<Document>> + '_ {
    self.documents.values().cloned()
  }

  pub fn documents(&self) -> impl Iterator<Item = &Document> {
    self.documents.values().map(AsRef::as_ref)
  }

  pub fn symbols(&self) -> impl Iterator<Item = &Symbol> {
    self.documents().flat_map(Document::symbols)
  }

  #[must_use]
  pub fn symbol(&self, id: SymbolId) -> Option<&Symbol> {
    self.document(id.snapshot.document)?.symbol(id)
  }

  #[must_use]
  pub fn occurrence(&self, id: OccurrenceId) -> Option<&Occurrence> {
    self.document(id.snapshot.document)?.occurrence(id)
  }

  pub fn symbols_named(&self, name: &[u8]) -> impl Iterator<Item = &Symbol> {
    self
      .symbols
      .get(name)
      .into_iter()
      .flatten()
      .filter_map(|id| self.symbol(*id))
  }

  pub fn search_symbols<'a>(
    &'a self,
    query: &'a [u8],
  ) -> impl Iterator<Item = &'a Symbol> {
    self
      .symbols
      .iter()
      .filter(move |(name, _)| {
        query.is_empty() || name.windows(query.len()).any(|part| part == query)
      })
      .flat_map(|(_, ids)| ids)
      .filter_map(|id| self.symbol(*id))
  }

  /// Failed updates leave the committed document unchanged.
  pub fn update(
    &mut self,
    snapshot: Snapshot,
    source: &[u8],
    cancelled: &AtomicBool,
  ) -> Result<Change, Error> {
    self.check_new(snapshot)?;
    let document = Document::parse(snapshot, source, cancelled)?;
    check_cancelled(cancelled)?;
    self.insert(document)
  }

  /// Revisions must exceed all previously committed revisions, including deleted ones.
  pub fn insert(&mut self, document: Document) -> Result<Change, Error> {
    self.insert_shared(Arc::new(document))
  }

  pub fn insert_shared(
    &mut self,
    document: Arc<Document>,
  ) -> Result<Change, Error> {
    if reserved(document.snapshot().document) {
      return Err(Error::InvalidAnalysis(
        "Document ID is reserved for derived documents".into(),
      ));
    }
    self.insert_arc(document)
  }

  pub(super) fn insert_document(
    &mut self,
    document: Document,
  ) -> Result<Change, Error> {
    self.insert_arc(Arc::new(document))
  }

  fn insert_arc(&mut self, document: Arc<Document>) -> Result<Change, Error> {
    let snapshot = document.snapshot();
    self.check_new(snapshot)?;
    let affected = self.affected(snapshot.document);
    self.discard(snapshot.document);
    for symbol in document.symbols() {
      self
        .symbols
        .entry(symbol.name.clone())
        .or_default()
        .insert(symbol.id);
    }
    for reference in document.references() {
      let state = match &reference.value {
        Value::Literal(_) => DependencyState::Pending,
        value @ (Value::Unknown(_) | Value::Partial(_)) => {
          let issue = value.issue().expect("unknown reference");
          DependencyState::Unresolved(format!("{issue:?}"))
        }
      };
      self.dependencies.insert(reference.id, state);
    }
    self.revisions.insert(snapshot.document, snapshot.revision);
    self.documents.insert(snapshot.document, document);
    self.changed();
    Ok(Change { affected })
  }

  pub fn remove(&mut self, snapshot: Snapshot) -> Result<Change, Error> {
    if reserved(snapshot.document) {
      return Err(Error::InvalidAnalysis(
        "Derived documents are managed by analysis".into(),
      ));
    }
    self.check_current(snapshot)?;
    let affected = self.affected(snapshot.document);
    self.discard(snapshot.document);
    self.changed();
    Ok(Change { affected })
  }

  /// Rejected references leave the dependency graph unchanged.
  pub fn set_reference(
    &mut self,
    id: ReferenceId,
    state: DependencyState,
  ) -> Result<Change, Error> {
    self.check_current(id.snapshot)?;
    if !self.dependencies.contains_key(&id) {
      return Err(Error::UnknownReference(id));
    }
    if let DependencyState::Resolved(snapshot) = &state {
      self.check_current(*snapshot)?;
    }
    let affected = self.affected(id.snapshot.document);
    self.disconnect(id);
    if let Some(target) = state.target() {
      self.incoming.entry(target).or_default().insert(id);
    }
    self.dependencies.insert(id, state);
    self.changed();
    Ok(Change { affected })
  }

  #[must_use]
  pub fn reference_state(&self, id: ReferenceId) -> Option<&DependencyState> {
    self.dependencies.get(&id)
  }

  pub fn dependencies(
    &self,
    id: DocumentId,
  ) -> impl Iterator<Item = (&SourceReference, &DependencyState)> {
    self
      .documents
      .get(&id)
      .into_iter()
      .flat_map(|document| document.references())
      .filter_map(|reference| {
        self
          .dependencies
          .get(&reference.id)
          .map(|state| (reference, state))
      })
  }

  #[must_use]
  pub fn dependents(&self, id: DocumentId) -> Vec<ReferenceId> {
    self
      .incoming
      .get(&id)
      .into_iter()
      .flatten()
      .copied()
      .collect()
  }

  #[must_use]
  pub fn reachable(&self, root: DocumentId) -> Vec<DocumentId> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
      if self.documents.contains_key(&id) && seen.insert(id) {
        pending.extend(
          self
            .derived
            .iter()
            .filter(|(_, unit)| unit.origin.argument.snapshot.document == id)
            .map(|(child, _)| *child),
        );
        pending.extend(
          self
            .dependencies(id)
            .filter_map(|(_, state)| state.target()),
        );
      }
    }
    seen.into_iter().collect()
  }

  fn check_new(&self, snapshot: Snapshot) -> Result<(), Error> {
    if self
      .revisions
      .get(&snapshot.document)
      .is_some_and(|revision| *revision >= snapshot.revision)
    {
      Err(Error::StaleSnapshot(snapshot))
    } else {
      Ok(())
    }
  }

  fn check_current(&self, snapshot: Snapshot) -> Result<(), Error> {
    match self.document(snapshot.document) {
      Some(document) if document.snapshot() == snapshot => Ok(()),
      Some(_) => Err(Error::StaleSnapshot(snapshot)),
      None => Err(Error::UnknownDocument(snapshot.document)),
    }
  }

  fn affected(&self, root: DocumentId) -> Vec<DocumentId> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
      if seen.insert(id) {
        if let Some(origin) = self.derived_origin(id) {
          pending.push(origin.argument.snapshot.document);
        }
        pending.extend(
          self
            .derived
            .iter()
            .filter(|(_, unit)| unit.origin.argument.snapshot.document == id)
            .map(|(child, _)| *child),
        );
        pending.extend(
          self
            .dependents(id)
            .into_iter()
            .map(|reference| reference.snapshot.document),
        );
      }
    }
    seen.into_iter().collect()
  }

  fn disconnect(&mut self, id: ReferenceId) {
    if let Some(target) = self
      .dependencies
      .remove(&id)
      .and_then(|state| state.target())
      && let Some(incoming) = self.incoming.get_mut(&target)
    {
      incoming.remove(&id);
      if incoming.is_empty() {
        self.incoming.remove(&target);
      }
    }
  }

  pub(super) fn discard(&mut self, id: DocumentId) {
    let mut pending = vec![id];
    while let Some(parent) = pending.pop() {
      let children: Vec<_> = self
        .derived
        .iter()
        .filter(|(_, unit)| unit.origin.argument.snapshot.document == parent)
        .map(|(id, _)| *id)
        .collect();
      pending.extend(children);
      if let Some(unit) = self.derived.remove(&parent) {
        self.derived_keys.remove(&unit.key);
        if !self
          .derived
          .values()
          .any(|other| other.key.body == unit.key.body)
        {
          self.programs.remove(&unit.key.body);
        }
        for keys in self.derived_entries.values_mut() {
          keys.remove(&parent);
        }
      }
      self.derived_entries.remove(&parent);
      self.indexed.remove(&parent);
      self.discard_document(parent);
      self.invalidate(parent);
    }
  }

  fn discard_document(&mut self, id: DocumentId) {
    if reserved(id) {
      self.revisions.remove(&id);
    }
    if let Some(document) = self.documents.remove(&id) {
      for symbol in document.symbols() {
        if let Some(ids) = self.symbols.get_mut(&symbol.name) {
          ids.remove(&symbol.id);
          if ids.is_empty() {
            self.symbols.remove(&symbol.name);
          }
        }
      }
      for reference in document.references() {
        self.disconnect(reference.id);
      }
    }
  }

  fn invalidate(&mut self, target: DocumentId) {
    for reference in self.dependents(target) {
      self
        .dependencies
        .insert(reference, DependencyState::Invalidated(target));
    }
  }
}

impl Workspace {
  pub fn analyze(&mut self, cancelled: &AtomicBool) -> Result<Analysis, Error> {
    let request = self.analysis_request.clone().ok_or_else(|| {
      Error::InvalidAnalysis("No analysis request configured".into())
    })?;
    self.analyze_with_inputs(request, None, cancelled)
  }

  pub fn analyze_with(
    &mut self,
    request: super::super::AnalysisRequest,
    inputs: &mut dyn super::super::AnalysisInputs,
    cancelled: &AtomicBool,
  ) -> Result<Analysis, Error> {
    self.analyze_with_inputs(request, Some(inputs), cancelled)
  }

  fn analyze_with_inputs(
    &mut self,
    request: super::super::AnalysisRequest,
    inputs: Option<&mut dyn super::super::AnalysisInputs>,
    cancelled: &AtomicBool,
  ) -> Result<Analysis, Error> {
    let mut staged = self.clone();
    staged.index_document(request.entry.document, request.limits, cancelled)?;
    staged.set_analysis_request(request.clone())?;
    let arena = typed_arena::Arena::new();
    let mut result = super::super::analysis::analyze(
      &mut staged,
      request,
      &arena,
      inputs.map(|input| input as &mut dyn super::super::AnalysisInputs),
      cancelled,
    )?;
    staged.prune_derived(
      result.request().entry.document,
      &result.derived.candidates,
    );
    result.synchronize(&staged);
    check_cancelled(cancelled)?;
    staged.analysis = None;
    *self = staged;
    Ok(result)
  }
}
