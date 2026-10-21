use super::{Application, Error, files};
use crate::interface::application::engine;
use engine::{Document, Workspace, check_cancelled};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug)]
pub(in crate::interface::application) struct EntryNames {
  pub path: PathBuf,
  pub relations: engine::NameRelations,
  pub definitions: BTreeMap<engine::SymbolId, Option<engine::Location>>,
  pub dependencies: BTreeSet<(PathBuf, PathBuf)>,
  pub damaged: bool,
}

#[derive(Clone, Debug, Default)]
pub(in crate::interface::application) struct NameCollection {
  pub entries: Vec<EntryNames>,
  pub documents: BTreeMap<engine::Snapshot, (PathBuf, Arc<str>)>,
  roots: BTreeSet<PathBuf>,
  explicit: BTreeSet<PathBuf>,
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
    self.refresh_for(None, cancelled)?;
    if self.document_id(&path).is_none() {
      self.load(&path, cancelled)?;
    }
    let roots = self
      .workspace
      .documents()
      .filter_map(|document| {
        self
          .path(document.snapshot().document)
          .map(Path::to_path_buf)
      })
      .collect();
    let mut names = self.collect_names(roots, cancelled)?;
    if !self.failures.is_empty() {
      return Err(Error::RenameRefused(
        "Some workspace inputs could not be read".into(),
      ));
    }
    names.explicit.insert(path);
    let names = Arc::new(names);
    self.names = Some(names.clone());
    Ok(names)
  }

  /// Collects the names of each of `roots` and adds the documents that the
  /// collection loaded to the workspace.
  fn collect_names(
    &mut self,
    roots: BTreeSet<PathBuf>,
    cancelled: &AtomicBool,
  ) -> Result<NameCollection, Error> {
    let workspace = std::mem::take(&mut self.workspace);
    let collection = self.collect_entries(roots, cancelled);
    self.workspace = workspace;
    let collection = collection?;
    let loaded: Vec<_> = self
      .cache
      .loaded
      .values()
      .filter_map(|loaded| loaded.as_ref().ok())
      .filter(|document| {
        self
          .workspace
          .document(document.snapshot().document)
          .is_none()
      })
      .cloned()
      .collect();
    for document in loaded {
      let id = document.snapshot().document;
      self.workspace.insert_shared(document)?;
      self
        .workspace
        .index_document(id, self.settings.limits, cancelled)?;
    }
    Ok(collection)
  }

  /// Analyzes each of `roots` alone, in a workspace of its own.
  fn collect_entries(
    &mut self,
    roots: BTreeSet<PathBuf>,
    cancelled: &AtomicBool,
  ) -> Result<NameCollection, Error> {
    let mut collection = NameCollection {
      roots,
      ..NameCollection::default()
    };
    for path in &collection.roots {
      check_cancelled(cancelled)?;
      collection
        .entries
        .push(self.collect_entry_names(path, cancelled)?);
      for document in self.workspace.documents() {
        if let Some(path) = self.path(document.snapshot().document) {
          collection.documents.insert(
            document.snapshot(),
            (path.to_path_buf(), Arc::from(document.source())),
          );
        }
      }
    }
    Ok(collection)
  }

  fn collect_entry_names(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<EntryNames, Error> {
    let document = self
      .cache
      .loaded
      .get(path)
      .and_then(|loaded| loaded.as_ref().ok())
      .cloned()
      .ok_or_else(|| {
        Error::Message(format!("Cannot load {}", path.display()))
      })?;
    let id = document.snapshot().document;
    self.workspace = Workspace::default();
    self.workspace.insert_shared(document)?;
    self.analyze_document(id, cancelled)?;
    let relations = self.workspace.name_relations(cancelled)?;
    let analysis = self.workspace.analysis().expect("completed analysis");
    let mut definitions = BTreeMap::new();
    for symbol in relations
      .names
      .iter()
      .flat_map(|name| &name.definitions)
      .chain(
        relations
          .occurrences
          .iter()
          .filter_map(|site| site.resolution.as_ref())
          .flat_map(|resolution| &resolution.candidates),
      )
    {
      definitions.entry(*symbol).or_insert_with(|| {
        self
          .workspace
          .symbol(*symbol)
          .and_then(|symbol| self.workspace.definition_location(symbol))
      });
    }
    let dependencies = analysis
      .source_bindings()
      .iter()
      .filter_map(|(reference, state)| {
        let engine::DependencyState::Resolved(target) = state else {
          return None;
        };
        Some((
          self
            .path(self.workspace.source_snapshot(reference.snapshot).document)?
            .to_path_buf(),
          self.path(target.document)?.to_path_buf(),
        ))
      })
      .collect();
    let damaged = relations.occurrences.iter().any(|site| {
      self
        .workspace
        .document(site.location.snapshot.document)
        .is_some_and(Document::has_syntax_errors)
    });
    Ok(EntryNames {
      path: path.to_path_buf(),
      relations,
      definitions,
      dependencies,
      damaged,
    })
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
    let mut result =
      scratch.collect_names(collection.roots.clone(), cancelled)?;
    result.explicit.clone_from(&collection.explicit);
    Ok(result)
  }

  pub(in crate::interface::application) fn check_name_inputs(
    &mut self,
    collection: &NameCollection,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check_cancelled(cancelled)?;
    if !self.failures.is_empty() {
      return Err(Error::RenameRefused(
        "Some workspace inputs could not be read".into(),
      ));
    }
    if !self.name_inputs_current(collection, cancelled)? {
      self.names = None;
      return Err(Error::Superseded);
    }
    Ok(())
  }

  fn name_inputs_current(
    &self,
    collection: &NameCollection,
    cancelled: &AtomicBool,
  ) -> Result<bool, Error> {
    let (selected, failures) =
      self.collect_inputs(collection.explicit.clone(), cancelled)?;
    if selected != collection.roots || !failures.is_empty() {
      return Ok(false);
    }
    for (path, source) in collection.documents.values() {
      check_cancelled(cancelled)?;
      if !self.opened.contains_key(path)
        && !files::read(path, &self.opened)
          .is_ok_and(|bytes| bytes == source.as_bytes())
      {
        return Ok(false);
      }
    }
    for (input, expected) in &self.cache.paths {
      check_cancelled(cancelled)?;
      if &self.resolve_source(input) != expected {
        return Ok(false);
      }
    }
    for (input, expected) in &self.cache.evaluated {
      check_cancelled(cancelled)?;
      if files::evaluate(input, cancelled)? != *expected {
        return Ok(false);
      }
    }
    Ok(true)
  }
}
