use super::{
  Application, Error, Failure,
  application::InputSelection,
  inputs::{InputCache, Inputs},
};
use crate::interface::application::engine::{
  self, AnalysisRequest, DocumentId, InitialBinding, Snapshot, Workspace,
  check_cancelled,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Clone, Debug)]
pub(in crate::interface::application) struct AnalysisCollection {
  pub entries: BTreeMap<PathBuf, Workspace>,
  collected: Instant,
  pub(super) selection: InputSelection,
  failures: Vec<Failure>,
  paths: BTreeMap<PathBuf, Snapshot>,
  cache: InputCache,
}

impl AnalysisCollection {
  pub(super) fn supersedes(&self, previous: Option<&Self>) -> bool {
    previous.is_none_or(|previous| previous.collected < self.collected)
  }
}

impl Application {
  pub(super) fn prepare_workspace_analysis(
    &mut self,
    cancelled: &AtomicBool,
  ) -> Result<Arc<AnalysisCollection>, Error> {
    match self.collect_analysis(BTreeSet::new(), cancelled) {
      Err(Error::Superseded) => {
        self.refresh_for(None, cancelled)?;
        self.outdated = true;
        self.collect_analysis(BTreeSet::new(), cancelled)
      }
      result => result,
    }
  }

  /// Fails with `Superseded` when a selected input was not loaded or the
  /// collected inputs changed.
  pub(super) fn collect_analysis(
    &mut self,
    explicit: BTreeSet<PathBuf>,
    cancelled: &AtomicBool,
  ) -> Result<Arc<AnalysisCollection>, Error> {
    let selection = self.select_inputs(explicit, cancelled)?;
    let mut entries = BTreeMap::new();
    for path in &selection.paths {
      check_cancelled(cancelled)?;
      match self.cache.loaded.get(path) {
        Some(Ok(_)) => {
          entries.insert(path.clone(), self.analyze_entry(path, cancelled)?);
        }
        Some(Err(_)) => {}
        None => return Err(Error::Superseded),
      }
    }
    let collection = AnalysisCollection {
      entries,
      collected: Instant::now(),
      selection,
      failures: self.failures.clone(),
      paths: self.entries.clone(),
      cache: self.cache.clone(),
    };
    if !self.analysis_inputs_current(&collection, cancelled)? {
      return Err(Error::Superseded);
    }
    let collection = Arc::new(collection);
    self.names = None;
    self.analyses = Some(collection.clone());
    Ok(collection)
  }

  pub(in crate::interface::application) fn prepare_analysis(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<Arc<AnalysisCollection>, Error> {
    let path = self.absolute(path);
    if self.request_prepared {
      self.check_inputs(cancelled)?;
      return self
        .analyses
        .clone()
        .filter(|collection| collection.entries.contains_key(&path))
        .ok_or_else(|| {
          Error::Message(
            "Workspace results were not prepared for this request".into(),
          )
        });
    }
    match self.analyses.clone() {
      Some(collection)
        if self.analysis_inputs_current(&collection, cancelled)? =>
      {
        self.share_analyses(collection);
      }
      Some(_) => {
        self.refresh_for(None, cancelled)?;
        self.outdated = true;
      }
      None => {
        self.verify_discovery(cancelled)?;
        self.refresh_for(None, cancelled)?;
      }
    }
    if self
      .analyses
      .as_ref()
      .is_none_or(|collection| !collection.entries.contains_key(&path))
    {
      if !self.cache.loaded.get(&path).is_some_and(Result::is_ok) {
        self.load(&path, cancelled)?;
      }
      let mut explicit = self
        .analyses
        .as_ref()
        .map(|collection| collection.selection.explicit.clone())
        .unwrap_or_default();
      explicit.insert(path);
      self.collect_analysis(explicit, cancelled)?;
    }
    self.restore_analysis_documents(cancelled)?;
    Ok(self.analyses.clone().expect("completed workspace analysis"))
  }

  pub(in crate::interface::application) fn prepare_position(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<Arc<AnalysisCollection>, Error> {
    if self.request_prepared {
      return self.prepare_analysis(path, cancelled);
    }
    let previous = self
      .document_id(path)
      .and_then(|id| self.workspace.shared_document(id));
    let mut prepared = self.clone();
    let result = prepared.prepare_analysis(path, cancelled);
    if let Some(previous) = previous
      && prepared
        .document_id(path)
        .and_then(|id| prepared.workspace.document(id))
        .is_none_or(|document| document.source() != previous.source())
    {
      self.discard_results();
      return Err(Error::Superseded);
    }
    *self = prepared;
    result
  }

  pub(super) fn restore_analysis_documents(
    &mut self,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let documents: Vec<_> = self
      .cache
      .loaded
      .values()
      .filter_map(|loaded| loaded.as_ref().ok())
      .cloned()
      .collect();
    for document in documents {
      check_cancelled(cancelled)?;
      let id = document.snapshot().document;
      if self
        .workspace
        .document(id)
        .is_none_or(|current| current.snapshot() != document.snapshot())
      {
        self.workspace.insert_shared(document)?;
        self
          .workspace
          .index_document(id, self.settings.limits, cancelled)?;
      }
    }
    Ok(())
  }

  pub(super) fn share_analyses(&mut self, collection: Arc<AnalysisCollection>) {
    if self
      .analyses
      .as_ref()
      .is_none_or(|previous| !Arc::ptr_eq(previous, &collection))
    {
      self.names = None;
    }
    self.entries.extend(collection.paths.clone());
    self.cache = collection.cache.clone();
    self.failures.clone_from(&collection.failures);
    self.analyses = Some(collection);
  }

  pub(super) fn analyze_entry(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<Workspace, Error> {
    let document = self
      .cache
      .loaded
      .get(path)
      .and_then(|loaded| loaded.as_ref().ok())
      .cloned()
      .ok_or_else(|| {
        Error::Message(format!("Cannot load {}", path.display()))
      })?;
    if let Some(workspace) = self
      .analyses
      .as_ref()
      .and_then(|analyses| analyses.entries.get(path))
      && workspace
        .analysis()
        .is_some_and(|analysis| analysis.request().entry == document.snapshot())
    {
      return Ok(workspace.clone());
    }
    let previous = std::mem::take(&mut self.workspace);
    let id = document.snapshot().document;
    let result = self
      .workspace
      .insert_shared(document)
      .map_err(Error::from)
      .and_then(|_| self.analyze_document(id, cancelled));
    let workspace = std::mem::replace(&mut self.workspace, previous);
    result?;
    Ok(workspace)
  }

  pub(in crate::interface::application) fn analysis_inputs_current(
    &self,
    collection: &AnalysisCollection,
    cancelled: &AtomicBool,
  ) -> Result<bool, Error> {
    if !self.selection_current(&collection.selection, cancelled)? {
      return Ok(false);
    }
    for (path, current) in &self.cache.loaded {
      if let (Ok(current), Some(Ok(previous))) =
        (current, collection.cache.loaded.get(path))
        && current.snapshot() != previous.snapshot()
      {
        return Ok(false);
      }
    }
    collection
      .cache
      .current(&self.opened, &collection.paths, cancelled)
  }

  pub(in crate::interface::application) fn inputs_current(
    &self,
    cancelled: &AtomicBool,
  ) -> Result<bool, Error> {
    if let Some(analyses) = &self.analyses {
      return self.analysis_inputs_current(analyses, cancelled);
    }
    if let Some(selection) = &self.cache.selection
      && !self.selection_current(selection, cancelled)?
    {
      return Ok(false);
    }
    self.cache.current(&self.opened, &self.entries, cancelled)
  }

  pub(in crate::interface::application) fn check_inputs(
    &mut self,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check_cancelled(cancelled)?;
    if !self.inputs_current(cancelled)? {
      self.discard_results();
      return Err(Error::Superseded);
    }
    Ok(())
  }
}

impl Application {
  pub(in crate::interface::application) fn analyze_document(
    &mut self,
    id: DocumentId,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check_cancelled(cancelled)?;
    let snapshot = self
      .workspace
      .document(id)
      .ok_or(engine::Error::UnknownDocument(id))?
      .snapshot();
    let initial = self.initial_environment(id)?;
    let mut request = AnalysisRequest::new(snapshot);
    request.initial = initial.clone();
    request.limits = self.settings.limits;
    if self.limited.as_ref() == Some(&request) {
      return Err(engine::Error::AnalysisLimit.into());
    }
    if self
      .workspace
      .analysis()
      .is_some_and(|result| result.request() == &request)
    {
      return Ok(());
    }
    self.share_workspace_documents();

    let mut inputs = Inputs {
      opened: &self.opened,
      documents: &self.documents,
      entries: &mut self.entries,
      failures: &mut self.failures,
      cache: &mut self.cache,
      cancelled,
    };
    let result =
      match self
        .workspace
        .analyze(request.clone(), &mut inputs, cancelled)
      {
        Err(engine::Error::AnalysisLimit) => {
          self.clear_semantics();
          self.limited = Some(request);
          return Err(engine::Error::AnalysisLimit.into());
        }
        result => result?,
      };
    self.limited = None;
    self.workspace.publish_analysis(result)?;
    self.failures.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
  }

  fn initial_environment(
    &self,
    id: DocumentId,
  ) -> Result<engine::InitialEnvironment, Error> {
    let cwd = self
      .path(id)
      .and_then(Path::parent)
      .ok_or(engine::Error::UnknownDocument(id))?;
    let mut initial = self.settings.initial.clone();
    initial.cwd = Some(cwd.as_os_str().as_encoded_bytes().to_vec());
    initial
      .values
      .entry(b"PWD".to_vec())
      .or_insert_with(|| cwd.as_os_str().as_encoded_bytes().to_vec());
    for (name, binding) in [
      (b"LINENO".as_slice(), InitialBinding::External),
      (b"PPID", InitialBinding::External),
      (b"PS1", InitialBinding::Unknown),
      (b"PS2", InitialBinding::Unknown),
      (b"PS4", InitialBinding::Unknown),
    ] {
      if !initial.environment.contains_key(name)
        && !initial.values.contains_key(name)
      {
        initial.variables.entry(name.to_vec()).or_insert(binding);
      }
    }
    Ok(initial)
  }
}
