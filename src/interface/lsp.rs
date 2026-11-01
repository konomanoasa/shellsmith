use crate::interface::application::engine::{
  self, DocumentId, Modifier, SemanticKind, SemanticSpan, SymbolDetails,
  SymbolKind,
};
use crate::interface::application::{
  Application, Error, FormatEdit, RenameEdit, RenameTarget, RequestScope,
  Session, Settings,
};
use async_lsp::lsp_types::notification::Notification;
use async_lsp::lsp_types::request::Request;
use async_lsp::lsp_types::{
  self as lsp, notification as notif, request as req,
};
use async_lsp::router::Router;
use async_lsp::{
  AnyEvent, AnyNotification, AnyRequest, ClientSocket, ErrorCode, LspService,
  MainLoop, RequestId, ResponseError,
};
use futures_util::future::{AbortHandle, Abortable, BoxFuture, try_join};
use futures_util::{TryStreamExt, stream};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tower::{Service, ServiceBuilder};
use url::Url;

pub async fn run(
  settings: Settings,
  input: impl tokio::io::AsyncRead + Unpin,
  mut output: impl tokio::io::AsyncWrite + Unpin,
) -> Result<bool, Error> {
  let session = Arc::new(Session::new(settings)?);
  let shutdown = Arc::new(AtomicBool::new(false));
  let ended = shutdown.clone();
  let (main_loop, client) = MainLoop::new_server(move |client| {
    let service = ServiceBuilder::new()
      .layer(async_lsp::server::LifecycleLayer::default())
      .service(router(session, client.clone(), shutdown));
    Requests::new(service, client)
  });
  let (sender, mut messages) = mpsc::channel::<Vec<u8>>(1);
  let input = stream::try_unfold(
    (BufReader::new(input), sender.clone(), client),
    |(mut input, sender, client)| async move {
      loop {
        let (body, header_error) = read_frame(&mut input).await?;
        let message = if let Some(error) = header_error {
          Err(Rejection::response(None, -32700, error))
        } else {
          decode_message(&body)
        };
        match message {
          Err(Rejection::Log(message)) => log(&client, message),
          Err(Rejection::Response(response)) => {
            send_frame(&sender, frame(&serde_json::to_vec(&response)?)).await?;
          }
          Ok(message) => {
            return Ok::<_, io::Error>(Some((
              frame(&serde_json::to_vec(&message)?),
              (input, sender, client),
            )));
          }
        }
      }
    },
  )
  .into_async_read();
  let service = async move {
    let result = main_loop
      .run(
        input,
        Output {
          sender,
          buffer: Vec::new(),
          flushing: None,
        },
      )
      .await;
    // Drain queued replies before reporting an input or dispatch error.
    Ok::<_, Error>(result)
  };
  let writer = async move {
    while let Some(bytes) = messages.recv().await {
      output.write_all(&bytes).await?;
      output.flush().await?;
    }
    Ok::<_, Error>(())
  };
  let (result, ()) = try_join(service, writer).await?;
  result.map_err(|error| Error::Message(error.to_string()))?;
  Ok(ended.load(Ordering::Relaxed))
}

pub fn stdio() -> Result<bool, Error> {
  let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()?;
  let result = runtime.block_on(run(
    Settings::new(std::env::current_dir()?),
    tokio::io::stdin(),
    tokio::io::stdout(),
  ));
  runtime.shutdown_background();
  result
}

async fn read_frame(
  input: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> io::Result<(Vec<u8>, Option<&'static str>)> {
  let mut length = None;
  let mut error = None;
  let mut line = Vec::new();
  loop {
    line.clear();
    if input.read_until(b'\n', &mut line).await? == 0 {
      return Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "Incomplete LSP header",
      ));
    }
    if !line.ends_with(b"\r\n") || !line.is_ascii() {
      error = Some("Invalid LSP header");
    }
    if line == b"\r\n" || line == b"\n" {
      break;
    }
    let Some((name, value)) = str::from_utf8(&line)
      .ok()
      .and_then(|line| line.trim_end().split_once(':'))
    else {
      error = Some("Invalid LSP header");
      continue;
    };
    if name.eq_ignore_ascii_case("Content-Length") {
      let value = value.trim();
      if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "Invalid Content-Length",
        ));
      }
      let value = value.parse::<u64>().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "Invalid Content-Length")
      })?;
      if length
        .replace(value)
        .is_some_and(|previous| previous != value)
      {
        return Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "Conflicting Content-Length headers",
        ));
      }
    } else if name.eq_ignore_ascii_case("Content-Type") {
      for parameter in value.split(';').skip(1) {
        if let Some((name, value)) = parameter.trim().split_once('=')
          && name.trim().eq_ignore_ascii_case("charset")
          && !["utf-8", "utf8"].iter().any(|encoding| {
            value
              .trim()
              .trim_matches('"')
              .eq_ignore_ascii_case(encoding)
          })
        {
          error = Some("Unsupported LSP charset");
        }
      }
    }
  }
  let length = length.ok_or_else(|| {
    io::Error::new(io::ErrorKind::InvalidData, "Missing Content-Length")
  })?;
  let mut body = Vec::new();
  input.take(length).read_to_end(&mut body).await?;
  if body.len() as u64 != length {
    return Err(io::Error::new(
      io::ErrorKind::UnexpectedEof,
      "Incomplete LSP body",
    ));
  }
  Ok((body, error))
}

fn frame(body: &[u8]) -> Vec<u8> {
  let mut bytes =
    format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
  bytes.extend_from_slice(body);
  bytes
}

enum Rejection {
  Response(Value),
  Log(String),
}

impl Rejection {
  fn response(id: Option<&Value>, code: i32, message: &str) -> Self {
    Self::Response(
      json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    )
  }
}

fn decode_message(body: &[u8]) -> Result<Value, Rejection> {
  let message: Value = serde_json::from_slice(body)
    .map_err(|_| Rejection::response(None, -32700, "Parse error"))?;
  let id = message
    .get("id")
    .filter(|id| serde_json::from_value::<RequestId>((*id).clone()).is_ok());
  let invalid = || Rejection::response(id, -32600, "Invalid Request");
  if message["jsonrpc"] != "2.0" {
    return Err(invalid());
  }
  if message.get("method").is_some() {
    if !message["method"].is_string()
      || (message.get("id").is_some() && id.is_none())
    {
      return Err(invalid());
    }
    if message.get("params").is_some_and(|params| {
      !params.is_null() && !params.is_object() && !params.is_array()
    }) {
      return Err(if let Some(id) = id {
        Rejection::response(Some(id), -32602, "Invalid params")
      } else {
        Rejection::Log(format!("{}: Invalid params", message["method"]))
      });
    }
    return Ok(message);
  }
  if message.get("result").is_some() || message.get("error").is_some() {
    if id.is_none()
      || (message.get("result").is_some() == message.get("error").is_some())
      || message.get("error").is_some_and(|error| {
        serde_json::from_value::<ResponseError>(error.clone()).is_err()
      })
    {
      return Err(Rejection::Log("Invalid JSON-RPC response".into()));
    }
    return Ok(message);
  }
  Err(invalid())
}

async fn send_frame(
  sender: &mpsc::Sender<Vec<u8>>,
  bytes: Vec<u8>,
) -> io::Result<()> {
  sender
    .send(bytes)
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "LSP output closed"))
}

struct Output {
  sender: mpsc::Sender<Vec<u8>>,
  // Queue complete frames so transport errors cannot split service responses.
  buffer: Vec<u8>,
  flushing: Option<BoxFuture<'static, io::Result<()>>>,
}

impl futures_util::io::AsyncWrite for Output {
  fn poll_write(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    bytes: &[u8],
  ) -> Poll<io::Result<usize>> {
    if self.flushing.is_some() {
      std::task::ready!(self.as_mut().poll_flush(cx))?;
    }
    self.buffer.extend_from_slice(bytes);
    Poll::Ready(Ok(bytes.len()))
  }

  fn poll_flush(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<io::Result<()>> {
    if self.flushing.is_none() && !self.buffer.is_empty() {
      let sender = self.sender.clone();
      let bytes = std::mem::take(&mut self.buffer);
      self.flushing =
        Some(Box::pin(async move { send_frame(&sender, bytes).await }));
    }
    if let Some(flushing) = &mut self.flushing {
      let result = std::task::ready!(flushing.as_mut().poll(cx));
      self.flushing = None;
      result?;
    }
    Poll::Ready(Ok(()))
  }

  fn poll_close(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<io::Result<()>> {
    self.poll_flush(cx)
  }
}

// Keep notifications flowing while application workers wait for CPU capacity.
struct Requests<S> {
  inner: S,
  client: ClientSocket,
  pending: HashMap<RequestId, (AbortHandle, Weak<()>)>,
}

impl<S> Requests<S> {
  fn new(inner: S, client: ClientSocket) -> Self {
    Self {
      inner,
      client,
      pending: HashMap::new(),
    }
  }
}

impl<S> Service<AnyRequest> for Requests<S>
where
  S: LspService<Error = ResponseError>,
  S::Future: Send + 'static,
  S::Response: Send + 'static,
{
  type Response = S::Response;
  type Error = ResponseError;
  type Future = BoxFuture<'static, Result<Self::Response, ResponseError>>;

  fn poll_ready(
    &mut self,
    cx: &mut Context<'_>,
  ) -> Poll<Result<(), Self::Error>> {
    self.inner.poll_ready(cx)
  }

  fn call(&mut self, request: AnyRequest) -> Self::Future {
    // Lifecycle changes precede the router's parameter decoding.
    let valid = match request.method.as_str() {
      req::Initialize::METHOD => {
        serde_json::from_value::<lsp::InitializeParams>(request.params.clone())
          .map(|_| ())
      }
      req::Shutdown::METHOD => {
        serde_json::from_value::<()>(request.params.clone())
      }
      _ => Ok(()),
    };
    if let Err(error) = valid {
      return Box::pin(std::future::ready(Err(ResponseError::new(
        ErrorCode::INVALID_PARAMS,
        error.to_string(),
      ))));
    }
    self
      .pending
      .retain(|_, (_, lifetime)| lifetime.strong_count() != 0);
    if self.pending.contains_key(&request.id) {
      return Box::pin(std::future::ready(Err(ResponseError::new(
        ErrorCode::INVALID_REQUEST,
        "Request ID is already active",
      ))));
    }
    let lifetime = Arc::new(());
    let (handle, registration) = AbortHandle::new_pair();
    self
      .pending
      .insert(request.id.clone(), (handle, Arc::downgrade(&lifetime)));
    let future = Abortable::new(self.inner.call(request), registration);
    Box::pin(async move {
      let result = future.await;
      drop(lifetime);
      result.unwrap_or_else(|_| {
        Err(ResponseError::new(
          ErrorCode::REQUEST_CANCELLED,
          "Request cancelled",
        ))
      })
    })
  }
}

impl<S> LspService for Requests<S>
where
  S: LspService<Error = ResponseError>,
  S::Future: Send + 'static,
  S::Response: Send + 'static,
{
  fn notify(
    &mut self,
    notification: AnyNotification,
  ) -> ControlFlow<async_lsp::Result<()>> {
    let method = notification.method.clone();
    let result = match method.as_str() {
      notif::Cancel::METHOD => {
        match serde_json::from_value::<lsp::CancelParams>(notification.params) {
          Ok(params) => {
            if let Some((handle, _)) = self.pending.get(&params.id) {
              handle.abort();
            }
            ControlFlow::Continue(())
          }
          Err(error) => ControlFlow::Break(Err(error.into())),
        }
      }
      notif::Initialized::METHOD | notif::Exit::METHOD => {
        let valid = if method == notif::Initialized::METHOD {
          serde_json::from_value::<lsp::InitializedParams>(
            notification.params.clone(),
          )
          .map(|_| ())
        } else {
          serde_json::from_value::<()>(notification.params.clone())
        };
        match valid {
          Ok(()) => self.inner.notify(notification),
          Err(error) => ControlFlow::Break(Err(error.into())),
        }
      }
      _ => self.inner.notify(notification),
    };
    match result {
      ControlFlow::Break(Err(async_lsp::Error::Deserialize(error))) => {
        log(&self.client, format!("{method}: {error}"));
        ControlFlow::Continue(())
      }
      ControlFlow::Break(Err(async_lsp::Error::Protocol(error)))
        if method != notif::Exit::METHOD =>
      {
        log(&self.client, format!("{method}: {error}"));
        ControlFlow::Continue(())
      }
      result => result,
    }
  }

  fn emit(&mut self, event: AnyEvent) -> ControlFlow<async_lsp::Result<()>> {
    self.inner.emit(event)
  }
}

type Reply<T> = Pin<Box<dyn Future<Output = Result<T, ResponseError>> + Send>>;

#[derive(Default)]
struct ClientSupport {
  refresh: bool,
  watch: bool,
  document_changes: bool,
  symbols: SymbolFormat,
  /// Present only when the client accepts full, relative-format tokens.
  semantic_tokens: Option<lsp::SemanticTokensLegend>,
}

impl ClientSupport {
  const fn refreshes_tokens(&self) -> bool {
    self.refresh && self.semantic_tokens.is_some()
  }
}

struct Server {
  session: Arc<Session>,
  client: ClientSocket,
  opened: BTreeMap<PathBuf, OpenDocument>,
  support: ClientSupport,
  shutdown: Arc<AtomicBool>,
}

impl Server {
  #[expect(
    deprecated,
    reason = "LSP clients may initialize with rootUri when workspaceFolders is absent."
  )]
  fn initialize(
    &mut self,
    params: lsp::InitializeParams,
  ) -> Result<lsp::InitializeResult, ResponseError> {
    let roots = if let Some(folders) = &params.workspace_folders {
      self.file_paths(folders.iter().map(|folder| &folder.uri))
    } else {
      self.file_paths(params.root_uri.iter())
    };
    for root in roots {
      self.session.add_input(&root).map_err(fault)?;
    }
    let workspace = params.capabilities.workspace;
    self.support.document_changes = workspace
      .as_ref()
      .and_then(|workspace| workspace.workspace_edit.as_ref())
      .and_then(|edit| edit.document_changes)
      == Some(true);
    self.support.refresh = workspace
      .as_ref()
      .and_then(|workspace| workspace.semantic_tokens.as_ref())
      .and_then(|tokens| tokens.refresh_support)
      == Some(true);
    self.support.watch = workspace
      .as_ref()
      .and_then(|workspace| workspace.did_change_watched_files.as_ref())
      .and_then(|watch| watch.dynamic_registration)
      == Some(true);
    self.support.symbols = if params
      .capabilities
      .text_document
      .as_ref()
      .and_then(|text| text.document_symbol.as_ref())
      .and_then(|symbol| symbol.hierarchical_document_symbol_support)
      == Some(true)
    {
      SymbolFormat::Hierarchical
    } else {
      SymbolFormat::Flat
    };
    self.support.semantic_tokens = params
      .capabilities
      .text_document
      .and_then(|text| text.semantic_tokens)
      .filter(|capability| {
        matches!(
          capability.requests.full,
          Some(
            lsp::SemanticTokensFullOptions::Bool(true)
              | lsp::SemanticTokensFullOptions::Delta { .. }
          )
        ) && capability.formats.contains(&lsp::TokenFormat::RELATIVE)
      })
      .map(|capability| legend(&capability));
    Ok(lsp::InitializeResult {
      capabilities: self.capabilities(),
      server_info: Some(lsp::ServerInfo {
        name: "shellsmith".into(),
        version: Some(env!("CARGO_PKG_VERSION").into()),
      }),
    })
  }

  fn capabilities(&self) -> lsp::ServerCapabilities {
    lsp::ServerCapabilities {
      position_encoding: Some(lsp::PositionEncodingKind::UTF16),
      text_document_sync: Some(lsp::TextDocumentSyncCapability::Options(
        lsp::TextDocumentSyncOptions {
          open_close: Some(true),
          change: Some(lsp::TextDocumentSyncKind::FULL),
          save: Some(lsp::TextDocumentSyncSaveOptions::Supported(true)),
          ..Default::default()
        },
      )),
      definition_provider: Some(lsp::OneOf::Left(true)),
      references_provider: Some(lsp::OneOf::Left(true)),
      rename_provider: Some(lsp::OneOf::Right(lsp::RenameOptions {
        prepare_provider: Some(true),
        work_done_progress_options: lsp::WorkDoneProgressOptions::default(),
      })),
      document_symbol_provider: Some(lsp::OneOf::Left(true)),
      document_formatting_provider: Some(lsp::OneOf::Left(true)),
      document_link_provider: Some(lsp::DocumentLinkOptions {
        resolve_provider: Some(false),
        work_done_progress_options: lsp::WorkDoneProgressOptions::default(),
      }),
      workspace_symbol_provider: Some(lsp::OneOf::Left(true)),
      workspace: Some(lsp::WorkspaceServerCapabilities {
        workspace_folders: Some(lsp::WorkspaceFoldersServerCapabilities {
          supported: Some(true),
          change_notifications: Some(lsp::OneOf::Left(true)),
        }),
        ..Default::default()
      }),
      semantic_tokens_provider: self.support.semantic_tokens.clone().map(
        |legend| {
          lsp::SemanticTokensOptions {
            legend,
            full: Some(lsp::SemanticTokensFullOptions::Bool(true)),
            ..Default::default()
          }
          .into()
        },
      ),
      ..Default::default()
    }
  }

  fn log(&self, message: impl std::fmt::Display) {
    log(&self.client, message);
  }

  fn initialized(&self) {
    if self.support.refreshes_tokens() {
      match self.session.updates() {
        Ok(mut updates) => {
          let client = self.client.clone();
          let shutdown = self.shutdown.clone();
          tokio::spawn(async move {
            while updates.changed().await.is_ok() {
              if shutdown.load(Ordering::Relaxed) {
                break;
              }
              refresh_tokens(&client).await;
            }
          });
        }
        Err(error) => self.log(error),
      }
    }
    if let Err(error) = self.session.start_index() {
      self.log(error);
    }
    if self.support.watch {
      let client = self.client.clone();
      let session = self.session.clone();
      tokio::spawn(async move {
        let options = lsp::DidChangeWatchedFilesRegistrationOptions {
          watchers: vec![lsp::FileSystemWatcher {
            glob_pattern: lsp::GlobPattern::String("**/*".into()),
            kind: Some(
              lsp::WatchKind::Create
                | lsp::WatchKind::Change
                | lsp::WatchKind::Delete,
            ),
          }],
        };
        let result = client
          .request::<req::RegisterCapability>(lsp::RegistrationParams {
            registrations: vec![lsp::Registration {
              id: "shellsmith-files".into(),
              method: "workspace/didChangeWatchedFiles".into(),
              register_options: Some(
                serde_json::to_value(options)
                  .expect("serializable registration"),
              ),
            }],
          })
          .await;
        if let Err(error) = result {
          log(&client, error);
        } else if let Err(error) = session
          .set_watching(true)
          .and_then(|()| session.start_index())
        {
          log(&client, error);
        }
      });
    }
  }

  fn updated(&self, result: Result<(), ResponseError>) {
    if let Err(error) = result {
      self.log(error);
      return;
    }
    if let Err(error) = self.session.start_index() {
      self.log(error);
    }
    if self.support.refreshes_tokens() {
      let client = self.client.clone();
      tokio::spawn(async move { refresh_tokens(&client).await });
    }
  }

  fn open(
    &mut self,
    params: lsp::DidOpenTextDocumentParams,
  ) -> Result<(), ResponseError> {
    let document = params.text_document;
    let path = self
      .session
      .document_path(&file_path(&document.uri)?)
      .map_err(fault)?;
    if self.opened.contains_key(&path)
      || self.opened_path(&document.uri).is_some()
    {
      return Err(invalid("Document is already open"));
    }
    self
      .session
      .set_overlay(&path, Some(document.text))
      .map_err(fault)?;
    self.opened.insert(
      path,
      OpenDocument {
        uri: document.uri,
        version: document.version,
      },
    );
    Ok(())
  }

  fn change(
    &mut self,
    params: lsp::DidChangeTextDocumentParams,
  ) -> Result<(), ResponseError> {
    file_path(&params.text_document.uri)?;
    let Some((path, document)) = self
      .opened
      .iter_mut()
      .find(|(_, document)| document.uri == params.text_document.uri)
    else {
      return Err(invalid("Document is not open"));
    };
    if params.text_document.version <= document.version {
      return Err(invalid("Outdated document version"));
    }
    if params
      .content_changes
      .iter()
      .any(|change| change.range.is_some())
    {
      return Err(invalid("Full document changes required"));
    }
    let text = params
      .content_changes
      .into_iter()
      .last()
      .ok_or_else(|| invalid("Missing content changes"))?
      .text;
    self.session.set_overlay(path, Some(text)).map_err(fault)?;
    document.version = params.text_document.version;
    Ok(())
  }

  fn close(
    &mut self,
    params: &lsp::DidCloseTextDocumentParams,
  ) -> Result<(), ResponseError> {
    file_path(&params.text_document.uri)?;
    let path = self
      .opened_path(&params.text_document.uri)
      .cloned()
      .ok_or_else(|| invalid("Document is not open"))?;
    self.session.set_overlay(&path, None).map_err(fault)?;
    self.opened.remove(&path);
    Ok(())
  }

  fn folders(
    &mut self,
    params: &lsp::DidChangeWorkspaceFoldersParams,
  ) -> Result<(), ResponseError> {
    let event = &params.event;
    let removed =
      self.file_paths(event.removed.iter().map(|folder| &folder.uri));
    let added = self.file_paths(event.added.iter().map(|folder| &folder.uri));
    for path in removed {
      self.session.remove_input(&path).map_err(fault)?;
    }
    for path in added {
      self.session.add_input(&path).map_err(fault)?;
    }
    Ok(())
  }

  /// Notifications cannot reject individual URIs; log invalid ones.
  fn file_paths<'a>(
    &self,
    uris: impl Iterator<Item = &'a Url>,
  ) -> Vec<PathBuf> {
    uris
      .filter_map(|uri| file_path(uri).map_err(|error| self.log(error)).ok())
      .collect()
  }

  fn opened_path(&self, uri: &Url) -> Option<&PathBuf> {
    self
      .opened
      .iter()
      .find_map(|(path, document)| (document.uri == *uri).then_some(path))
  }

  /// Rejects URIs retargeted since the document was opened.
  fn document_path(&self, uri: &Url) -> Result<PathBuf, ResponseError> {
    let requested = file_path(uri)?;
    if let Some(path) = self.opened_path(uri)
      && self.session.absolute(&requested).map_err(fault)? != *path
    {
      return Err(fault(Error::Superseded));
    }
    Ok(requested)
  }

  fn document_query<T: Send + 'static>(
    &self,
    uri: &Url,
    scope: fn(PathBuf) -> RequestScope,
    operation: impl FnOnce(
      &mut Application,
      DocumentId,
      &AtomicBool,
    ) -> Result<T, Error>
    + Send
    + 'static,
  ) -> Reply<T> {
    let path = match self.document_path(uri) {
      Ok(path) => path,
      Err(error) => return Box::pin(std::future::ready(Err(error))),
    };
    self.query(scope(path.clone()), move |application, cancelled| {
      let id = application.loaded_document(&path)?;
      operation(application, id, cancelled)
    })
  }

  fn name_query<T: Send + 'static>(
    &self,
    position: &lsp::TextDocumentPositionParams,
    operation: impl FnOnce(
      &mut Application,
      &Path,
      usize,
      &AtomicBool,
    ) -> Result<T, Error>
    + Send
    + 'static,
  ) -> Reply<T> {
    let offset = position.position;
    self.document_query(
      &position.text_document.uri,
      RequestScope::Names,
      move |application, id, cancelled| {
        let offset = document_offset(application, id, offset)?;
        let path = application
          .path(id)
          .ok_or_else(|| Error::Message("Unknown document path".into()))?
          .to_path_buf();
        operation(application, &path, offset, cancelled)
      },
    )
  }

  fn query<T: Send + 'static>(
    &self,
    scope: RequestScope,
    operation: impl FnOnce(&mut Application, &AtomicBool) -> Result<T, Error>
    + Send
    + 'static,
  ) -> Reply<T> {
    let job = self.session.request(scope, operation);
    let client = self.client.clone();
    let session = self.session.clone();
    Box::pin(async move {
      let outcome = job.await;
      if (outcome.result.is_ok()
        || matches!(outcome.result, Err(Error::Superseded)))
        && let Err(error) = session.start_index()
      {
        log(&client, error);
      }
      for failure in outcome.failures {
        log(
          &client,
          format!("{}: {}", failure.path.display(), failure.message),
        );
      }
      outcome.result.map_err(fault)
    })
  }

  fn semantic(
    &self,
    params: &lsp::SemanticTokensParams,
  ) -> Reply<Option<lsp::SemanticTokensResult>> {
    let Some(legend) = self.support.semantic_tokens.clone() else {
      return Box::pin(std::future::ready(Err(ResponseError::new(
        ErrorCode::METHOD_NOT_FOUND,
        "Semantic tokens were not negotiated",
      ))));
    };
    self.document_query(
      &params.text_document.uri,
      RequestScope::Analysis,
      move |application, id, cancelled| {
        let spans = application.classify(id, cancelled)?;
        semantic_tokens(application, id, &spans, &legend).map(Some)
      },
    )
  }

  fn definitions(
    &self,
    params: &lsp::GotoDefinitionParams,
  ) -> Reply<Option<lsp::GotoDefinitionResponse>> {
    let position = params.text_document_position_params.position;
    let opened = self.opened.clone();
    self.document_query(
      &params.text_document_position_params.text_document.uri,
      RequestScope::Results,
      move |application, id, cancelled| {
        let offset = document_offset(application, id, position)?;
        let definitions = application.definitions(id, offset, cancelled)?;
        if definitions.is_empty() {
          return Ok(None);
        }
        locations(application, &opened, &definitions)
          .map(|locations| Some(lsp::GotoDefinitionResponse::Array(locations)))
      },
    )
  }

  fn references(
    &self,
    params: &lsp::ReferenceParams,
  ) -> Reply<Option<Vec<lsp::Location>>> {
    let include_declaration = params.context.include_declaration;
    let opened = self.opened.clone();
    self.name_query(
      &params.text_document_position,
      move |application, path, offset, cancelled| {
        let references = application.references(
          path,
          offset,
          include_declaration,
          cancelled,
        )?;
        if references.is_empty() {
          return Ok(None);
        }
        locations(application, &opened, &references).map(Some)
      },
    )
  }

  fn prepare_rename(
    &self,
    params: &lsp::TextDocumentPositionParams,
  ) -> Reply<Option<lsp::PrepareRenameResponse>> {
    self.name_query(params, |application, path, offset, cancelled| {
      application
        .prepare_rename(path, offset, cancelled)?
        .map(|target| rename_range(application, target))
        .transpose()
    })
  }

  fn rename(
    &self,
    params: lsp::RenameParams,
  ) -> Reply<Option<lsp::WorkspaceEdit>> {
    let opened = self.opened.clone();
    let document_changes = self.support.document_changes;
    let new_name = params.new_name;
    self.name_query(
      &params.text_document_position,
      move |application, path, offset, cancelled| {
        let edits = application.rename(path, offset, &new_name, cancelled)?;
        workspace_edit(application, edits, &opened, document_changes).map(Some)
      },
    )
  }

  fn document_links(
    &self,
    params: &lsp::DocumentLinkParams,
  ) -> Reply<Option<Vec<lsp::DocumentLink>>> {
    let opened = self.opened.clone();
    self.document_query(
      &params.text_document.uri,
      RequestScope::Results,
      move |application, id, cancelled| {
        let links = application.document_links(id, cancelled)?;
        document_links(application, &opened, links).map(Some)
      },
    )
  }

  fn format(
    &self,
    params: &lsp::DocumentFormattingParams,
  ) -> Reply<Option<Vec<lsp::TextEdit>>> {
    self.document_query(
      &params.text_document.uri,
      RequestScope::Document,
      |application, id, cancelled| {
        formatting_edits(application, application.format(id, cancelled)?)
          .map(Some)
      },
    )
  }
}

async fn refresh_tokens(client: &ClientSocket) {
  if let Err(error) = client.request::<req::SemanticTokensRefresh>(()).await {
    log(client, error);
  }
}

fn log(client: &ClientSocket, message: impl std::fmt::Display) {
  let _ = client.notify::<notif::LogMessage>(lsp::LogMessageParams {
    typ: lsp::MessageType::ERROR,
    message: message.to_string(),
  });
}

fn router(
  session: Arc<Session>,
  client: ClientSocket,
  shutdown: Arc<AtomicBool>,
) -> Router<Server> {
  let mut router = Router::new(Server {
    session,
    client,
    opened: BTreeMap::new(),
    support: ClientSupport::default(),
    shutdown,
  });
  router
    .request::<req::Initialize, _>(|server, params| {
      std::future::ready(server.initialize(params))
    })
    .request::<req::Shutdown, _>(|server, ()| {
      let result = server.session.invalidate().map_err(fault);
      server.shutdown.store(true, Ordering::Relaxed);
      std::future::ready(result)
    })
    .request::<req::SemanticTokensFullRequest, _>(|server, params| {
      server.semantic(&params)
    })
    .request::<req::GotoDefinition, _>(|server, params| {
      server.definitions(&params)
    })
    .request::<req::References, _>(|server, params| server.references(&params))
    .request::<req::PrepareRenameRequest, _>(|server, params| {
      server.prepare_rename(&params)
    })
    .request::<req::Rename, _>(|server, params| server.rename(params))
    .request::<req::DocumentLinkRequest, _>(|server, params| {
      server.document_links(&params)
    })
    .request::<req::DocumentSymbolRequest, _>(|server, params| {
      let format = server.support.symbols;
      let opened = server.opened.clone();
      server.document_query(
        &params.text_document.uri,
        RequestScope::Document,
        move |application, id, _| {
          document_symbols(application, &opened, id, format).map(Some)
        },
      )
    })
    .request::<req::Formatting, _>(|server, params| server.format(&params))
    .request::<req::WorkspaceSymbolRequest, _>(|server, params| {
      let opened = server.opened.clone();
      server.query(RequestScope::Workspace, move |application, _| {
        symbols(
          application,
          &opened,
          application.workspace_symbols(&params.query),
        )
        .map(|symbols| Some(lsp::WorkspaceSymbolResponse::Flat(symbols)))
      })
    })
    .notification::<notif::Initialized>(|server, _| {
      server.initialized();
      ControlFlow::Continue(())
    })
    .notification::<notif::DidOpenTextDocument>(|server, params| {
      let result = server.open(params);
      server.updated(result);
      ControlFlow::Continue(())
    })
    .notification::<notif::DidChangeTextDocument>(|server, params| {
      let result = server.change(params);
      server.updated(result);
      ControlFlow::Continue(())
    })
    .notification::<notif::DidCloseTextDocument>(|server, params| {
      let result = server.close(&params);
      server.updated(result);
      ControlFlow::Continue(())
    })
    .notification::<notif::DidChangeWorkspaceFolders>(|server, params| {
      let result = server.folders(&params);
      server.updated(result);
      ControlFlow::Continue(())
    })
    .notification::<notif::DidChangeWatchedFiles>(|server, params| {
      let paths =
        server.file_paths(params.changes.iter().map(|event| &event.uri));
      let result = server.session.files_changed(&paths).map_err(fault);
      server.updated(result);
      ControlFlow::Continue(())
    })
    .notification::<notif::DidSaveTextDocument>(|server, params| {
      let result = file_path(&params.text_document.uri)
        .and_then(|path| server.session.files_changed(&[path]).map_err(fault));
      server.updated(result);
      ControlFlow::Continue(())
    })
    .unhandled_notification(|_, _| ControlFlow::Continue(()));
  router
}

#[derive(Clone, Copy, Default)]
enum SymbolFormat {
  #[default]
  Flat,
  Hierarchical,
}

#[derive(Clone)]
struct OpenDocument {
  uri: Url,
  version: i32,
}

fn invalid(message: impl std::fmt::Display) -> ResponseError {
  ResponseError::new(ErrorCode::INVALID_PARAMS, message)
}

fn fault(error: Error) -> ResponseError {
  let code = match error {
    Error::RenameRefused(_)
    | Error::FormattingRefused(_)
    | Error::Engine(engine::Error::AnalysisLimit) => ErrorCode::REQUEST_FAILED,
    Error::Superseded => ErrorCode::CONTENT_MODIFIED,
    Error::InvalidName(_) | Error::Engine(engine::Error::InvalidPosition) => {
      ErrorCode::INVALID_PARAMS
    }
    Error::Engine(engine::Error::Cancelled) => ErrorCode::REQUEST_CANCELLED,
    _ => ErrorCode::INTERNAL_ERROR,
  };
  ResponseError::new(code, error)
}

fn file_path(uri: &Url) -> Result<PathBuf, ResponseError> {
  if uri.scheme() != "file" || uri.query().is_some() || uri.fragment().is_some()
  {
    return Err(invalid("Expected a file URI"));
  }
  uri
    .to_file_path()
    .map_err(|()| invalid("Unsupported file URI"))
}

fn formatting_edits(
  application: &Application,
  edits: Vec<FormatEdit>,
) -> Result<Vec<lsp::TextEdit>, Error> {
  let mut positions = Positions::new(application);
  edits
    .into_iter()
    .map(|edit| {
      Ok(lsp::TextEdit {
        range: positions.result_range(&edit.location)?,
        new_text: edit.new_text,
      })
    })
    .collect()
}

fn uri(
  application: &Application,
  opened: &BTreeMap<PathBuf, OpenDocument>,
  path: &Path,
) -> Result<Url, Error> {
  if let Some(document) = opened.get(path) {
    if document
      .uri
      .to_file_path()
      .ok()
      .is_none_or(|client_path| application.absolute(&client_path) != path)
    {
      return Err(Error::Superseded);
    }
    return Ok(document.uri.clone());
  }
  Url::from_file_path(path)
    .map_err(|()| Error::Message("Cannot encode file URI".into()))
}

fn document_source(
  application: &Application,
  id: DocumentId,
) -> Result<&str, Error> {
  Ok(
    application
      .workspace()
      .document(id)
      .ok_or(engine::Error::UnknownDocument(id))?
      .source(),
  )
}

fn document_offset(
  application: &Application,
  id: DocumentId,
  position: lsp::Position,
) -> Result<usize, Error> {
  Ok(
    Lines::new(document_source(application, id)?)
      .offset(position)
      .ok_or(engine::Error::InvalidPosition)?,
  )
}

fn workspace_edit(
  application: &Application,
  edits: Vec<RenameEdit>,
  opened: &BTreeMap<PathBuf, OpenDocument>,
  document_changes: bool,
) -> Result<lsp::WorkspaceEdit, Error> {
  let mut documents: BTreeMap<PathBuf, Vec<lsp::TextEdit>> = BTreeMap::new();
  let mut positions = Positions::new(application);
  for edit in edits {
    let range = positions.result_range(&edit.location)?;
    let path = positions.path(edit.location.snapshot.document)?;
    documents
      .entry(path.to_path_buf())
      .or_default()
      .push(lsp::TextEdit {
        range,
        new_text: edit.new_text,
      });
  }
  let mut output = lsp::WorkspaceEdit::default();
  if document_changes {
    output.document_changes = Some(lsp::DocumentChanges::Edits(
      documents
        .into_iter()
        .map(|(path, edits)| {
          let document = opened.get(&path);
          Ok(lsp::TextDocumentEdit {
            text_document: lsp::OptionalVersionedTextDocumentIdentifier {
              uri: uri(application, opened, &path)?,
              version: document.map(|document| document.version),
            },
            edits: edits.into_iter().map(lsp::OneOf::Left).collect(),
          })
        })
        .collect::<Result<_, Error>>()?,
    ));
  } else {
    output.changes = Some(
      documents
        .into_iter()
        .map(|(path, edits)| Ok((uri(application, opened, &path)?, edits)))
        .collect::<Result<_, Error>>()?,
    );
  }
  Ok(output)
}

fn rename_range(
  application: &Application,
  target: RenameTarget,
) -> Result<lsp::PrepareRenameResponse, Error> {
  Ok(lsp::PrepareRenameResponse::RangeWithPlaceholder {
    range: Positions::new(application).result_range(&target.location)?,
    placeholder: target.placeholder,
  })
}

fn document_links(
  application: &Application,
  opened: &BTreeMap<PathBuf, OpenDocument>,
  links: Vec<(engine::Location, PathBuf)>,
) -> Result<Vec<lsp::DocumentLink>, Error> {
  let mut positions = Positions::new(application);
  links
    .into_iter()
    .map(|(location, path)| {
      Ok(lsp::DocumentLink {
        range: positions.result_range(&location)?,
        target: Some(uri(application, opened, &path)?),
        tooltip: None,
        data: None,
      })
    })
    .collect()
}

fn locations(
  application: &Application,
  opened: &BTreeMap<PathBuf, OpenDocument>,
  locations: &[engine::Location],
) -> Result<Vec<lsp::Location>, Error> {
  let mut positions = Positions::new(application);
  locations
    .iter()
    .map(|location| {
      let range = positions.result_range(location)?;
      let path = positions.path(location.snapshot.document)?;
      Ok(lsp::Location {
        uri: uri(application, opened, path)?,
        range,
      })
    })
    .collect()
}

#[expect(
  deprecated,
  reason = "SymbolInformation requires initializing its deprecated field; None omits it from the response."
)]
fn symbols<'a>(
  application: &Application,
  opened: &BTreeMap<PathBuf, OpenDocument>,
  locations: impl Iterator<Item = SymbolDetails<'a>>,
) -> Result<Vec<lsp::SymbolInformation>, Error> {
  let mut symbols = Vec::new();
  let mut positions = Positions::new(application);
  for entry in locations {
    let symbol = entry.symbol;
    let location = entry.location;
    let Some(range) = positions.range(&location)? else {
      continue;
    };
    let path = positions.path(location.snapshot.document)?;
    symbols.push(lsp::SymbolInformation {
      name: String::from_utf8_lossy(&symbol.name).into_owned(),
      kind: symbol_kind(symbol.kind),
      location: lsp::Location {
        uri: uri(application, opened, path)?,
        range,
      },
      tags: None,
      deprecated: None,
      container_name: entry
        .container
        .map(|parent| String::from_utf8_lossy(&parent.name).into_owned()),
    });
  }
  Ok(symbols)
}

fn symbol_kind(kind: SymbolKind) -> lsp::SymbolKind {
  match kind {
    SymbolKind::Function => lsp::SymbolKind::FUNCTION,
    SymbolKind::Variable => lsp::SymbolKind::VARIABLE,
  }
}

#[expect(
  deprecated,
  reason = "DocumentSymbol requires initializing its deprecated field; None omits it from the response."
)]
fn document_symbols(
  application: &Application,
  opened: &BTreeMap<PathBuf, OpenDocument>,
  id: DocumentId,
  format: SymbolFormat,
) -> Result<lsp::DocumentSymbolResponse, Error> {
  let entries = application.document_symbols(id)?;
  if matches!(format, SymbolFormat::Flat) {
    return symbols(application, opened, entries.map(|entry| entry.details))
      .map(lsp::DocumentSymbolResponse::Flat);
  }
  let lines = Lines::new(document_source(application, id)?);
  let entries: Vec<_> = entries.collect();
  let mut children: BTreeMap<Option<usize>, Vec<lsp::DocumentSymbol>> =
    BTreeMap::new();
  for (index, entry) in entries.into_iter().enumerate().rev() {
    let details = entry.details;
    let mut nested = children.remove(&Some(index)).unwrap_or_default();
    let Some((range, selection_range)) = lines
      .range(&details.range.as_ref().unwrap_or(&details.location).bytes)
      .zip(lines.range(&details.location.bytes))
    else {
      children.entry(entry.parent).or_default().extend(nested);
      continue;
    };
    nested.reverse();
    children
      .entry(entry.parent)
      .or_default()
      .push(lsp::DocumentSymbol {
        name: String::from_utf8_lossy(&details.symbol.name).into_owned(),
        detail: None,
        kind: symbol_kind(details.symbol.kind),
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children: (!nested.is_empty()).then_some(nested),
      });
  }
  let mut roots = children.remove(&None).unwrap_or_default();
  roots.reverse();
  Ok(lsp::DocumentSymbolResponse::Nested(roots))
}

const TYPES: &[&str] = &[
  "comment",
  "keyword",
  "operator",
  "string",
  "number",
  "variable",
  "parameter",
  "function",
  "command",
];
const MODIFIERS: &[&str] = &[
  "declaration",
  "definition",
  "modification",
  "readonly",
  "defaultLibrary",
  "environment",
];

struct Positions<'a> {
  application: &'a Application,
  lines: BTreeMap<DocumentId, Lines<'a>>,
  paths: BTreeMap<DocumentId, &'a Path>,
}

impl<'a> Positions<'a> {
  const fn new(application: &'a Application) -> Self {
    Self {
      application,
      lines: BTreeMap::new(),
      paths: BTreeMap::new(),
    }
  }

  fn range(
    &mut self,
    location: &engine::Location,
  ) -> Result<Option<lsp::Range>, Error> {
    let id = location.snapshot.document;
    let lines = match self.lines.entry(id) {
      std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
      std::collections::btree_map::Entry::Vacant(entry) => {
        entry.insert(Lines::new(document_source(self.application, id)?))
      }
    };
    Ok(lines.range(&location.bytes))
  }

  fn result_range(
    &mut self,
    location: &engine::Location,
  ) -> Result<lsp::Range, Error> {
    self
      .range(location)?
      .ok_or_else(|| Error::Message("Invalid result range".into()))
  }

  fn path(&mut self, id: DocumentId) -> Result<&'a Path, Error> {
    if let Some(path) = self.paths.get(&id) {
      return Ok(path);
    }
    let path = self
      .application
      .path(id)
      .ok_or_else(|| Error::Message("Unknown document path".into()))?;
    self.paths.insert(id, path);
    Ok(path)
  }
}

struct Lines<'a> {
  source: &'a str,
  starts: Vec<usize>,
  ends: Vec<usize>,
  excess: Vec<(usize, usize)>,
}

impl<'a> Lines<'a> {
  fn new(source: &'a str) -> Self {
    let mut total = 0;
    let excess = source
      .char_indices()
      .filter_map(|(offset, character)| {
        if character.is_ascii() {
          return None;
        }
        total += character.len_utf8() - character.len_utf16();
        Some((offset + character.len_utf8(), total))
      })
      .collect();
    let bytes = source.as_bytes();
    let mut starts = vec![0];
    let mut ends = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
      if matches!(bytes[offset], b'\r' | b'\n') {
        ends.push(offset);
        if bytes[offset] == b'\r' && bytes.get(offset + 1) == Some(&b'\n') {
          offset += 1;
        }
        starts.push(offset + 1);
      }
      offset += 1;
    }
    ends.push(source.len());
    Self {
      source,
      starts,
      ends,
      excess,
    }
  }

  fn offset(&self, position: lsp::Position) -> Option<usize> {
    let line = usize::try_from(position.line).ok()?;
    let Some(&start) = self.starts.get(line) else {
      return Some(self.source.len());
    };
    let end = self.ends[line];
    let target = usize::try_from(position.character).ok()?;
    let mut column = 0;
    for (offset, character) in self.source[start..end].char_indices() {
      if column == target {
        return Some(start + offset);
      }
      column += character.len_utf16();
      if column > target {
        return None;
      }
    }
    Some(end)
  }

  fn range(&self, bytes: &std::ops::Range<usize>) -> Option<lsp::Range> {
    let (sl, sc) = self.position(bytes.start)?;
    let (el, ec) = self.position(bytes.end)?;
    Some(lsp::Range::new(
      lsp::Position::new(sl, sc),
      lsp::Position::new(el, ec),
    ))
  }

  fn excess_at(&self, offset: usize) -> usize {
    let index = self.excess.partition_point(|(end, _)| *end <= offset);
    index.checked_sub(1).map_or(0, |index| self.excess[index].1)
  }

  /// Nonempty segments as (line, UTF-16 column, UTF-16 length).
  fn segments(
    &self,
    bytes: std::ops::Range<usize>,
  ) -> impl Iterator<Item = (u32, u32, u32)> + '_ {
    let mut offset = bytes.start;
    std::iter::from_fn(move || {
      while offset < bytes.end {
        let row = self.starts.partition_point(|start| *start <= offset) - 1;
        let text =
          &self.source[offset..bytes.end.min(self.ends[row]).max(offset)];
        let position = self.position(offset);
        offset = self
          .starts
          .get(row + 1)
          .copied()
          .unwrap_or(bytes.end)
          .min(bytes.end);
        if let Some((line, column)) = position
          && let Ok(length) = u32::try_from(text.encode_utf16().count())
          && length > 0
        {
          return Some((line, column, length));
        }
      }
      None
    })
  }

  fn position(&self, offset: usize) -> Option<(u32, u32)> {
    if !self.source.is_char_boundary(offset) {
      return None;
    }
    let line = self
      .starts
      .partition_point(|start| *start <= offset)
      .checked_sub(1)?;
    let offset = offset.min(self.ends[line]);
    Some((
      u32::try_from(line).ok()?,
      u32::try_from(
        offset
          - self.starts[line]
          - (self.excess_at(offset) - self.excess_at(self.starts[line])),
      )
      .ok()?,
    ))
  }
}

fn legend(
  capability: &lsp::SemanticTokensClientCapabilities,
) -> lsp::SemanticTokensLegend {
  lsp::SemanticTokensLegend {
    token_types: TYPES
      .iter()
      .filter(|name| {
        capability
          .token_types
          .iter()
          .any(|kind| kind.as_str() == **name)
      })
      .map(|name| lsp::SemanticTokenType::new(name))
      .collect(),
    token_modifiers: MODIFIERS
      .iter()
      .filter(|name| {
        capability
          .token_modifiers
          .iter()
          .any(|kind| kind.as_str() == **name)
      })
      .map(|name| lsp::SemanticTokenModifier::new(name))
      .collect(),
  }
}

fn semantic_tokens(
  application: &Application,
  id: DocumentId,
  spans: &[SemanticSpan],
  legend: &lsp::SemanticTokensLegend,
) -> Result<lsp::SemanticTokensResult, Error> {
  Ok(lsp::SemanticTokensResult::Tokens(lsp::SemanticTokens {
    result_id: None,
    data: encode(document_source(application, id)?, spans, legend),
  }))
}

fn encode(
  source: &str,
  spans: &[SemanticSpan],
  legend: &lsp::SemanticTokensLegend,
) -> Vec<lsp::SemanticToken> {
  let encoded: Vec<_> = spans
    .iter()
    .filter(|span| {
      let range = &span.location.bytes;
      !range.is_empty()
        && source.is_char_boundary(range.start)
        && source.is_char_boundary(range.end)
    })
    .filter_map(|span| {
      let kind = legend
        .token_types
        .iter()
        .position(|name| name.as_str() == token_kind(span.kind))?;
      let bits = span
        .modifiers
        .iter()
        .filter_map(|modifier| {
          legend
            .token_modifiers
            .iter()
            .position(|name| name.as_str() == token_modifier(*modifier))
        })
        .fold(0_u32, |bits, index| bits | (1 << index));
      Some((span, u32::try_from(kind).ok()?, bits))
    })
    .collect();
  let mut events: BTreeMap<usize, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
  for (index, (span, ..)) in encoded.iter().enumerate() {
    let range = &span.location.bytes;
    events.entry(range.start).or_default().0.push(index);
    events.entry(range.end).or_default().1.push(index);
  }
  let priority = |index: usize| {
    let span = encoded[index].0;
    (
      span.semantic,
      std::cmp::Reverse(span.location.bytes.len()),
      index,
    )
  };
  let mut active = BTreeSet::new();
  let mut ranges = Vec::new();
  let mut previous = 0;
  for (offset, (starts, ends)) in events {
    if previous < offset
      && let Some(&(_, _, index)) = active.last()
    {
      ranges.push((previous, offset, index));
    }
    for index in ends {
      active.remove(&priority(index));
    }
    for index in starts {
      active.insert(priority(index));
    }
    previous = offset;
  }
  let lines = Lines::new(source);
  let mut tokens: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
  for (start, end, index) in ranges {
    let (_, kind, bits) = encoded[index];
    for (line, column, length) in lines.segments(start..end) {
      if let Some(last) = tokens.last_mut()
        && last.0 == line
        && last.1 + last.2 == column
        && last.3 == kind
        && last.4 == bits
      {
        last.2 += length;
      } else {
        tokens.push((line, column, length, kind, bits));
      }
    }
  }
  encode_relative(tokens)
}

fn encode_relative(
  tokens: Vec<(u32, u32, u32, u32, u32)>,
) -> Vec<lsp::SemanticToken> {
  let mut encoded = Vec::with_capacity(tokens.len());
  let (mut last_line, mut last_column) = (0, 0);
  for (line, column, length, kind, bits) in tokens {
    encoded.push(lsp::SemanticToken {
      delta_line: line - last_line,
      delta_start: if line == last_line {
        column - last_column
      } else {
        column
      },
      length,
      token_type: kind,
      token_modifiers_bitset: bits,
    });
    last_line = line;
    last_column = column;
  }
  encoded
}

const fn token_kind(kind: SemanticKind) -> &'static str {
  match kind {
    SemanticKind::Comment => "comment",
    SemanticKind::Keyword => "keyword",
    SemanticKind::Operator => "operator",
    SemanticKind::String => "string",
    SemanticKind::Number => "number",
    SemanticKind::Variable => "variable",
    SemanticKind::Parameter => "parameter",
    SemanticKind::Function => "function",
    SemanticKind::Command => "command",
  }
}

const fn token_modifier(modifier: Modifier) -> &'static str {
  match modifier {
    Modifier::Declaration => "declaration",
    Modifier::Definition => "definition",
    Modifier::Modification => "modification",
    Modifier::Readonly => "readonly",
    Modifier::DefaultLibrary => "defaultLibrary",
    Modifier::Environment => "environment",
  }
}

#[cfg(test)]
#[path = "../../tests/implementation/transport.rs"]
mod tests;
