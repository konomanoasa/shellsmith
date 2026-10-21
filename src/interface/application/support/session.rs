use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use super::{Application, Error, Settings};
use crate::interface::application::engine::check_cancelled;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RequestScope {
  Analysis(PathBuf),
  Document(PathBuf),
  Names(PathBuf),
  Workspace,
}

struct State {
  application: Application,
  generation: Arc<()>,
  jobs: Vec<Weak<AtomicBool>>,
  preparations: BTreeMap<RequestScope, Arc<Preparation>>,
  background: Option<tokio::task::AbortHandle>,
}

impl State {
  fn invalidate(&mut self) {
    self.generation = Arc::default();
    if let Some(task) = self.background.take() {
      task.abort();
    }
    for job in std::mem::take(&mut self.preparations).into_values() {
      job.cancelled.store(true, Ordering::Relaxed);
    }
    for flag in self.jobs.drain(..).filter_map(|flag| flag.upgrade()) {
      flag.store(true, Ordering::Relaxed);
    }
  }
}

pub struct Session {
  state: Arc<Mutex<State>>,
  workers: Arc<tokio::sync::Semaphore>,
  index_worker: Arc<tokio::sync::Semaphore>,
}

struct Preparation {
  result: Shared<BoxFuture<'static, Result<Arc<Application>, Error>>>,
  cancelled: Arc<AtomicBool>,
  subscribers: AtomicUsize,
  retain_completed: bool,
}

struct Subscription(Arc<Preparation>);

impl Drop for Subscription {
  fn drop(&mut self) {
    if self.0.subscribers.fetch_sub(1, Ordering::AcqRel) == 1
      && !(self.0.retain_completed
        && self.0.result.peek().is_some_and(Result::is_ok))
    {
      self.0.cancelled.store(true, Ordering::Release);
    }
  }
}

struct Cancellation(Arc<AtomicBool>);

impl Drop for Cancellation {
  fn drop(&mut self) {
    self.0.store(true, Ordering::Relaxed);
  }
}

fn lock(state: &Mutex<State>) -> Result<MutexGuard<'_, State>, Error> {
  state
    .lock()
    .map_err(|_| Error::Message("Analysis session poisoned".into()))
}

impl Session {
  pub fn absolute(&self, path: &Path) -> Result<PathBuf, Error> {
    Ok(lock(&self.state)?.application.absolute(path))
  }

  pub fn document_path(&self, path: &Path) -> Result<PathBuf, Error> {
    lock(&self.state)?.application.document_path(path)
  }
  pub fn new(settings: Settings) -> Result<Self, Error> {
    Ok(Self {
      state: Arc::new(Mutex::new(State {
        application: Application::new(settings)?,
        generation: Arc::default(),
        jobs: Vec::new(),
        preparations: BTreeMap::new(),
        background: None,
      })),
      workers: Arc::new(tokio::sync::Semaphore::new(
        std::thread::available_parallelism()
          .map_or(1, |count| count.get().saturating_sub(1).max(1)),
      )),
      index_worker: Arc::new(tokio::sync::Semaphore::new(1)),
    })
  }

  pub fn add_input(&self, path: &Path) -> Result<(), Error> {
    let mut state = lock(&self.state)?;
    if state.application.add_input(path) {
      state.invalidate();
    }
    Ok(())
  }

  pub fn remove_input(&self, path: &Path) -> Result<(), Error> {
    let mut state = lock(&self.state)?;
    if state.application.remove_input(path) {
      state.invalidate();
    }
    Ok(())
  }

  pub fn set_overlay(
    &self,
    path: &Path,
    source: Option<String>,
  ) -> Result<(), Error> {
    let mut state = lock(&self.state)?;
    let path = if source.is_some() {
      state.application.document_path(path)?
    } else {
      state.application.absolute(path)
    };
    state.application.set_overlay(path, source.map(Arc::from));
    state.invalidate();
    Ok(())
  }

  pub fn invalidate(&self) -> Result<(), Error> {
    let mut state = lock(&self.state)?;
    state.application.collection.invalidate();
    state.invalidate();
    Ok(())
  }

  pub fn set_watching(&self, enabled: bool) -> Result<(), Error> {
    let mut state = lock(&self.state)?;
    state.application.collection.watching(enabled);
    state.invalidate();
    Ok(())
  }

  pub fn files_changed(&self, paths: &[PathBuf]) -> Result<(), Error> {
    let mut state = lock(&self.state)?;
    state.application.files_changed(paths);
    state.invalidate();
    Ok(())
  }

  /// Indexing errors are returned by workspace requests.
  pub fn start_index(&self) -> Result<(), Error> {
    {
      let state = lock(&self.state)?;
      if state
        .background
        .as_ref()
        .is_some_and(|task| !task.is_finished())
        || state
          .preparations
          .get(&RequestScope::Workspace)
          .is_some_and(|job| job.result.peek().is_some_and(Result::is_ok))
      {
        return Ok(());
      }
    }
    let (subscription, _, guard) = self.subscribe(RequestScope::Workspace)?;
    let task = tokio::spawn(async move {
      let _ = subscription.0.result.clone().await;
      drop(subscription);
      drop(guard);
    });
    lock(&self.state)?.background = Some(task.abort_handle());
    Ok(())
  }

  /// Inputs are captured before polling. Dropping the future cancels this request;
  /// shared preparation stops only when its last subscriber leaves.
  pub fn request<T: Send + 'static>(
    &self,
    scope: RequestScope,
    operation: impl FnOnce(&mut Application, &AtomicBool) -> Result<T, Error>
    + Send
    + 'static,
  ) -> impl Future<Output = Result<T, Error>> + Send + 'static {
    let workers = self.workers_for(&scope);
    let prepared = self.subscribe(scope);
    let state = self.state.clone();
    async move {
      let (subscription, generation, guard) = prepared?;
      let result = subscription.0.result.clone().await;
      validate(&state, &generation, &guard.0)?;
      let application = result?;
      let permit = workers
        .acquire_owned()
        .await
        .map_err(|error| Error::Message(error.to_string()))?;
      let flag = guard.0.clone();
      let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        check_cancelled(&flag)?;
        let mut application = application.as_ref().clone();
        let output = operation(&mut application, &flag)?;
        check_cancelled(&flag)?;
        Ok::<_, Error>(output)
      })
      .await
      .map_err(|error| worker_error(&error))?;
      let mut state = lock(&state)?;
      current(&state, &generation, &guard.0)?;
      if matches!(result, Err(Error::Superseded)) {
        state.invalidate();
      }
      let output = result?;
      drop(subscription);
      Ok(output)
    }
  }

  fn workers_for(&self, scope: &RequestScope) -> Arc<tokio::sync::Semaphore> {
    if matches!(scope, RequestScope::Workspace | RequestScope::Names(_)) {
      self.index_worker.clone()
    } else {
      self.workers.clone()
    }
  }

  fn subscribe(
    &self,
    scope: RequestScope,
  ) -> Result<(Subscription, Arc<()>, Cancellation), Error> {
    let mut state = lock(&self.state)?;
    let scope = match scope {
      RequestScope::Analysis(path) => {
        RequestScope::Analysis(state.application.document_path(&path)?)
      }
      RequestScope::Document(path) => {
        RequestScope::Document(state.application.document_path(&path)?)
      }
      RequestScope::Names(path) => {
        RequestScope::Names(state.application.document_path(&path)?)
      }
      RequestScope::Workspace => RequestScope::Workspace,
    };
    state
      .preparations
      .retain(|_, job| !job.cancelled.load(Ordering::Acquire));
    let job = if let Some(job) = state.preparations.get(&scope)
      && job
        .subscribers
        .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
          (count != 0
            || (job.retain_completed
              && job.result.peek().is_some_and(Result::is_ok)))
          .then(|| count.checked_add(1))
          .flatten()
        })
        .is_ok()
    {
      job.clone()
    } else {
      let job = Arc::new(Preparation::new(
        state.application.clone(),
        scope.clone(),
        self.workers_for(&scope),
      ));
      state.preparations.insert(scope, job.clone());
      job
    };
    let flag = Arc::new(AtomicBool::new(false));
    state.jobs.retain(|job| job.strong_count() != 0);
    state.jobs.push(Arc::downgrade(&flag));
    Ok((
      Subscription(job),
      state.generation.clone(),
      Cancellation(flag),
    ))
  }
}

impl Preparation {
  fn new(
    mut application: Application,
    scope: RequestScope,
    workers: Arc<tokio::sync::Semaphore>,
  ) -> Self {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    let retain_completed =
      matches!(scope, RequestScope::Workspace | RequestScope::Names(_));
    let result = async move {
      let permit = workers
        .acquire_owned()
        .await
        .map_err(|error| Error::Message(error.to_string()))?;
      tokio::task::spawn_blocking(move || {
        let _permit = permit;
        check_cancelled(&flag)?;
        match &scope {
          RequestScope::Analysis(path) => {
            application.refresh_for(Some(path), &flag)?;
            let id = application.document_id(path).ok_or_else(|| {
              Error::Message(format!("Cannot load {}", path.display()))
            })?;
            application.analyze_document(id, &flag)?;
          }
          RequestScope::Document(path) => {
            application.refresh_for(Some(path), &flag)?;
          }
          RequestScope::Names(path) => {
            application.prepare_names(path, &flag)?;
          }
          RequestScope::Workspace => {
            application.refresh_for(None, &flag)?;
          }
        }
        check_cancelled(&flag)?;
        Ok(Arc::new(application))
      })
      .await
      .map_err(|error| worker_error(&error))?
    }
    .boxed()
    .shared();
    Self {
      result,
      cancelled,
      subscribers: AtomicUsize::new(1),
      retain_completed,
    }
  }
}

fn worker_error(error: &tokio::task::JoinError) -> Error {
  Error::Message(format!("Analysis worker failed: {error}"))
}

fn current(
  state: &State,
  generation: &Arc<()>,
  flag: &AtomicBool,
) -> Result<(), Error> {
  if !Arc::ptr_eq(generation, &state.generation) {
    return Err(Error::Superseded);
  }
  Ok(check_cancelled(flag)?)
}

fn validate(
  state: &Mutex<State>,
  generation: &Arc<()>,
  flag: &AtomicBool,
) -> Result<(), Error> {
  let state = lock(state)?;
  current(&state, generation, flag)
}

impl Drop for Session {
  fn drop(&mut self) {
    if let Ok(mut state) = self.state.lock() {
      state.invalidate();
    }
  }
}

#[cfg(test)]
#[path = "../../../../tests/implementation/session.rs"]
mod tests;
