#[path = "behavior/application.rs"]
mod application;
#[path = "behavior/engine"]
mod engine {
  mod resolution;
  mod semantic_tokens;
  mod values;
  mod workspace;
}
#[path = "behavior/interface.rs"]
mod interface;

#[path = "common/fixtures.rs"]
mod fixtures;
#[path = "common/lsp.rs"]
mod lsp_client;
#[path = "common/parse.rs"]
mod support;
