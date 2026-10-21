use super::{collection::Collection, files, names::NameCollection};
use crate::interface::application::engine;
use engine::{
  AnalysisInputs, AnalysisRequest, Document, InputQuery, InputResult, Snapshot,
  SourceDocument, SourceInput, check_cancelled,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use engine::{
  AnalysisLimits, DocumentId, InitialBinding, InitialEnvironment, Workspace,
};

#[derive(Clone, Debug)]
pub struct Settings {
  pub cwd: PathBuf,
  pub initial: InitialEnvironment,
  pub limits: AnalysisLimits,
}

impl Settings {
  #[must_use]
  pub fn new(cwd: PathBuf) -> Self {
    Self {
      cwd,
      initial: InitialEnvironment {
        environment: std::env::vars_os()
          .filter_map(|(name, value)| {
            let name = name.as_encoded_bytes();
            (engine::valid_name(name)
              && !matches!(
                name,
                b"IFS" | b"LINENO" | b"OPTIND" | b"PPID" | b"PWD"
              ))
            .then(|| (name.to_vec(), value.as_encoded_bytes().to_vec()))
          })
          .collect(),
        values: BTreeMap::from([
          (b"IFS".to_vec(), b" \t\n".to_vec()),
          (b"OPTIND".to_vec(), b"1".to_vec()),
        ]),
        positional: None,
        ..InitialEnvironment::empty()
      },
      limits: AnalysisLimits::default(),
    }
  }

  fn checked(self) -> Result<Self, Error> {
    if self.cwd.is_absolute() {
      Ok(self)
    } else {
      Err(Error::Message("Working directory must be absolute".into()))
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Failure {
  pub path: PathBuf,
  pub message: String,
}

#[derive(Clone, Debug)]
pub enum Error {
  Message(String),
  InvalidName(String),
  RenameRefused(String),
  Engine(engine::Error),
  Superseded,
}

impl std::fmt::Display for Error {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Message(message)
      | Self::InvalidName(message)
      | Self::RenameRefused(message) => f.write_str(message),
      Self::Engine(error) => error.fmt(f),
      Self::Superseded => f.write_str("Analysis inputs changed"),
    }
  }
}
impl std::error::Error for Error {}
impl From<engine::Error> for Error {
  fn from(error: engine::Error) -> Self {
    Self::Engine(error)
  }
}
impl From<std::io::Error> for Error {
  fn from(error: std::io::Error) -> Self {
    Self::Message(error.to_string())
  }
}

#[derive(Clone, Debug, Default)]
pub(super) struct InputCache {
  pub(super) loaded: BTreeMap<PathBuf, Result<Arc<Document>, String>>,
  pub(super) paths: BTreeMap<SourceInput, Result<PathBuf, String>>,
  pub(super) evaluated: BTreeMap<engine::InputQuery, engine::InputResult>,
  pub(super) frozen: bool,
}

#[derive(Clone, Debug)]
pub struct Application {
  pub(super) settings: Settings,
  pub(in crate::interface::application) workspace: Workspace,
  entries: BTreeMap<PathBuf, Snapshot>,
  pub(super) opened: BTreeMap<PathBuf, Arc<str>>,
  inputs: BTreeSet<PathBuf>,
  pub(super) failures: Vec<Failure>,
  documents: Arc<files::Documents>,
  pub(super) collection: Arc<Collection>,
  pub(super) cache: InputCache,
  pub(super) names: Option<Arc<NameCollection>>,
}

impl Application {
  pub fn new(settings: Settings) -> Result<Self, Error> {
    Ok(Self {
      settings: settings.checked()?,
      workspace: engine::Workspace::default(),
      entries: BTreeMap::new(),
      opened: BTreeMap::new(),
      inputs: BTreeSet::new(),
      failures: Vec::new(),
      documents: Arc::default(),
      collection: Arc::default(),
      cache: InputCache::default(),
      names: None,
    })
  }

  #[must_use]
  pub const fn workspace(&self) -> &Workspace {
    &self.workspace
  }
  #[must_use]
  pub const fn settings(&self) -> &Settings {
    &self.settings
  }
  pub fn configure(&mut self, settings: Settings) -> Result<(), Error> {
    self.settings = settings.checked()?;
    self.invalidate_inputs();
    Ok(())
  }
  fn invalidate_inputs(&mut self) {
    self.cache = InputCache::default();
    self.names = None;
    self.workspace.clear_analysis();
  }

  #[must_use]
  pub fn failures(&self) -> &[Failure] {
    &self.failures
  }
  #[must_use]
  pub fn path(&self, id: DocumentId) -> Option<&Path> {
    files::entry_path(&self.entries, id)
  }
  #[must_use]
  pub fn document_id(&self, path: &Path) -> Option<DocumentId> {
    self
      .entries
      .get(&self.absolute(path))
      .map(|entry| entry.document)
      .filter(|id| self.workspace.document(*id).is_some())
  }
  #[must_use]
  pub fn absolute(&self, path: &Path) -> PathBuf {
    files::absolute(&self.settings.cwd, path)
  }

  /// The identity of a document whose path names a regular file or nothing.
  pub(super) fn document_path(&self, path: &Path) -> Result<PathBuf, Error> {
    files::ordinary(&self.settings.cwd.join(path), true)?;
    Ok(self.absolute(path))
  }

  /// Records or discards the unsaved text of the document `path`.
  pub(super) fn set_overlay(
    &mut self,
    path: PathBuf,
    source: Option<Arc<str>>,
  ) {
    self.invalidate_inputs();
    if let Some(source) = source {
      self.opened.insert(path, source);
    } else {
      self.opened.remove(&path);
      self.collection.changed(std::slice::from_ref(&path));
    }
  }

  pub(super) fn resolve_source(
    &self,
    input: &SourceInput,
  ) -> Result<PathBuf, String> {
    files::resolve(input, &self.opened, &self.entries)
  }

  /// Inputs keep the given path and follow its symbolic links at each
  /// collection. Returns whether the inputs changed.
  pub fn add_input(&mut self, path: &Path) -> bool {
    let added = self.inputs.insert(self.settings.cwd.join(path));
    if added {
      self.names = None;
    }
    added
  }

  pub fn remove_input(&mut self, path: &Path) -> bool {
    let removed = self.inputs.remove(&self.settings.cwd.join(path));
    if removed {
      self.names = None;
      self.collection.remove(&self.absolute(path));
    }
    removed
  }

  pub fn files_changed(&mut self, paths: &[PathBuf]) {
    let paths: Vec<_> = paths
      .iter()
      .map(|path| self.settings.cwd.join(path))
      .collect();
    self.collection.changed(&paths);
    self.invalidate_inputs();
  }

  pub fn open(
    &mut self,
    path: &Path,
    source: &str,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let path = self.document_path(path)?;
    check_cancelled(cancelled)?;
    self.set_overlay(path.clone(), Some(Arc::from(source)));
    self.replace(&path, source.as_bytes(), cancelled)
  }

  pub fn change(
    &mut self,
    path: &Path,
    source: &str,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let path = self.absolute(path);
    if !self.opened.contains_key(&path) {
      return Err(Error::Message("Document is not open".into()));
    }
    check_cancelled(cancelled)?;
    self.set_overlay(path.clone(), Some(Arc::from(source)));
    self.replace(&path, source.as_bytes(), cancelled)
  }

  /// Discards the overlay even if reloading from disk fails.
  pub fn close(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check_cancelled(cancelled)?;
    let path = self.absolute(path);
    self.set_overlay(path.clone(), None);
    self.load(&path, cancelled)
  }

  fn discard(&mut self, path: &Path) {
    if let Some(entry) = self.entries.get(path)
      && let Some(document) = self.workspace.document(entry.document)
    {
      let _ = self.workspace.remove(document.snapshot());
    }
  }

  pub(super) fn replace(
    &mut self,
    path: &Path,
    bytes: &[u8],
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check_cancelled(cancelled)?;
    if let Some(id) = self.document_id(path)
      && self
        .workspace
        .document(id)
        .is_some_and(|document| document.source().as_bytes() == bytes)
    {
      self
        .workspace
        .index_document(id, self.settings.limits, cancelled)?;
      return Ok(());
    }
    let result = self
      .documents
      .parse_entry(&mut self.entries, path, bytes, cancelled)
      .and_then(|document| {
        let snapshot = document.snapshot();
        self.workspace.insert_shared(document.clone())?;
        self.workspace.index_document(
          snapshot.document,
          self.settings.limits,
          cancelled,
        )?;
        self.cache.loaded.insert(path.to_path_buf(), Ok(document));
        Ok(())
      });
    if result.is_err() {
      self.discard(path);
    }
    result
  }

  /// Makes `bytes` the text that analyses load for `path`, without adding it
  /// to the workspace.
  pub(super) fn stage(
    &mut self,
    path: &Path,
    bytes: &[u8],
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let document =
      self
        .documents
        .parse_entry(&mut self.entries, path, bytes, cancelled)?;
    self.cache.loaded.insert(path.to_path_buf(), Ok(document));
    Ok(())
  }

  pub(super) fn load(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let bytes = files::read(path, &self.opened);
    match bytes {
      Ok(bytes) => self.replace(path, &bytes, cancelled),
      Err(error) => {
        self.discard(path);
        Err(error)
      }
    }
  }

  /// Individual file errors go to `failures`; they do not fail the whole refresh.
  pub fn refresh(&mut self, cancelled: &AtomicBool) -> Result<(), Error> {
    self.collection.invalidate();
    let selected = self.refresh_for(None, cancelled)?;
    self.analyze_entries(&selected, cancelled)
  }

  /// Analyzes the loaded `entries` and keeps the source targets that all of
  /// them agree on.
  fn analyze_entries(
    &mut self,
    entries: &BTreeSet<PathBuf>,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let mut states: BTreeMap<_, engine::DependencyState> = BTreeMap::new();
    for path in entries {
      let Some(id) = self.document_id(path) else {
        continue;
      };
      self.analyze_document(id, cancelled)?;
      let result = self.workspace.analysis().expect("completed analysis");
      let references: BTreeSet<_> =
        result.source_inputs().map(|(id, _, _)| id).collect();
      for reference in references {
        if let Some(state) = result.source_bindings().get(&reference) {
          states
            .entry(reference)
            .and_modify(|old| {
              if old != state {
                *old = engine::DependencyState::Unresolved(
                  "Source target differs between entries".into(),
                );
              }
            })
            .or_insert_with(|| state.clone());
        }
      }
    }
    for (reference, state) in states {
      if self.workspace.reference_state(reference) != Some(&state) {
        self.workspace.set_reference(reference, state)?;
      }
    }
    Ok(check_cancelled(cancelled)?)
  }

  /// Reloads the inputs of `document`, or of the workspace when it is absent,
  /// returning the selected entries.
  pub(super) fn refresh_for(
    &mut self,
    document: Option<&Path>,
    cancelled: &AtomicBool,
  ) -> Result<BTreeSet<PathBuf>, Error> {
    check_cancelled(cancelled)?;
    self.failures.clear();
    self.names = None;
    self.cache = InputCache::default();
    let selected = if let Some(path) = document {
      BTreeSet::from([self.absolute(path)])
    } else {
      let (selected, failures) =
        self.collect_inputs(BTreeSet::new(), cancelled)?;
      self.failures = failures;
      selected
    };
    for path in &selected {
      if let Err(error) = self.load(path, cancelled) {
        check_cancelled(cancelled)?;
        self.failures.push(Failure {
          path: path.clone(),
          message: error.to_string(),
        });
        self
          .cache
          .loaded
          .insert(path.clone(), Err(error.to_string()));
      }
    }
    self.retain_documents(&selected);
    self.workspace.clear_analysis();
    self.share_workspace_documents();
    self.failures.sort_by(|a, b| a.path.cmp(&b.path));
    check_cancelled(cancelled)?;
    Ok(selected)
  }

  pub(super) fn collect_inputs(
    &self,
    mut selected: BTreeSet<PathBuf>,
    cancelled: &AtomicBool,
  ) -> Result<(BTreeSet<PathBuf>, Vec<Failure>), Error> {
    selected.extend(self.opened.keys().cloned());
    let inputs = self.collection_inputs();
    let (collected, failures) =
      self.collection.collect(&inputs, false, cancelled)?;
    selected.extend(collected);
    Ok((selected, failures))
  }

  fn collection_inputs(&self) -> BTreeSet<PathBuf> {
    self
      .inputs
      .iter()
      .map(|input| self.absolute(input))
      .filter(|input| !self.opened.contains_key(input))
      .collect()
  }

  pub(in crate::interface::application) fn verify_collection(
    &self,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    self
      .collection
      .collect(&self.collection_inputs(), true, cancelled)?;
    Ok(())
  }

  /// Lets analyses load the workspace documents without reading them again.
  fn share_workspace_documents(&mut self) {
    for document in self.workspace.shared_documents() {
      if let Some(path) = self.path(document.snapshot().document) {
        self
          .cache
          .loaded
          .entry(path.to_path_buf())
          .or_insert(Ok(document));
      }
    }
  }

  fn retain_documents(&mut self, selected: &BTreeSet<PathBuf>) {
    let removed: Vec<_> = self
      .entries
      .keys()
      .filter(|path| !selected.contains(*path))
      .cloned()
      .collect();
    for path in removed {
      self.discard(&path);
    }
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

struct Inputs<'a> {
  opened: &'a BTreeMap<PathBuf, Arc<str>>,
  documents: &'a files::Documents,
  entries: &'a mut BTreeMap<PathBuf, Snapshot>,
  failures: &'a mut Vec<Failure>,
  cache: &'a mut InputCache,
  cancelled: &'a AtomicBool,
}

fn engine_error(error: Error) -> engine::Error {
  match error {
    Error::Engine(error) => error,
    other => engine::Error::Input(other.to_string()),
  }
}

impl AnalysisInputs for Inputs<'_> {
  fn source(
    &mut self,
    input: &SourceInput,
  ) -> Result<SourceDocument, engine::Error> {
    check_cancelled(self.cancelled)?;
    if self.cache.frozen && !self.cache.paths.contains_key(input) {
      return Ok(SourceDocument::Failed(
        "Source lookup differs from the captured inputs".into(),
      ));
    }
    let path = match self
      .cache
      .paths
      .entry(input.clone())
      .or_insert_with(|| files::resolve(input, self.opened, self.entries))
    {
      Ok(path) => path.clone(),
      Err(message) => return Ok(SourceDocument::Unresolved(message.clone())),
    };
    if !self.cache.loaded.contains_key(&path) {
      if self.cache.frozen {
        return Ok(SourceDocument::Failed(
          "Source was not part of the captured inputs".into(),
        ));
      }
      let loaded = files::read(&path, self.opened).and_then(|bytes| {
        self
          .documents
          .parse_entry(self.entries, &path, &bytes, self.cancelled)
      });
      check_cancelled(self.cancelled)?;
      if let Err(error) = &loaded {
        self.failures.push(Failure {
          path: path.clone(),
          message: error.to_string(),
        });
      }
      self
        .cache
        .loaded
        .insert(path.clone(), loaded.map_err(|error| error.to_string()));
    }
    Ok(match &self.cache.loaded[&path] {
      Ok(document) => SourceDocument::Resolved(document.clone()),
      Err(message) => SourceDocument::Failed(message.clone()),
    })
  }

  fn evaluate(
    &mut self,
    query: &InputQuery,
  ) -> Result<InputResult, engine::Error> {
    if let Some(value) = self.cache.evaluated.get(query) {
      return Ok(value.clone());
    }
    if self.cache.frozen {
      return Ok(InputResult::Unknown(engine::ValueIssue::Dynamic));
    }
    let value = files::evaluate(query, self.cancelled).map_err(engine_error)?;
    self.cache.evaluated.insert(query.clone(), value.clone());
    Ok(value)
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
}
