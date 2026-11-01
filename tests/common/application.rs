use shellsmith::interface::application::engine::{
  DocumentId, Location, SemanticSpan,
};
use shellsmith::interface::application::{
  Application, Error, Failure, RenameEdit, RenameTarget, RequestScope, Session,
  Settings,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Client {
  session: Session,
  runtime: tokio::runtime::Runtime,
  snapshot: Application,
  failures: Vec<Failure>,
}

impl Client {
  pub fn new(settings: Settings) -> Result<Self, Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .unwrap();
    let session = {
      let _entered = runtime.enter();
      Session::new(settings.clone())?
    };
    Ok(Self {
      session,
      runtime,
      snapshot: Application::new(settings)?,
      failures: Vec::new(),
    })
  }

  pub fn snapshot(&self) -> &Application {
    &self.snapshot
  }

  pub fn failures(&self) -> &[Failure] {
    &self.failures
  }

  fn request<T: Send + 'static>(
    &mut self,
    scope: RequestScope,
    cancelled: &AtomicBool,
    operation: impl FnOnce(&mut Application, &AtomicBool) -> Result<T, Error>
    + Send
    + 'static,
  ) -> Result<T, Error> {
    let cancelled = cancelled.load(Ordering::Relaxed);
    let outcome = self.runtime.block_on(async {
      self
        .session
        .request(scope, move |app, flag| {
          let stopped = AtomicBool::new(true);
          let output = operation(app, if cancelled { &stopped } else { flag })?;
          Ok((output, app.clone()))
        })
        .await
    });
    self.failures = outcome.failures;
    let (output, snapshot) = outcome.result?;
    self.snapshot = snapshot;
    Ok(output)
  }

  pub fn add_input(&self, path: &Path) {
    self.session.add_input(path).unwrap();
  }

  pub fn open(
    &mut self,
    path: &Path,
    source: &str,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    self.session.set_overlay(path, Some(source.to_owned()))?;
    self.request(
      RequestScope::Document(path.to_path_buf()),
      cancelled,
      |_, _| Ok(()),
    )
  }

  pub fn change(
    &mut self,
    path: &Path,
    source: &str,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    self.open(path, source, cancelled)
  }

  pub fn close(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    self.session.set_overlay(path, None)?;
    self.request(
      RequestScope::Document(path.to_path_buf()),
      cancelled,
      |_, _| Ok(()),
    )
  }

  pub fn document(&mut self, path: &Path) -> Result<DocumentId, Error> {
    let path = path.to_path_buf();
    self.request(
      RequestScope::Document(path.clone()),
      &AtomicBool::new(false),
      move |app, _| app.loaded_document(&path),
    )
  }

  pub fn classify(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<Vec<SemanticSpan>, Error> {
    let path = path.to_path_buf();
    self.request(
      RequestScope::Analysis(path.clone()),
      cancelled,
      move |app, flag| app.classify(app.loaded_document(&path)?, flag),
    )
  }

  pub fn definitions(
    &mut self,
    path: &Path,
    offset: usize,
    cancelled: &AtomicBool,
  ) -> Result<Vec<Location>, Error> {
    let path = path.to_path_buf();
    self.request(
      RequestScope::Results(path.clone()),
      cancelled,
      move |app, flag| {
        app.definitions(app.loaded_document(&path)?, offset, flag)
      },
    )
  }

  pub fn document_links(
    &mut self,
    path: &Path,
    cancelled: &AtomicBool,
  ) -> Result<Vec<(Location, PathBuf)>, Error> {
    let path = path.to_path_buf();
    self.request(
      RequestScope::Results(path.clone()),
      cancelled,
      move |app, flag| app.document_links(app.loaded_document(&path)?, flag),
    )
  }

  pub fn references(
    &mut self,
    path: &Path,
    offset: usize,
    include_declaration: bool,
    cancelled: &AtomicBool,
  ) -> Result<Vec<Location>, Error> {
    let path = path.to_path_buf();
    self.request(
      RequestScope::Names(path.clone()),
      cancelled,
      move |app, flag| app.references(&path, offset, include_declaration, flag),
    )
  }

  pub fn prepare_rename(
    &mut self,
    path: &Path,
    offset: usize,
    cancelled: &AtomicBool,
  ) -> Result<Option<RenameTarget>, Error> {
    let path = path.to_path_buf();
    self.request(
      RequestScope::Names(path.clone()),
      cancelled,
      move |app, flag| app.prepare_rename(&path, offset, flag),
    )
  }

  pub fn rename(
    &mut self,
    path: &Path,
    offset: usize,
    new_name: &str,
    cancelled: &AtomicBool,
  ) -> Result<Vec<RenameEdit>, Error> {
    let path = path.to_path_buf();
    let new_name = new_name.to_owned();
    self.request(
      RequestScope::Names(path.clone()),
      cancelled,
      move |app, flag| app.rename(&path, offset, &new_name, flag),
    )
  }
}
