use super::{Error, Failure, check, files};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
pub(super) struct Collection {
  state: Mutex<State>,
  scan: Mutex<()>,
}

#[derive(Debug, Default)]
struct State {
  generation: Arc<()>,
  watching: bool,
  roots: BTreeMap<PathBuf, CachedRoot>,
}

#[derive(Clone, Debug, Default)]
struct CachedRoot {
  data: Option<Arc<Root>>,
  changes: BTreeSet<PathBuf>,
}

#[derive(Clone, Debug, Default)]
struct Root {
  files: BTreeMap<PathBuf, PathBuf>,
  directories: BTreeMap<PathBuf, PathBuf>,
  links: BTreeMap<PathBuf, PathBuf>,
  failures: Vec<Failure>,
}

impl Collection {
  pub(super) fn watching(&self, enabled: bool) {
    let mut state = self.state.lock().expect("collection state");
    if state.watching != enabled {
      state.watching = enabled;
      state.generation = Arc::default();
      state.roots.clear();
    }
  }

  pub(super) fn invalidate(&self) {
    let mut state = self.state.lock().expect("collection state");
    state.generation = Arc::default();
    state.roots.clear();
  }

  pub(super) fn changed(&self, paths: &[PathBuf]) {
    let mut state = self.state.lock().expect("collection state");
    state.generation = Arc::default();
    for root in state.roots.values_mut() {
      root.changes.extend(paths.iter().cloned());
    }
  }

  pub(super) fn remove(&self, path: &Path) {
    let mut state = self.state.lock().expect("collection state");
    state.generation = Arc::default();
    state.roots.remove(path);
  }

  pub(super) fn collect(
    &self,
    inputs: &BTreeSet<PathBuf>,
    verify: bool,
    cancelled: &AtomicBool,
  ) -> Result<(BTreeSet<PathBuf>, Vec<Failure>), Error> {
    check(cancelled)?;
    let _scan = self.scan.lock().expect("collection worker");
    check(cancelled)?;
    let (generation, roots) = {
      let state = self.state.lock().expect("collection state");
      let roots: Vec<_> = inputs
        .iter()
        .map(|path| {
          let root = if verify && !state.watching {
            CachedRoot::default()
          } else {
            state.roots.get(path).cloned().unwrap_or_default()
          };
          (path.clone(), root)
        })
        .collect();
      (state.generation.clone(), roots)
    };
    let mut completed = BTreeMap::new();
    let mut selected = BTreeSet::new();
    let mut failures = Vec::new();
    for (path, cached) in roots {
      check(cancelled)?;
      let data = match cached.data {
        Some(data) if cached.changes.is_empty() => data,
        Some(data) => {
          let scopes = data.affected(&path, &cached.changes);
          if scopes.is_empty() {
            data
          } else {
            Arc::new(scan(&path, &scopes, &data, cancelled)?)
          }
        }
        None => Arc::new(scan(
          &path,
          &BTreeSet::from([path.clone()]),
          &Root::default(),
          cancelled,
        )?),
      };
      selected.extend(data.files.values().cloned());
      failures.extend(data.failures.iter().cloned());
      completed.insert(
        path,
        CachedRoot {
          data: Some(data),
          changes: BTreeSet::new(),
        },
      );
    }
    check(cancelled)?;
    let mut state = self.state.lock().expect("collection state");
    if !Arc::ptr_eq(&generation, &state.generation) {
      return Err(Error::Superseded);
    }
    state.roots.extend(completed);
    Ok((selected, failures))
  }
}

impl Root {
  fn affected(
    &self,
    root: &Path,
    changed: &BTreeSet<PathBuf>,
  ) -> BTreeSet<PathBuf> {
    let mut scopes = BTreeSet::new();
    for path in changed {
      let resolved = files::absolute(root, path);
      for path in [path.as_path(), resolved.as_path()] {
        if root.starts_with(path) {
          return BTreeSet::from([root.to_path_buf()]);
        }
        if path.starts_with(root) {
          scopes.insert(path.to_path_buf());
        }
        for (alias, target) in self.directories.iter().chain(&self.links) {
          if let Ok(suffix) = path.strip_prefix(target) {
            scopes.insert(alias.join(suffix));
          } else if target.starts_with(path) {
            scopes.insert(alias.clone());
          }
        }
      }
    }
    let scopes: BTreeSet<_> = scopes
      .into_iter()
      .map(|path| {
        if path.file_name().is_some_and(|name| name == ".gitignore") {
          path.parent().expect("ignore parent").to_path_buf()
        } else {
          path
        }
      })
      .collect();
    if scopes.iter().any(|scope| {
      self.links.keys().any(|link| link.starts_with(scope))
        || std::fs::symlink_metadata(scope)
          .is_ok_and(|metadata| metadata.is_symlink())
    }) {
      return BTreeSet::from([root.to_path_buf()]);
    }
    scopes
      .iter()
      .filter(|path| {
        !path
          .ancestors()
          .skip(1)
          .any(|parent| scopes.contains(parent))
      })
      .cloned()
      .collect()
  }
}

fn scan(
  root: &Path,
  scopes: &BTreeSet<PathBuf>,
  previous: &Root,
  cancelled: &AtomicBool,
) -> Result<Root, Error> {
  check(cancelled)?;
  let affected =
    |path: &Path| scopes.iter().any(|scope| path.starts_with(scope));
  let mut result = previous.clone();
  result.files.retain(|path, _| !affected(path));
  result.directories.retain(|path, _| !affected(path));
  result.links.retain(|path, _| !affected(path));
  result.failures.retain(|failure| !affected(&failure.path));
  let visited: BTreeSet<_> = result
    .directories
    .iter()
    .filter(|(path, _)| !scopes.iter().any(|scope| scope.starts_with(path)))
    .map(|(_, target)| target.clone())
    .collect();
  let traversal = Arc::new(Mutex::new((visited, BTreeMap::new())));
  let filter_state = traversal.clone();
  let filter_scopes = scopes.clone();
  let mut walker = ignore::WalkBuilder::new(root);
  walker
    .hidden(false)
    .parents(false)
    .ignore(false)
    .git_global(false)
    .git_exclude(false)
    .require_git(false)
    .follow_links(true)
    .sort_by_file_name(Ord::cmp)
    .filter_entry(move |entry| {
      if entry.file_name() == ".git"
        || !filter_scopes.iter().any(|scope| {
          entry.path().starts_with(scope) || scope.starts_with(entry.path())
        })
      {
        return false;
      }
      let mut state = filter_state.lock().expect("collection traversal");
      let directory = entry.file_type().is_some_and(|kind| kind.is_dir());
      if !directory && !entry.path_is_symlink() {
        return true;
      }
      let Ok(path) = entry.path().canonicalize() else {
        return true;
      };
      if entry.path_is_symlink() {
        state.1.insert(entry.path().to_path_buf(), path.clone());
      }
      !directory
        || (path.file_name().is_none_or(|name| name != ".git")
          && state.0.insert(path))
    });
  for entry in walker.build() {
    check(cancelled)?;
    let entry = match entry {
      Ok(entry) => entry,
      Err(error) => {
        collect_failure(root, &error, &mut result.failures);
        continue;
      }
    };
    if let Some(error) = entry.error() {
      collect_failure(entry.path(), error, &mut result.failures);
    }
    let path = entry.path();
    let loaded = (|| -> Result<(), Error> {
      let resolved = path.canonicalize()?;
      if entry.file_type().is_some_and(|kind| kind.is_dir()) {
        result.directories.insert(path.to_path_buf(), resolved);
      } else if entry.file_type().is_some_and(|kind| kind.is_file()) {
        if entry.depth() == 0 || files::selected_shell(&resolved, path)? {
          result.files.insert(path.to_path_buf(), resolved);
        }
      } else if entry.depth() == 0 {
        return Err(Error::Message("Not a regular file or directory".into()));
      }
      Ok(())
    })();
    if let Err(error) = loaded {
      result.failures.push(Failure {
        path: path.to_path_buf(),
        message: error.to_string(),
      });
    }
  }
  result.links.extend(
    traversal
      .lock()
      .expect("collection traversal")
      .1
      .iter()
      .map(|(alias, target)| (alias.clone(), target.clone())),
  );
  result
    .failures
    .sort_by(|a, b| (&a.path, &a.message).cmp(&(&b.path, &b.message)));
  result.failures.dedup();
  check(cancelled)?;
  Ok(result)
}

fn collect_failure(
  path: &Path,
  error: &ignore::Error,
  failures: &mut Vec<Failure>,
) {
  match error {
    ignore::Error::Partial(errors) => {
      for error in errors {
        collect_failure(path, error, failures);
      }
    }
    ignore::Error::WithPath { path, err } => {
      collect_failure(path, err, failures);
    }
    ignore::Error::WithDepth { err, .. } => {
      collect_failure(path, err, failures);
    }
    ignore::Error::Loop { .. } => {}
    error => failures.push(Failure {
      path: path.to_path_buf(),
      message: error.to_string(),
    }),
  }
}

#[cfg(test)]
#[path = "../../../../tests/implementation/collection.rs"]
mod tests;
