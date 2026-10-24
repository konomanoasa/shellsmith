use super::{
  analysis::AnalysisCollection, discovery::Discovery, files,
  inputs::InputCache, names::NameCollection,
};
use crate::interface::application::engine;
use engine::{Snapshot, check_cancelled};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use engine::{AnalysisLimits, DocumentId, InitialEnvironment, Workspace};

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

/// Workspace inputs together with the explicitly requested documents.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct InputSelection {
  pub(super) explicit: BTreeSet<PathBuf>,
  pub(super) paths: BTreeSet<PathBuf>,
  failures: Vec<Failure>,
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

#[derive(Clone, Debug)]
pub struct Application {
  pub(super) settings: Settings,
  pub(in crate::interface::application) workspace: Workspace,
  pub(super) entries: BTreeMap<PathBuf, Snapshot>,
  pub(super) opened: BTreeMap<PathBuf, Arc<str>>,
  inputs: BTreeSet<PathBuf>,
  pub(super) failures: Vec<Failure>,
  /// A recheck found inputs that changed after this state was prepared.
  pub(super) outdated: bool,
  pub(in crate::interface::application) request_prepared: bool,
  pub(super) documents: Arc<files::Documents>,
  pub(super) discovery: Arc<Discovery>,
  pub(super) cache: InputCache,
  pub(super) names: Option<Arc<NameCollection>>,
  pub(in crate::interface::application) analyses:
    Option<Arc<AnalysisCollection>>,
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
      outdated: false,
      request_prepared: false,
      documents: Arc::default(),
      discovery: Arc::default(),
      cache: InputCache::default(),
      names: None,
      analyses: None,
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
    self.analyses = None;
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

  /// Accepts regular files and missing paths; rejects other file types.
  pub(super) fn document_path(&self, path: &Path) -> Result<PathBuf, Error> {
    files::ordinary(&self.settings.cwd.join(path), true)?;
    Ok(self.absolute(path))
  }

  /// Opened documents keep the identity resolved when they were opened.
  pub(super) fn overlay_path(
    &self,
    path: &Path,
    open: bool,
  ) -> Result<PathBuf, Error> {
    let joined = self.settings.cwd.join(path);
    if self.opened.contains_key(&joined) {
      Ok(joined)
    } else if open {
      self.document_path(path)
    } else {
      Ok(self.absolute(path))
    }
  }

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
      self.discovery.changed(std::slice::from_ref(&path));
    }
  }

  /// Retains the given path; symlinks are resolved anew at each collection.
  pub fn add_input(&mut self, path: &Path) -> bool {
    let added = self.inputs.insert(self.settings.cwd.join(path));
    if added {
      self.names = None;
      self.analyses = None;
    }
    added
  }

  pub fn remove_input(&mut self, path: &Path) -> bool {
    let removed = self.inputs.remove(&self.settings.cwd.join(path));
    if removed {
      self.names = None;
      self.analyses = None;
      self.discovery.remove(&self.absolute(path));
    }
    removed
  }

  pub fn files_changed(&mut self, paths: &[PathBuf]) {
    let paths: Vec<_> = paths
      .iter()
      .map(|path| self.settings.cwd.join(path))
      .collect();
    self.discovery.changed(&paths);
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
    let path = self.overlay_path(path, false)?;
    if !self.opened.contains_key(&path) {
      return Err(Error::Message("Document is not open".into()));
    }
    check_cancelled(cancelled)?;
    self.set_overlay(path.clone(), Some(Arc::from(source)));
    self.replace(&path, source.as_bytes(), cancelled)
  }

  /// Discards the overlay even if reloading from disk fails. A path that
  /// now resolves elsewhere is no longer a document identity.
  pub fn close(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check_cancelled(cancelled)?;
    let path = self.overlay_path(path, false)?;
    self.set_overlay(path.clone(), None);
    let current = self.absolute(&path);
    if current != path {
      self.discard(&path);
    }
    self.load(&current, cancelled)
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
      && let Some(document) = self
        .workspace
        .shared_document(id)
        .filter(|document| document.source().as_bytes() == bytes)
    {
      self
        .workspace
        .index_document(id, self.settings.limits, cancelled)?;
      self.cache.loaded.insert(path.to_path_buf(), Ok(document));
      return Ok(());
    }
    let result = self
      .documents
      .parse_entry(&mut self.entries, path, bytes, false, cancelled)
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

  /// Overrides analysis input without adding a workspace document.
  pub(super) fn stage(
    &mut self,
    path: &Path,
    bytes: &[u8],
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    self.analyses = None;
    self.names = None;
    let document = self.documents.parse_entry(
      &mut self.entries,
      path,
      bytes,
      true,
      cancelled,
    )?;
    self.cache.loaded.insert(path.to_path_buf(), Ok(document));
    Ok(())
  }

  pub(super) fn load(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let bytes = files::read(path, &self.opened);
    let result = match &bytes {
      Ok(bytes) => self.replace(path, bytes, cancelled),
      Err(error) => {
        self.discard(path);
        Err(error.clone())
      }
    };
    self.failures.retain(|failure| failure.path != path);
    if let Err(error) = &result {
      self.cache.record_failure(path.to_path_buf(), bytes);
      check_cancelled(cancelled)?;
      self.failures.push(Failure {
        path: path.to_path_buf(),
        message: error.to_string(),
      });
    }
    result
  }

  /// Individual file errors go to `failures`; they do not fail the whole refresh.
  pub fn refresh(&mut self, cancelled: &AtomicBool) -> Result<(), Error> {
    self.discovery.invalidate();
    self.refresh_for(None, cancelled)?;
    let analyses = self.collect_analysis(BTreeSet::new(), cancelled)?;
    if let Some(workspace) = analyses.entries.values().next_back() {
      self.workspace = workspace.clone();
    }
    self.restore_analysis_documents(cancelled)?;
    for (reference, state) in
      engine::AnalysisResults::new(analyses.entries.values()).dependencies()
    {
      if self.workspace.reference_state(reference) != Some(&state) {
        self.workspace.set_reference(reference, state)?;
      }
    }
    Ok(check_cancelled(cancelled)?)
  }

  pub(in crate::interface::application) fn refresh_for(
    &mut self,
    document: Option<&Path>,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check_cancelled(cancelled)?;
    self.failures.clear();
    self.names = None;
    self.analyses = None;
    self.cache = InputCache::default();
    let selected = if let Some(path) = document {
      BTreeSet::from([self.absolute(path)])
    } else {
      let (selected, failures) =
        self.collect_inputs(BTreeSet::new(), cancelled)?;
      self.cache.selection = Some(InputSelection {
        explicit: BTreeSet::new(),
        paths: selected.clone(),
        failures: failures.clone(),
      });
      self.failures = failures;
      selected
    };
    for path in &selected {
      if let Err(error) = self.load(path, cancelled) {
        check_cancelled(cancelled)?;
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
    Ok(())
  }

  fn collect_inputs(
    &self,
    mut selected: BTreeSet<PathBuf>,
    cancelled: &AtomicBool,
  ) -> Result<(BTreeSet<PathBuf>, Vec<Failure>), Error> {
    selected.extend(self.opened.keys().cloned());
    let inputs = self.discovery_inputs();
    let (collected, failures) =
      self.discovery.collect(&inputs, false, cancelled)?;
    selected.extend(collected);
    Ok((selected, failures))
  }

  pub(super) fn select_inputs(
    &self,
    explicit: BTreeSet<PathBuf>,
    cancelled: &AtomicBool,
  ) -> Result<InputSelection, Error> {
    let (paths, failures) = self.collect_inputs(explicit.clone(), cancelled)?;
    Ok(InputSelection {
      explicit,
      paths,
      failures,
    })
  }

  fn discovery_inputs(&self) -> BTreeSet<PathBuf> {
    self
      .inputs
      .iter()
      .map(|input| self.absolute(input))
      .filter(|input| !self.opened.contains_key(input))
      .collect()
  }

  pub(super) fn verify_discovery(
    &self,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    self
      .discovery
      .collect(&self.discovery_inputs(), true, cancelled)?;
    Ok(())
  }

  /// Rescans the inputs and compares the files they select now.
  pub(super) fn selection_current(
    &self,
    selection: &InputSelection,
    cancelled: &AtomicBool,
  ) -> Result<bool, Error> {
    self.verify_discovery(cancelled)?;
    Ok(self.select_inputs(selection.explicit.clone(), cancelled)? == *selection)
  }

  /// Results collected from inputs that changed without notification are
  /// collected again by the next request.
  pub(in crate::interface::application) fn discard_results(&mut self) {
    self.names = None;
    self.analyses = None;
    self.workspace.clear_analysis();
    self.outdated = true;
  }

  pub(super) fn share_workspace_documents(&mut self) {
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
}
