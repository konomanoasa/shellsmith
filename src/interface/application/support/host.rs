use super::{collection::Collection, files, names::NameCollection};
use crate::interface::application::engine;
use engine::{
  AnalysisInputs, AnalysisRequest, Document, InputQuery, InputResult, Snapshot,
  SourceDocument, SourceInput,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
            if name.is_empty()
              || name[0].is_ascii_digit()
              || !name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
              || matches!(
                name,
                b"IFS" | b"LINENO" | b"OPTIND" | b"PPID" | b"PWD"
              )
            {
              return None;
            }
            Some((name.to_vec(), value.as_encoded_bytes().to_vec()))
          })
          .collect(),
        values: BTreeMap::from([
          (b"IFS".to_vec(), b" \t\n".to_vec()),
          (b"OPTIND".to_vec(), b"1".to_vec()),
        ]),
        special: BTreeMap::from([(b"?".to_vec(), b"0".to_vec())]),
        other_variables: InitialBinding::Absent,
        other_functions: InitialBinding::Absent,
        errexit: Some(false),
        nounset: Some(false),
        noglob: Some(false),
        ..InitialEnvironment::default()
      },
      limits: AnalysisLimits::default(),
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

pub(crate) fn check(cancelled: &AtomicBool) -> Result<(), Error> {
  if cancelled.load(Ordering::Relaxed) {
    Err(engine::Error::Cancelled.into())
  } else {
    Ok(())
  }
}

#[derive(Clone, Debug, Default)]
pub(super) struct InputCache {
  pub(super) loaded: BTreeMap<PathBuf, Result<Arc<Document>, String>>,
  pub(super) paths: BTreeMap<SourceInput, Result<PathBuf, String>>,
  pub(super) evaluated: engine::EvaluationInputs,
  pub(super) frozen: bool,
}

#[derive(Clone, Debug)]
pub struct Application {
  pub(in crate::interface::application) settings: Settings,
  pub(in crate::interface::application) workspace: Workspace,
  entries: BTreeMap<PathBuf, Snapshot>,
  pub(super) opened: BTreeMap<PathBuf, Arc<str>>,
  pub(super) inputs: BTreeSet<PathBuf>,
  pub(super) failures: Vec<Failure>,
  documents: Arc<files::Documents>,
  pub(super) collection: Arc<Collection>,
  pub(super) cache: InputCache,
  pub(super) names: Option<Arc<NameCollection>>,
}

impl Application {
  pub fn new(settings: Settings) -> Result<Self, Error> {
    if !settings.cwd.is_absolute() {
      return Err(Error::Message("Working directory must be absolute".into()));
    }
    Ok(Self {
      settings,
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
    if !settings.cwd.is_absolute() {
      return Err(Error::Message("Working directory must be absolute".into()));
    }
    self.settings = settings;
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
    self.entries.iter().find_map(|(path, entry)| {
      (entry.document == id).then_some(path.as_path())
    })
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

  pub(super) fn resolve_source(
    &self,
    input: &SourceInput,
  ) -> Result<PathBuf, String> {
    files::resolve(input, &self.opened, &self.entries)
  }

  pub fn add_input(&mut self, path: &Path) {
    self.names = None;
    self.inputs.insert(self.settings.cwd.join(path));
  }
  pub fn remove_input(&mut self, path: &Path) {
    self.names = None;
    self.inputs.remove(&self.settings.cwd.join(path));
    self.collection.remove(&self.absolute(path));
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
    files::ordinary(&self.settings.cwd.join(path), true)?;
    let path = self.absolute(path);
    check(cancelled)?;
    self.invalidate_inputs();
    self.opened.insert(path.clone(), Arc::from(source));
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
    check(cancelled)?;
    self.invalidate_inputs();
    self.opened.insert(path.clone(), Arc::from(source));
    self.replace(&path, source.as_bytes(), cancelled)
  }

  /// Discards the overlay even if reloading from disk fails.
  pub fn close(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check(cancelled)?;
    let path = self.absolute(path);
    self.invalidate_inputs();
    self.opened.remove(&path);
    self.collection.changed(std::slice::from_ref(&path));
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
    check(cancelled)?;
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
    let after = self.entries.get(path).map_or(0, |entry| entry.revision.0);
    let result = self
      .documents
      .parse(path, bytes, after, cancelled)
      .and_then(|document| {
        let snapshot = document.snapshot();
        self.entries.insert(path.to_path_buf(), snapshot);
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
    self.refresh_for(None, true, cancelled)
  }

  pub(super) fn refresh_for(
    &mut self,
    document: Option<&Path>,
    analyze: bool,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check(cancelled)?;
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
        check(cancelled)?;
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
    for document in self.workspace.shared_documents() {
      if let Some(path) = self.path(document.snapshot().document) {
        self.cache.loaded.insert(path.to_path_buf(), Ok(document));
      }
    }
    if analyze {
      let mut states: BTreeMap<_, engine::DependencyState> = BTreeMap::new();
      for path in &selected {
        if let Some(id) = self.document_id(path) {
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
      }
      for (reference, state) in states {
        if self.workspace.reference_state(reference) != Some(&state) {
          self.workspace.set_reference(reference, state)?;
        }
      }
    }
    self.failures.sort_by(|a, b| a.path.cmp(&b.path));
    check(cancelled)
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

  pub(super) fn initial_environment(
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
      (b"LINENO".as_slice(), InitialBinding::Unknown),
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
    check(self.cancelled).map_err(engine_error)?;
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
      let loaded = (|| {
        let bytes = files::read(&path, self.opened)?;
        let after = self.entries.get(&path).map_or(0, |entry| entry.revision.0);
        self.documents.parse(&path, &bytes, after, self.cancelled)
      })();
      check(self.cancelled).map_err(engine_error)?;
      match &loaded {
        Ok(document) => {
          let snapshot = document.snapshot();
          self.entries.insert(path.clone(), snapshot);
        }
        Err(error) => self.failures.push(Failure {
          path: path.clone(),
          message: error.to_string(),
        }),
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
    check(cancelled)?;
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
    for document in self.workspace.shared_documents() {
      if let Some(path) = self.path(document.snapshot().document) {
        self
          .cache
          .loaded
          .entry(path.to_path_buf())
          .or_insert(Ok(document));
      }
    }
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
    let result =
      self
        .workspace
        .analyze_with(request, &mut inputs, cancelled)?;
    self.workspace.publish_analysis(result)?;
    Ok(())
  }
}
