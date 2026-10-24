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
  Results(PathBuf),
  Document(PathBuf),
  Names(PathBuf),
  Workspace,
  WorkspaceAnalysis,
}

struct State {
  application: Application,
  generation: Arc<()>,
  jobs: Vec<Weak<AtomicBool>>,
  preparations: BTreeMap<RequestScope, Arc<Preparation>>,
  background: Option<tokio::task::AbortHandle>,
  updates: tokio::sync::watch::Sender<()>,
}

impl State {
  fn accept<T>(
    &mut self,
    request: &Request,
    result: Result<T, Error>,
  ) -> Result<T, Error> {
    if !Arc::ptr_eq(&request.generation, &self.generation) {
      return Err(Error::Superseded);
    }
    check_cancelled(&request.cancellation.0)?;
    if matches!(result, Err(Error::Superseded)) {
      self.invalidate();
    }
    result
  }

  /// Keeps the most recently collected analyses; reused results may be older.
  fn publish(&mut self, application: &Application) {
    if let Some(analyses) = &application.analyses
      && self
        .application
        .analyses
        .as_ref()
        .is_none_or(|previous| previous.collected < analyses.collected)
    {
      self.application.analyses = Some(analyses.clone());
      self.updates.send_replace(());
    }
  }

  fn invalidate(&mut self) {
    self.generation = Arc::default();
    self.application.analyses = None;
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
  analysis_worker: Arc<tokio::sync::Semaphore>,
}

struct Request {
  subscription: Subscription,
  generation: Arc<()>,
  cancellation: Cancellation,
}

struct Preparation {
  result: Shared<BoxFuture<'static, Result<Arc<Application>, Error>>>,
  cancelled: Arc<AtomicBool>,
  subscribers: AtomicUsize,
  retain_completed: bool,
  prerequisite: Option<Arc<Subscription>>,
}

struct Subscription {
  job: Arc<Preparation>,
  active: AtomicBool,
}

impl Subscription {
  fn new(job: Arc<Preparation>) -> Self {
    Self {
      job,
      active: AtomicBool::new(true),
    }
  }

  fn release(&self) {
    if self.active.swap(false, Ordering::AcqRel)
      && self.job.subscribers.fetch_sub(1, Ordering::AcqRel) == 1
      && !self.job.retains_result()
    {
      self.job.cancelled.store(true, Ordering::Release);
      if let Some(prerequisite) = &self.job.prerequisite {
        prerequisite.release();
      }
    }
  }
}

impl Drop for Subscription {
  fn drop(&mut self) {
    self.release();
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
        updates: tokio::sync::watch::channel(()).0,
      })),
      workers: Arc::new(tokio::sync::Semaphore::new(
        std::thread::available_parallelism()
          .map_or(1, |count| count.get().saturating_sub(1).max(1)),
      )),
      index_worker: Arc::new(tokio::sync::Semaphore::new(1)),
      analysis_worker: Arc::new(tokio::sync::Semaphore::new(1)),
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
    state.application.discovery.invalidate();
    state.invalidate();
    Ok(())
  }

  pub fn set_watching(&self, enabled: bool) -> Result<(), Error> {
    let mut state = lock(&self.state)?;
    state.application.discovery.watching(enabled);
    state.invalidate();
    Ok(())
  }

  pub fn files_changed(&self, paths: &[PathBuf]) -> Result<(), Error> {
    let mut state = lock(&self.state)?;
    state.application.files_changed(paths);
    state.invalidate();
    Ok(())
  }

  pub fn updates(&self) -> Result<tokio::sync::watch::Receiver<()>, Error> {
    Ok(lock(&self.state)?.updates.subscribe())
  }

  /// Indexes syntax, then analyzes each entry.
  pub fn start_index(&self) -> Result<(), Error> {
    let mut state = lock(&self.state)?;
    if state
      .background
      .as_ref()
      .is_some_and(|task| !task.is_finished())
    {
      return Ok(());
    }
    if let Some(application) = state
      .preparations
      .get(&RequestScope::WorkspaceAnalysis)
      .and_then(|job| job.result.peek())
      .and_then(|result| result.as_ref().ok())
      .cloned()
    {
      state.publish(&application);
      return Ok(());
    }
    let request =
      self.subscribe_current(&mut state, RequestScope::WorkspaceAnalysis);
    let shared = self.state.clone();
    let task = tokio::spawn(async move {
      let result = request.subscription.job.result.clone().await;
      if let Ok(mut state) = lock(&shared)
        && let Ok(application) = state.accept(&request, result)
      {
        state.publish(&application);
      }
    });
    state.background = Some(task.abort_handle());
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
      let request = prepared?;
      let result = request.subscription.job.result.clone().await;
      let (application, analyses) = {
        let mut state = lock(&state)?;
        let application = state.accept(&request, result)?;
        state.publish(&application);
        (application, state.application.analyses.clone())
      };
      let failed_inputs = !application.failures().is_empty();
      let result =
        run_blocking(workers, request.cancellation.0.clone(), move |flag| {
          let mut application = application.as_ref().clone();
          application.analyses = analyses.or(application.analyses);
          operation(&mut application, flag)
        })
        .await;
      let mut state = lock(&state)?;
      let result = state.accept(&request, result);
      if result.is_err() && failed_inputs {
        state
          .preparations
          .retain(|_, job| !Arc::ptr_eq(job, &request.subscription.job));
      }
      result
    }
  }

  fn workers_for(&self, scope: &RequestScope) -> Arc<tokio::sync::Semaphore> {
    match scope {
      RequestScope::WorkspaceAnalysis => &self.analysis_worker,
      RequestScope::Workspace | RequestScope::Names(_) => &self.index_worker,
      _ => &self.workers,
    }
    .clone()
  }

  fn subscribe(&self, mut scope: RequestScope) -> Result<Request, Error> {
    let mut state = lock(&self.state)?;
    match &mut scope {
      RequestScope::Analysis(path)
      | RequestScope::Results(path)
      | RequestScope::Document(path)
      | RequestScope::Names(path) => {
        *path = state.application.document_path(path)?;
      }
      RequestScope::Workspace | RequestScope::WorkspaceAnalysis => {}
    }
    Ok(self.subscribe_current(&mut state, scope))
  }

  fn subscribe_current(
    &self,
    state: &mut State,
    scope: RequestScope,
  ) -> Request {
    state
      .preparations
      .retain(|_, job| !job.cancelled.load(Ordering::Acquire));
    let subscription = self.prepare(state, scope);
    let flag = Arc::new(AtomicBool::new(false));
    state.jobs.retain(|job| job.strong_count() != 0);
    state.jobs.push(Arc::downgrade(&flag));
    Request {
      subscription,
      generation: state.generation.clone(),
      cancellation: Cancellation(flag),
    }
  }

  fn prepare(&self, state: &mut State, scope: RequestScope) -> Subscription {
    if let Some(job) = state.preparations.get(&scope)
      && job
        .subscribers
        .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
          (count != 0 || job.retains_result())
            .then(|| count.checked_add(1))
            .flatten()
        })
        .is_ok()
    {
      return Subscription::new(job.clone());
    }
    let prerequisite = match &scope {
      RequestScope::WorkspaceAnalysis => {
        Some(self.prepare(state, RequestScope::Workspace))
      }
      RequestScope::Names(_) | RequestScope::Results(_) => {
        Some(self.prepare(state, RequestScope::WorkspaceAnalysis))
      }
      _ => None,
    };
    let job = Arc::new(Preparation::new(
      state.application.clone(),
      scope.clone(),
      self.workers_for(&scope),
      prerequisite,
    ));
    state.preparations.insert(scope, job.clone());
    Subscription::new(job)
  }
}

impl Preparation {
  fn retains_result(&self) -> bool {
    self.retain_completed && self.result.peek().is_some_and(Result::is_ok)
  }

  fn new(
    mut application: Application,
    scope: RequestScope,
    workers: Arc<tokio::sync::Semaphore>,
    prerequisite: Option<Subscription>,
  ) -> Self {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    let retain_completed = matches!(
      scope,
      RequestScope::Workspace
        | RequestScope::WorkspaceAnalysis
        | RequestScope::Names(_)
        | RequestScope::Results(_)
    );
    let prerequisite = prerequisite.map(Arc::new);
    let dependency = prerequisite.clone();
    let result = async move {
      if let Some(prerequisite) = dependency {
        let result = prerequisite.job.result.clone().await;
        prerequisite.release();
        application = result?.as_ref().clone();
      }
      run_blocking(workers, flag, move |flag| {
        match &scope {
          RequestScope::Analysis(path) => {
            application.refresh_for(Some(path), flag)?;
            let id = application.document_id(path).ok_or_else(|| {
              Error::Message(format!("Cannot load {}", path.display()))
            })?;
            application.analyze_document(id, flag)?;
          }
          RequestScope::Results(path) => {
            application.prepare_analysis(path, flag)?;
          }
          RequestScope::Document(path) => {
            application.refresh_for(Some(path), flag)?;
          }
          RequestScope::Names(path) => {
            application.prepare_names(path, flag)?;
          }
          RequestScope::Workspace => {
            application.refresh_for(None, flag)?;
          }
          RequestScope::WorkspaceAnalysis => {
            application.prepare_workspace_analysis(flag)?;
          }
        }
        Ok(Arc::new(application))
      })
      .await
    }
    .boxed()
    .shared();
    Self {
      result,
      cancelled,
      subscribers: AtomicUsize::new(1),
      retain_completed,
      prerequisite,
    }
  }
}

/// Runs blocking work once a worker is free, unless it is cancelled first.
async fn run_blocking<T: Send + 'static>(
  workers: Arc<tokio::sync::Semaphore>,
  cancelled: Arc<AtomicBool>,
  work: impl FnOnce(&AtomicBool) -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
  let permit = workers
    .acquire_owned()
    .await
    .map_err(|error| Error::Message(error.to_string()))?;
  tokio::task::spawn_blocking(move || {
    let _permit = permit;
    check_cancelled(&cancelled)?;
    let output = work(&cancelled)?;
    check_cancelled(&cancelled)?;
    Ok(output)
  })
  .await
  .map_err(|error| Error::Message(format!("Analysis worker failed: {error}")))?
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
