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
  let mut requests = Requests::new(router);
  let request = || {
    serde_json::from_value::<AnyRequest>(serde_json::json!({
      "id":7, "method":"shutdown", "params":null
    }))
    .unwrap()
  };
  let first = requests.call(request());
  let duplicate = requests.call(request()).await.unwrap_err();
  assert_eq!(duplicate.code, ErrorCode::INVALID_REQUEST);
  finish_first.send(()).unwrap();
  assert_eq!(first.await.unwrap(), serde_json::Value::Null);
  let second = requests.call(request());
  finish_second.send(()).unwrap();
  assert_eq!(second.await.unwrap(), serde_json::Value::Null);
}
