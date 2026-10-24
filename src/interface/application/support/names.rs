use super::application::InputSelection;
use super::{Application, Error};
use crate::interface::application::engine;
use engine::check_cancelled;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug)]
pub(in crate::interface::application) struct EntryMetadata {
  pub path: PathBuf,
  pub dependencies: BTreeSet<(PathBuf, PathBuf)>,
  pub damaged: bool,
}

#[derive(Clone, Debug, Default)]
pub(in crate::interface::application) struct NameCollection {
  pub entries: Vec<EntryMetadata>,
  pub results: engine::NameResults,
  pub documents: BTreeMap<engine::Snapshot, (PathBuf, Arc<str>)>,
  roots: BTreeSet<PathBuf>,
  selection: InputSelection,
}

impl Application {
  pub(in crate::interface::application) fn prepare_names(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<Arc<NameCollection>, Error> {
    let path = self.absolute(path);
    if let Some(names) = &self.names
      && names.roots.contains(&path)
    {
      return Ok(names.clone());
    }
    let analyses = self.prepare_analysis(&path, cancelled)?;
    let roots = analyses.entries.keys().cloned().collect();
    let names = Arc::new(self.collect_names(
      roots,
      analyses.selection.clone(),
      cancelled,
    )?);
    self.names = Some(names.clone());
    Ok(names)
  }

  fn collect_names(
    &mut self,
    roots: BTreeSet<PathBuf>,
    selection: InputSelection,
    cancelled: &AtomicBool,
  ) -> Result<NameCollection, Error> {
    let workspace = std::mem::take(&mut self.workspace);
    let collection = self.collect_entries(roots, selection, cancelled);
    self.workspace = workspace;
    let collection = collection?;
    self.restore_analysis_documents(cancelled)?;
    Ok(collection)
  }

  /// Each root has an independent execution environment.
  fn collect_entries(
    &mut self,
    roots: BTreeSet<PathBuf>,
    selection: InputSelection,
    cancelled: &AtomicBool,
  ) -> Result<NameCollection, Error> {
    let mut collection = NameCollection {
      roots,
      selection,
      ..NameCollection::default()
    };
    let mut workspaces = Vec::new();
    for path in &collection.roots {
      check_cancelled(cancelled)?;
      self.workspace = self.analyze_entry(path, cancelled)?;
      let analysis = self.workspace.analysis().expect("completed analysis");
      let dependencies = analysis
        .source_bindings()
        .iter()
        .filter_map(|(reference, state)| {
          let engine::DependencyState::Resolved(target) = state else {
            return None;
          };
          Some((
            self
              .path(
                self.workspace.source_snapshot(reference.snapshot).document,
              )?
              .to_path_buf(),
            self.path(target.document)?.to_path_buf(),
          ))
        })
        .collect();
      collection.entries.push(EntryMetadata {
        path: path.clone(),
        dependencies,
        damaged: false,
      });
      for document in self.workspace.documents() {
        if let Some(path) = self.path(document.snapshot().document) {
          collection.documents.insert(
            document.snapshot(),
            (path.to_path_buf(), Arc::from(document.source())),
          );
        }
      }
      workspaces.push(self.workspace.clone());
    }
    collection.results =
      engine::AnalysisResults::new(&workspaces).names(cancelled)?;
    for ((metadata, result), workspace) in collection
      .entries
      .iter_mut()
      .zip(&collection.results.entries)
      .zip(&workspaces)
    {
      metadata.damaged = result.relations.occurrences.iter().any(|site| {
        workspace
          .document(site.location.snapshot.document)
          .is_some_and(engine::Document::has_syntax_errors)
      });
    }
    Ok(collection)
  }

  pub(in crate::interface::application) fn reanalyze_names(
    &self,
    collection: &NameCollection,
    changed: &BTreeMap<PathBuf, String>,
    cancelled: &AtomicBool,
  ) -> Result<NameCollection, Error> {
    let mut scratch = self.clone();
    scratch.cache.frozen = true;
    for (path, source) in collection.documents.values() {
      let Some(text) = changed
        .get(path)
        .filter(|text| text.as_str() != source.as_ref())
      else {
        continue;
      };
      scratch.stage(path, text.as_bytes(), cancelled)?;
    }
    scratch.collect_names(
      collection.roots.clone(),
      collection.selection.clone(),
      cancelled,
    )
  }

  pub(in crate::interface::application) fn check_name_inputs(
    &mut self,
    collection: &NameCollection,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check_cancelled(cancelled)?;
    if !self.name_inputs_current(collection, cancelled)? {
      self.names = None;
      self.analyses = None;
      return Err(Error::Superseded);
    }
    Ok(())
  }

  fn name_inputs_current(
    &self,
    collection: &NameCollection,
    cancelled: &AtomicBool,
  ) -> Result<bool, Error> {
    if self.select_inputs(collection.selection.explicit.clone(), cancelled)?
      != collection.selection
    {
      return Ok(false);
    }
    self.cache.current(&self.opened, &self.entries, cancelled)
  }
}
