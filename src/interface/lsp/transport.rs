use super::server;
use crate::interface::application::{Error, Session, Settings};
use async_lsp::lsp_types::notification::Notification;
use async_lsp::lsp_types::request::Request;
use async_lsp::lsp_types::{
  self as lsp, notification as notif, request as req,
};
use async_lsp::{
  AnyEvent, AnyNotification, AnyRequest, ClientSocket, ErrorCode, LspService,
  MainLoop, RequestId, ResponseError,
};
use futures_util::future::{AbortHandle, Abortable, BoxFuture, try_join};
use futures_util::{TryStreamExt, stream};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tower::{Service, ServiceBuilder};

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
      .service(server::router(session, client.clone(), shutdown));
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
          Err(Rejection::Log(message)) => server::log(&client, message),
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
        server::log(&self.client, format!("{method}: {error}"));
        ControlFlow::Continue(())
      }
      ControlFlow::Break(Err(async_lsp::Error::Protocol(error)))
        if method != notif::Exit::METHOD =>
      {
        server::log(&self.client, format!("{method}: {error}"));
        ControlFlow::Continue(())
      }
      result => result,
    }
  }

  fn emit(&mut self, event: AnyEvent) -> ControlFlow<async_lsp::Result<()>> {
    self.inner.emit(event)
  }
}

#[cfg(test)]
#[path = "../../../tests/implementation/transport.rs"]
mod tests;
