use super::*;
use async_lsp::lsp_types::request as req;
use async_lsp::router::Router;

#[tokio::test]
async fn duplicate_active_ids_are_rejected_without_cancelling_the_original_and_can_be_reused_after_completion()
 {
  let (finish_first, first) = tokio::sync::oneshot::channel::<()>();
  let (finish_second, second) = tokio::sync::oneshot::channel::<()>();
  let mut router =
    Router::new(std::collections::VecDeque::from([first, second]));
  router.request::<req::Shutdown, _>(|receivers, ()| {
    let finish = receivers
      .pop_front()
      .expect("one receiver per accepted request");
    async move {
      finish.await.unwrap();
      Ok(())
    }
  });
  let mut requests = Requests::new(router, ClientSocket::new_closed());
  let request = || {
    serde_json::from_value::<AnyRequest>(serde_json::json!({
      "id":7, "method":"shutdown", "params":null
    }))
    .unwrap()
  };
  let first = requests.call(request());
  let duplicate = requests.call(request()).await.unwrap_err();
  assert_eq!(duplicate.code, ErrorCode::INVALID_REQUEST);
  for params in [json!({}), json!({"id":null}), json!({"id":7.5})] {
    let notification = serde_json::from_value::<AnyNotification>(json!({
      "method":"$/cancelRequest", "params":params,
    }))
    .unwrap();
    assert!(requests.notify(notification).is_continue());
  }
  finish_first.send(()).unwrap();
  assert_eq!(first.await.unwrap(), serde_json::Value::Null);
  let second = requests.call(request());
  finish_second.send(()).unwrap();
  assert_eq!(second.await.unwrap(), serde_json::Value::Null);
}

#[tokio::test]
async fn error_replies_and_service_messages_remain_complete_under_output_backpressure()
 {
  async fn receive(output: &mut BufReader<tokio::io::DuplexStream>) -> Value {
    let (body, header_error) = read_frame(output).await.unwrap();
    assert_eq!(header_error, None);
    serde_json::from_slice(&body).unwrap()
  }
  tokio::time::timeout(std::time::Duration::from_secs(30), async {
    let (mut input, server_input) = tokio::io::duplex(4096);
    let (output, server_output) = tokio::io::duplex(1);
    let server = tokio::spawn(run(Settings::new(std::env::current_dir().unwrap()), server_input, server_output));
    let mut output = BufReader::new(output);
    input.write_all(&frame(br#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"capabilities":{}}}"#)).await.unwrap();
    assert_eq!(receive(&mut output).await["id"], "init");
    let mut bytes = frame(br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
    for id in 0..4 {
      bytes.extend(frame(b"{"));
      bytes.extend(frame(&serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":"unknown"})).unwrap()));
      bytes.extend(frame(br#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{}}"#));
    }
    input.write_all(&bytes).await.unwrap();
    let mut parse_errors = 0;
    let mut logs = 0;
    let mut ids = Vec::new();
    for _ in 0..12 {
      let message = receive(&mut output).await;
      if message["method"] == "window/logMessage" {
        assert!(message.get("id").is_none());
        logs += 1;
      } else if message["id"].is_null() {
        assert_eq!(message["error"]["code"], -32700);
        parse_errors += 1;
      } else {
        assert_eq!(message["error"]["code"], -32601);
        ids.push(message["id"].as_u64().unwrap());
      }
    }
    ids.sort_unstable();
    assert_eq!((parse_errors, logs, ids), (4, 4, vec![0, 1, 2, 3]));
    input.write_all(&frame(br#"{"jsonrpc":"2.0","id":"stop","method":"shutdown"}"#)).await.unwrap();
    assert_eq!(receive(&mut output).await["id"], "stop");
    input.write_all(&frame(br#"{"jsonrpc":"2.0","method":"exit"}"#)).await.unwrap();
    assert!(server.await.unwrap().unwrap());
  }).await.expect("LSP output stalled");
}

#[tokio::test]
async fn framed_error_responses_are_written_before_reporting_input_eof() {
  let settings = Settings::new(std::env::current_dir().unwrap());
  let input = b"Content-Length: 1\r\n\r\n{".as_slice();
  let (output, server_output) = tokio::io::duplex(1024);
  let error = run(settings, input, server_output).await.unwrap_err();
  assert!(error.to_string().contains("Incomplete LSP header"));
  let (body, header_error) =
    read_frame(&mut BufReader::new(output)).await.unwrap();
  assert_eq!(header_error, None);
  let response: Value = serde_json::from_slice(&body).unwrap();
  assert_eq!(response["error"]["code"], -32700);
  assert_eq!(response["id"], Value::Null);
}

struct BrokenIo;

impl tokio::io::AsyncRead for BrokenIo {
  fn poll_read(
    self: Pin<&mut Self>,
    _: &mut Context<'_>,
    _: &mut tokio::io::ReadBuf<'_>,
  ) -> Poll<io::Result<()>> {
    Poll::Ready(Err(io::Error::other("read failed")))
  }
}

impl tokio::io::AsyncWrite for BrokenIo {
  fn poll_write(
    self: Pin<&mut Self>,
    _: &mut Context<'_>,
    _: &[u8],
  ) -> Poll<io::Result<usize>> {
    Poll::Ready(Err(io::Error::other("write failed")))
  }
  fn poll_flush(
    self: Pin<&mut Self>,
    _: &mut Context<'_>,
  ) -> Poll<io::Result<()>> {
    Poll::Ready(Ok(()))
  }
  fn poll_shutdown(
    self: Pin<&mut Self>,
    _: &mut Context<'_>,
  ) -> Poll<io::Result<()>> {
    Poll::Ready(Ok(()))
  }
}

#[tokio::test]
async fn input_and_output_failures_are_fatal_for_normal_and_error_responses() {
  let settings = || Settings::new(std::env::current_dir().unwrap());
  let error = run(settings(), BrokenIo, tokio::io::sink())
    .await
    .unwrap_err();
  assert!(error.to_string().contains("read failed"));
  for body in [
    b"{".as_slice(),
    br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#,
  ] {
    let (mut input, server_input) = tokio::io::duplex(1024);
    input.write_all(&frame(body)).await.unwrap();
    let error = run(settings(), server_input, BrokenIo).await.unwrap_err();
    assert!(error.to_string().contains("write failed"), "{error}");
  }
}
