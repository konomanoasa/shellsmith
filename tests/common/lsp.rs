use serde_json::{Value, json};
use shellsmith::interface::application::{Error, Settings};
use tokio::io::{
  AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream,
  ReadHalf, WriteHalf,
};
use tokio::task::JoinHandle;

pub struct Client {
  input: WriteHalf<DuplexStream>,
  output: BufReader<ReadHalf<DuplexStream>>,
  task: JoinHandle<Result<bool, Error>>,
  next_id: u64,
  messages: Vec<Value>,
}

impl Client {
  pub fn new(settings: Settings) -> Self {
    let (client, server) = tokio::io::duplex(65536);
    let (input, output) = tokio::io::split(server);
    let task =
      tokio::spawn(shellsmith::interface::lsp::run(settings, input, output));
    let (output, input) = tokio::io::split(client);
    Self {
      input,
      output: BufReader::new(output),
      task,
      next_id: 0,
      messages: Vec::new(),
    }
  }

  pub async fn send(&mut self, message: Value) {
    let bytes = serde_json::to_vec(&message).unwrap();
    self.send_body(&bytes).await;
  }

  pub async fn send_body(&mut self, bytes: &[u8]) {
    self
      .send_bytes(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
      .await;
    self.send_bytes(bytes).await;
  }

  pub async fn send_bytes(&mut self, bytes: &[u8]) {
    self.input.write_all(bytes).await.unwrap();
  }

  pub async fn receive(&mut self) -> Value {
    let message = self.receive_unanswered().await;
    if message.get("method").is_some() && message.get("id").is_some() {
      self
        .send(json!({"jsonrpc":"2.0","id":message["id"],"result":null}))
        .await;
    }
    message
  }

  pub async fn receive_unanswered(&mut self) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
      let mut line = String::new();
      self.output.read_line(&mut line).await.unwrap();
      let length = line
        .trim()
        .strip_prefix("Content-Length: ")
        .unwrap()
        .parse()
        .unwrap();
      line.clear();
      self.output.read_line(&mut line).await.unwrap();
      assert_eq!(line, "\r\n");
      let mut bytes = vec![0; length];
      self.output.read_exact(&mut bytes).await.unwrap();
      serde_json::from_slice(&bytes).unwrap()
    })
    .await
    .expect("LSP response timed out")
  }

  pub async fn request(&mut self, method: &str, params: Value) -> Value {
    self.next_id += 1;
    let id = self.next_id;
    self
      .send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
      .await;
    loop {
      let message = self.receive().await;
      if message.get("method").is_none() && message["id"] == id {
        return message;
      }
      self.messages.push(message);
    }
  }

  pub async fn notify(&mut self, method: &str, params: Value) {
    self
      .send(json!({"jsonrpc":"2.0","method":method,"params":params}))
      .await;
  }

  pub fn take_messages(&mut self) -> Vec<Value> {
    std::mem::take(&mut self.messages)
  }
}

impl Drop for Client {
  fn drop(&mut self) {
    self.task.abort();
  }
}
