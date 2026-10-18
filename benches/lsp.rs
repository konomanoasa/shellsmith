use serde_json::{Value, json};
use shellsmith::interface::application::Settings;
use shellsmith::interface::application::engine::InitialEnvironment;
#[path = "../tests/common/lsp.rs"]
mod lsp_client;
use lsp_client::Client;
use std::path::PathBuf;
use std::time::Instant;
use url::Url;

struct Fixture(PathBuf);
impl Drop for Fixture {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.0);
  }
}

fn main() {
  let args: Vec<_> = std::env::args()
    .skip(1)
    .filter(|arg| arg != "--bench")
    .collect();
  assert!(args.is_empty() || args.len() == 2, "usage: lsp [CASE SIZE]");
  let cases = if args.len() == 2 {
    vec![(args[0].clone(), args[1].parse::<usize>().unwrap())]
  } else {
    vec![
      ("lines".into(), 1200),
      ("lines".into(), 4800),
      ("long_line".into(), 1200),
      ("long_line".into(), 4800),
      ("documents".into(), 100),
      ("documents".into(), 400),
      ("workspace_tokens".into(), 100),
      ("workspace_tokens".into(), 400),
      ("sources".into(), 100),
      ("sources".into(), 400),
      ("concurrent_requests".into(), 1200),
      ("concurrent_requests".into(), 4800),
      ("rename".into(), 1200),
      ("rename".into(), 4800),
    ]
  };
  let runtime = tokio::runtime::Runtime::new().unwrap();
  runtime.block_on(async {
    for (case, size) in cases {
      measure(&case, size).await;
    }
  });
}

fn fixture(case: &str, size: usize) -> (Fixture, String) {
  assert!(size > 0, "fixture size must be positive");
  assert!(
    matches!(
      case,
      "lines"
        | "long_line"
        | "documents"
        | "workspace_tokens"
        | "sources"
        | "concurrent_requests"
        | "rename"
    ),
    "unknown fixture: {case}"
  );
  let fixture = Fixture(
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("target")
      .join(format!("lsp-benchmark-{}", std::process::id())),
  );
  std::fs::create_dir_all(&fixture.0).unwrap();
  let source = if case == "sources" {
    for index in 0..size {
      let body = if index + 1 < size {
        format!("value={index}\n. ./lib{}.sh\n", index + 1)
      } else {
        format!("value={index}\n")
      };
      std::fs::write(fixture.0.join(format!("lib{index}.sh")), body).unwrap();
    }
    "x=1\n. ./lib0.sh\n: \"$value\"\n".into()
  } else if case == "concurrent_requests" {
    std::fs::write(fixture.0.join("lib.sh"), "library=1\n").unwrap();
    format!(". ./lib.sh\nx=1\n{}", ": \"$x\"\n".repeat(size))
  } else if case == "long_line" {
    "x=1; : \"$x\"; ".repeat(size)
  } else {
    "x=1; : \"$x\"\n".repeat(
      if matches!(case, "documents" | "workspace_tokens") {
        1
      } else {
        size
      },
    )
  };
  let file = fixture.0.join("main.sh");
  std::fs::write(&file, &source).unwrap();
  if matches!(case, "documents" | "workspace_tokens") {
    for index in 1..size {
      std::fs::write(fixture.0.join(format!("{index}.sh")), &source).unwrap();
    }
  }
  (fixture, source)
}

async fn measure(case: &str, size: usize) {
  let (fixture, source) = fixture(case, size);
  let file = fixture.0.join("main.sh");
  let uri = Url::from_file_path(&file).unwrap().to_string();
  let startup = Instant::now();
  let mut settings = Settings::new(fixture.0.clone());
  settings.initial = InitialEnvironment::empty();
  let mut server = Client::new(settings);
  server.request("initialize", json!({"rootUri":Url::from_file_path(&fixture.0).unwrap().to_string(),"capabilities":{"textDocument":{"semanticTokens":{
    "requests":{"full":true},"formats":["relative"],"tokenTypes":["comment","keyword","operator","string","number","variable","parameter","function","command"],
    "tokenModifiers":["declaration","definition","modification","readonly","defaultLibrary"]}}}})).await;
  server.notify("initialized", json!({})).await;
  let (method, params) = if case == "documents" {
    ("workspace/symbol", json!({"query":"x"}))
  } else if case == "rename" {
    (
      "textDocument/rename",
      json!({"textDocument":{"uri":uri},"position":{"line":0,"character":0},"newName":"value"}),
    )
  } else {
    (
      "textDocument/semanticTokens/full",
      json!({"textDocument":{"uri":uri}}),
    )
  };
  let started = Instant::now();
  server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"shellscript","version":1,"text":source}})).await;
  let response = request(&mut server, method, &params, case).await;
  assert!(response.get("error").is_none(), "{response}");
  let initial = started.elapsed();
  let startup = startup.elapsed();
  let changed = source.replacen("x=1", "x=2", 1);
  let started = Instant::now();
  server.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":changed}]})).await;
  let response = request(&mut server, method, &params, case).await;
  let update = started.elapsed();
  assert!(response.get("error").is_none(), "{response}");
  let count = if case == "rename" {
    response["result"]["changes"]
      .as_object()
      .unwrap()
      .values()
      .map(|edits| edits.as_array().unwrap().len())
      .sum()
  } else {
    response["result"]["data"].as_array().map_or_else(
      || response["result"].as_array().map_or(0, Vec::len),
      |data| data.len() / 5,
    )
  };
  println!(
    "{}",
    json!({"case":case,"size":size,"initial_ms":initial.as_secs_f64()*1000.0,"startup_ms":startup.as_secs_f64()*1000.0,"update_ms":update.as_secs_f64()*1000.0,"items":count})
  );
  std::hint::black_box::<Value>(response);
  assert_eq!(server.take_messages(), [] as [Value; 0]);
}

async fn request(
  server: &mut Client,
  method: &str,
  params: &Value,
  case: &str,
) -> Value {
  if case != "concurrent_requests" {
    return server.request(method, params.clone()).await;
  }
  for (id, method) in [
    ("tokens", method),
    ("links", "textDocument/documentLink"),
    ("tokens-again", method),
  ] {
    server
      .send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
      .await;
  }
  let mut tokens: Option<Value> = None;
  for _ in 0..3 {
    let response = server.receive().await;
    assert!(response.get("error").is_none(), "{response}");
    if response["id"] == "links" {
      assert_eq!(response["result"].as_array().unwrap().len(), 1);
    } else {
      if let Some(previous) = &tokens {
        assert_eq!(response["result"], previous["result"]);
      }
      tokens = Some(response);
    }
  }
  tokens.unwrap()
}
