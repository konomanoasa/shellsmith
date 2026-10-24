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
  pub(super) collected: Instant,
  pub(super) selection: InputSelection,
  failures: Vec<Failure>,
  paths: BTreeMap<PathBuf, Snapshot>,
  cache: InputCache,
}

impl Application {
  pub(super) fn prepare_workspace_analysis(
    &mut self,
    cancelled: &AtomicBool,
  ) -> Result<Arc<AnalysisCollection>, Error> {
    self.collect_analysis(BTreeSet::new(), cancelled)
  }

  fn collect_analysis(
    &mut self,
    explicit: BTreeSet<PathBuf>,
    cancelled: &AtomicBool,
  ) -> Result<Arc<AnalysisCollection>, Error> {
    let selection = self.select_inputs(explicit, cancelled)?;
    let mut entries = BTreeMap::new();
    for path in &selection.paths {
      check_cancelled(cancelled)?;
      if self.cache.loaded.get(path).is_some_and(Result::is_ok) {
        entries.insert(path.clone(), self.analyze_entry(path, cancelled)?);
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
    self.analyses = Some(collection.clone());
    Ok(collection)
  }

  pub(in crate::interface::application) fn prepare_analysis(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<Arc<AnalysisCollection>, Error> {
    let path = self.absolute(path);
    if let Some(collection) = self.analyses.clone()
      && self.analysis_inputs_current(&collection, cancelled)?
    {
      self.share_analyses(collection);
    } else {
      self.verify_discovery(cancelled)?;
      self.refresh_for(None, cancelled)?;
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
    self.verify_discovery(cancelled)?;
    if self.select_inputs(collection.selection.explicit.clone(), cancelled)?
      != collection.selection
    {
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
    if self.workspace.analysis().is_some_and(|result| {
      result.request().entry == snapshot && result.request().initial == initial
    }) {
      return Ok(());
    }
    self.share_workspace_documents();
    let mut request = AnalysisRequest::new(snapshot);
    request.initial = initial;
    request.limits = self.settings.limits;
    let mut inputs = Inputs {
      opened: &self.opened,
      documents: &self.documents,
      entries: &mut self.entries,
      failures: &mut self.failures,
      cache: &mut self.cache,
      cancelled,
    };
    let result = self.workspace.analyze(request, &mut inputs, cancelled)?;
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
