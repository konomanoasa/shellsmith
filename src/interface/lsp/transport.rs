use super::server;
use crate::interface::application::{Error, Session, Settings};
use async_lsp::lsp_types::notification::Notification;
use async_lsp::lsp_types::{self as lsp, notification as notif};
use async_lsp::{
  AnyEvent, AnyNotification, AnyRequest, ErrorCode, LspService, MainLoop,
  RequestId, ResponseError,
};
use futures_util::future::{AbortHandle, Abortable, BoxFuture};
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tower::{Service, ServiceBuilder};

pub async fn run(
  settings: Settings,
  input: impl tokio::io::AsyncRead + Unpin,
  output: impl tokio::io::AsyncWrite + Unpin,
) -> Result<bool, Error> {
  let session = Arc::new(Session::new(settings)?);
  let shutdown = Arc::new(AtomicBool::new(false));
  let ended = shutdown.clone();
  let (main_loop, _) = MainLoop::new_server(move |client| {
    ServiceBuilder::new()
      .layer(async_lsp::server::LifecycleLayer::default())
      .service(Requests::new(server::router(session, client, shutdown)))
  });
  main_loop
    .run_buffered(input.compat(), output.compat_write())
    .await
    .map_err(|error| Error::Message(error.to_string()))?;
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

// Keep notifications flowing while application workers wait for CPU capacity.
struct Requests<S> {
  inner: S,
  pending: HashMap<RequestId, (AbortHandle, Weak<()>)>,
}

impl<S> Requests<S> {
  fn new(inner: S) -> Self {
    Self {
      inner,
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
    if notification.method != notif::Cancel::METHOD {
      return self.inner.notify(notification);
    }
    let params: lsp::CancelParams =
      match serde_json::from_value(notification.params) {
        Ok(params) => params,
        Err(error) => return ControlFlow::Break(Err(error.into())),
      };
    if let Some((handle, _)) = self.pending.get(&params.id) {
      handle.abort();
    }
    ControlFlow::Continue(())
  }

  fn emit(&mut self, event: AnyEvent) -> ControlFlow<async_lsp::Result<()>> {
    self.inner.emit(event)
  }
}

#[cfg(test)]
#[path = "../../../tests/implementation/transport.rs"]
mod tests;
