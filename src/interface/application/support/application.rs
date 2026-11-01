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

#[derive(Clone, Debug, Eq, PartialEq)]
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
  pub(super) limited: Option<engine::AnalysisRequest>,
  pub(in crate::interface::application) analyses:
    Option<Arc<AnalysisCollection>>,
}

impl Application {
  pub fn new(settings: Settings) -> Result<Self, Error> {
    Ok(Self {
      settings: settings.checked()?,
      workspace: Workspace::default(),
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
      limited: None,
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

  fn invalidate_inputs(&mut self) {
    self.limited = None;
    self.cache = InputCache::default();
    self.clear_semantics();
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
    self.entry_document(&self.absolute(path))
  }

  pub fn loaded_document(&self, path: &Path) -> Result<DocumentId, Error> {
    self
      .document_id(path)
      .ok_or_else(|| Error::Message(format!("Cannot load {}", path.display())))
  }

  /// An opened document keeps its identity after its path resolves elsewhere.
  pub(super) fn entry_document(&self, identity: &Path) -> Option<DocumentId> {
    self
      .entries
      .get(identity)
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
      self.set_analyses(None);
    }
    added
  }

  pub fn remove_input(&mut self, path: &Path) -> bool {
    let removed = self.inputs.remove(&self.settings.cwd.join(path));
    if removed {
      self.set_analyses(None);
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
    if let Some(id) = self.entry_document(path)
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
    self.limited = None;
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
    self.set_analyses(None);
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

  pub(in crate::interface::application) fn refresh_for(
    &mut self,
    document: Option<&Path>,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    self.limited = None;
    check_cancelled(cancelled)?;
    self.failures.clear();
    self.set_analyses(None);
    self.cache = InputCache::default();
    let selected = if let Some(path) = document {
      BTreeSet::from([self.absolute(path)])
    } else {
      let selection = self.select_inputs(BTreeSet::new(), cancelled)?;
      self.failures.clone_from(&selection.failures);
      let selected = selection.paths.clone();
      self.cache.selection = Some(selection);
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

  pub(super) fn select_inputs(
    &self,
    mut explicit: BTreeSet<PathBuf>,
    cancelled: &AtomicBool,
  ) -> Result<InputSelection, Error> {
    let (collected, failures) =
      self
        .discovery
        .collect(&self.discovery_inputs(), false, cancelled)?;
    let mut paths: BTreeSet<_> =
      self.opened.keys().cloned().chain(collected).collect();
    explicit.retain(|path| !paths.contains(path));
    paths.extend(explicit.iter().cloned());
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

  pub(super) fn selection_current(
    &self,
    selection: &InputSelection,
    cancelled: &AtomicBool,
  ) -> Result<bool, Error> {
    self.verify_discovery(cancelled)?;
    Ok(self.select_inputs(selection.explicit.clone(), cancelled)? == *selection)
  }

  /// Unnotified input changes require recollection on the next request.
  pub(in crate::interface::application) fn discard_results(&mut self) {
    self.limited = None;
    self.clear_semantics();
    self.outdated = true;
  }

  pub(in crate::interface::application) fn clear_semantics(&mut self) {
    self.set_analyses(None);
    self.workspace.clear_analysis();
  }

  /// Name results derive from the analyses and end with them.
  pub(super) fn set_analyses(
    &mut self,
    analyses: Option<Arc<AnalysisCollection>>,
  ) {
    if !matches!(
      (&self.analyses, &analyses),
      (Some(old), Some(new)) if Arc::ptr_eq(old, new)
    ) {
      self.names = None;
    }
    self.analyses = analyses;
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

#[cfg(test)]
#[path = "../../../../tests/implementation/application.rs"]
mod tests;
