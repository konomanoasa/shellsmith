use super::application::InputSelection;
use super::{Error, Failure, files};
use crate::interface::application::engine::{
  self, AnalysisInputs, Document, InputQuery, InputResult, Snapshot,
  SourceDocument, SourceInput, check_cancelled,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug, Default)]
pub(super) struct InputCache {
  pub(super) selection: Option<InputSelection>,
  pub(super) loaded: BTreeMap<PathBuf, Result<Arc<Document>, String>>,
  failed_inputs: BTreeMap<PathBuf, Result<Arc<[u8]>, String>>,
  pub(super) paths: BTreeMap<SourceInput, Result<PathBuf, String>>,
  pub(super) evaluated: BTreeMap<InputQuery, InputResult>,
  pub(super) frozen: bool,
}

pub(super) struct Inputs<'a> {
  pub(super) opened: &'a BTreeMap<PathBuf, Arc<str>>,
  pub(super) documents: &'a files::Documents,
  pub(super) entries: &'a mut BTreeMap<PathBuf, Snapshot>,
  pub(super) failures: &'a mut Vec<Failure>,
  pub(super) cache: &'a mut InputCache,
  pub(super) cancelled: &'a AtomicBool,
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
      let bytes = files::read(&path, self.opened);
      let loaded = bytes.as_ref().map_err(Clone::clone).and_then(|bytes| {
        self.documents.parse_entry(
          self.entries,
          &path,
          bytes,
          false,
          self.cancelled,
        )
      });
      check_cancelled(self.cancelled)?;
      if let Err(error) = &loaded {
        self.cache.record_failure(path.clone(), bytes);
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

/// What a failed load observed; a different observation invalidates the cache.
fn failed_input(bytes: Result<Vec<u8>, Error>) -> Result<Arc<[u8]>, String> {
  bytes.map(Arc::from).map_err(|error| error.to_string())
}

impl InputCache {
  pub(super) fn record_failure(
    &mut self,
    path: PathBuf,
    bytes: Result<Vec<u8>, Error>,
  ) {
    self.failed_inputs.insert(path, failed_input(bytes));
  }

  pub(super) fn current(
    &self,
    opened: &BTreeMap<PathBuf, Arc<str>>,
    entries: &BTreeMap<PathBuf, Snapshot>,
    cancelled: &AtomicBool,
  ) -> Result<bool, Error> {
    for (path, document) in &self.loaded {
      check_cancelled(cancelled)?;
      if let Ok(document) = document
        && !files::read(path, opened)
          .is_ok_and(|bytes| bytes == document.source().as_bytes())
      {
        return Ok(false);
      }
    }
    for (path, expected) in &self.failed_inputs {
      check_cancelled(cancelled)?;
      if failed_input(files::read(path, opened)) != *expected {
        return Ok(false);
      }
    }
    for (input, expected) in &self.paths {
      check_cancelled(cancelled)?;
      if &files::resolve(input, opened, entries) != expected {
        return Ok(false);
      }
    }
    for (input, expected) in &self.evaluated {
      check_cancelled(cancelled)?;
      if files::evaluate(input, cancelled)? != *expected {
        return Ok(false);
      }
    }
    Ok(true)
  }
}
