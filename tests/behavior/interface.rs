mod lsp {
  use crate::fixtures;
  use crate::lsp_client;
  use fixtures::Fixture;
  use lsp_client::Client;
  use serde_json::{Value, json};
  use shellsmith::interface::application::Settings;
  use url::Url;

  async fn initialize(server: &mut Client, command: bool) -> Value {
    let mut types = vec![
      "comment",
      "keyword",
      "operator",
      "string",
      "number",
      "variable",
      "parameter",
      "function",
    ];
    if command {
      types.push("command");
    }
    let response = server.request("initialize", json!({"capabilities":{"textDocument":{"semanticTokens":{
      "requests":{"full":true},"formats":["relative"],"tokenTypes":types,"tokenModifiers":["declaration","definition","modification","readonly","defaultLibrary"]}}}})).await;
    server.notify("initialized", json!({})).await;
    response
  }

  #[tokio::test]
  async fn completed_workspace_analysis_refreshes_tokens_after_caller_changes()
  {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "a() { :; }\nb() { a; }\n");
    let caller = fixture.write("main.sh", ". ./lib.sh; b");
    let library_uri = Url::from_file_path(&library).unwrap().to_string();
    let caller_uri = Url::from_file_path(&caller).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    server.request("initialize", json!({
      "workspaceFolders":[{"uri":Url::from_file_path(&fixture.root).unwrap(),"name":"fixture"}],
      "capabilities":{
        "workspace":{"semanticTokens":{"refreshSupport":true}},
        "textDocument":{"semanticTokens":{"requests":{"full":true},"formats":["relative"],"tokenTypes":["function","command"],"tokenModifiers":[]}}
      }
    })).await;
    server.notify("initialized", json!({})).await;
    assert_eq!(
      server.receive().await["method"],
      "workspace/semanticTokens/refresh"
    );
    let tokens = decoded(
      &request(
        &mut server,
        "textDocument/semanticTokens/full",
        &library_uri,
      )
      .await,
    );
    assert!(tokens.contains(&(1, 6, 1, 0, 0)), "{tokens:?}");
    let selection = json!({"textDocument":{"uri":library_uri},"position":{"line":1,"character":6}});
    let definitions = server
      .request("textDocument/definition", selection.clone())
      .await;
    assert_eq!(
      definitions["result"],
      json!([{"uri":library_uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}])
    );
    let references = server.request("textDocument/references", json!({"textDocument":{"uri":library_uri},"position":{"line":1,"character":6},"context":{"includeDeclaration":false}})).await;
    assert_eq!(
      references["result"],
      json!([{"uri":library_uri,"range":{"start":{"line":1,"character":6},"end":{"line":1,"character":7}}}])
    );

    for (source, expected) in [
      (". ./lib.sh; unset -f a; b", 1),
      (". ./lib.sh; a() { echo replacement; }; b", 0),
    ] {
      fixture.write("main.sh", source);
      server
        .notify(
          "workspace/didChangeWatchedFiles",
          json!({"changes":[{"uri":caller_uri,"type":2}]}),
        )
        .await;
      for _ in 0..2 {
        assert_eq!(
          server.receive().await["method"],
          "workspace/semanticTokens/refresh"
        );
      }
      let tokens = decoded(
        &request(
          &mut server,
          "textDocument/semanticTokens/full",
          &library_uri,
        )
        .await,
      );
      assert!(tokens.contains(&(1, 6, 1, expected, 0)), "{tokens:?}");
      let definitions = server
        .request("textDocument/definition", selection.clone())
        .await;
      let references = server.request("textDocument/references", json!({"textDocument":{"uri":library_uri},"position":{"line":1,"character":6},"context":{"includeDeclaration":false}})).await;
      if expected == 1 {
        assert_eq!(definitions["result"], Value::Null);
        assert_eq!(references["result"], Value::Null);
      } else {
        let start = source.find("a()").unwrap();
        assert_eq!(
          definitions["result"],
          json!([{"uri":caller_uri,"range":{"start":{"line":0,"character":start},"end":{"line":0,"character":start + 1}}}])
        );
        assert_eq!(
          references["result"],
          json!([{"uri":library_uri,"range":{"start":{"line":1,"character":6},"end":{"line":1,"character":7}}}])
        );
      }
    }
  }

  #[tokio::test]
  async fn references_advertise_support_and_preserve_utf16_ranges_and_inclusion()
   {
    let fixture = Fixture::new();
    let path = fixture.write("main.sh", "disk=ignored");
    let uri = Url::from_file_path(path).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    let initialized = initialize(&mut server, true).await;
    assert_eq!(
      initialized["result"]["capabilities"]["referencesProvider"],
      true
    );
    let source =
      ": '😀'; value=1\r\n: \"${value}\"\r\nvalue=2\r\n: \"$value\"\r\n";
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"sh","version":1,"text":source}})).await;
    let location = |line, start, end| json!({"uri":uri,"range":{"start":{"line":line,"character":start},"end":{"line":line,"character":end}}});
    for include in [false, true] {
      let response = server.request("textDocument/references", json!({"textDocument":{"uri":uri},"position":{"line":0,"character":8},"context":{"includeDeclaration":include}})).await;
      let expected = if include {
        json!([
          location(0, 8, 13),
          location(1, 5, 10),
          location(2, 0, 5),
          location(3, 4, 9)
        ])
      } else {
        json!([location(1, 5, 10), location(3, 4, 9)])
      };
      assert_eq!(response["result"], expected, "{response}");
    }
    let empty = server.request("textDocument/references", json!({"textDocument":{"uri":uri},"position":{"line":1,"character":3},"context":{"includeDeclaration":true}})).await;
    assert_eq!(empty["result"], Value::Null);
    assert!(empty.get("error").is_none());
    server.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"value=3"}]})).await;
    for (include, expected) in
      [(false, Value::Null), (true, json!([location(0, 0, 5)]))]
    {
      let response = server.request("textDocument/references", json!({"textDocument":{"uri":uri},"position":{"line":0,"character":0},"context":{"includeDeclaration":include}})).await;
      assert_eq!(response["result"], expected, "{response}");
    }
  }

  #[tokio::test]
  async fn references_log_unreadable_inputs_and_fail_for_unavailable_requested_documents()
   {
    let fixture = Fixture::new();
    let path = fixture.write("main.sh", "value=1; : \"$value\"");
    std::fs::write(fixture.root.join("broken.sh"), [0xff]).unwrap();
    let uri = Url::from_file_path(path).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    server.request("initialize", json!({"rootUri":Url::from_file_path(&fixture.root).unwrap(),"capabilities":{}})).await;
    server.notify("initialized", json!({})).await;
    let response = server.request("textDocument/references", json!({"textDocument":{"uri":uri},"position":{"line":0,"character":0},"context":{"includeDeclaration":false}})).await;
    assert_eq!(
      response["result"],
      json!([{"uri":uri,"range":{"start":{"line":0,"character":13},"end":{"line":0,"character":18}}}]),
      "{response}"
    );
    let message = log_message(&mut server).await;
    assert!(
      message["params"]["message"]
        .as_str()
        .unwrap()
        .contains("broken.sh")
    );
    let uri = Url::from_file_path(fixture.root.join("missing.sh")).unwrap();
    let response = server.request("textDocument/references", json!({"textDocument":{"uri":uri},"position":{"line":0,"character":0},"context":{"includeDeclaration":false}})).await;
    assert!(response.get("error").is_some(), "{response}");
  }

  #[tokio::test]
  async fn symbol_hierarchy_negotiates_format_and_preserves_utf16_ranges_and_containers()
   {
    fn range(line: u32, start: u32, end: u32) -> Value {
      json!({"start":{"line":line,"character":start},"end":{"line":line,"character":end}})
    }
    let fixture = Fixture::new();
    let source = ": '😀'; outer() { x=1; inner() { y=2; }; }; # first\r\nouter() { z=3; }; # second\r\n";
    let path = fixture.write("main.sh", source);
    let uri = Url::from_file_path(path).unwrap().to_string();
    for hierarchical in [false, true] {
      let mut server = Client::new(Settings::new(fixture.root.clone()));
      server.request("initialize", json!({"capabilities":{"textDocument":{"documentSymbol":{"hierarchicalDocumentSymbolSupport":hierarchical}}}})).await;
      server.notify("initialized", json!({})).await;
      server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"sh","version":1,"text":source}})).await;
      let response =
        request(&mut server, "textDocument/documentSymbol", &uri).await;
      if hierarchical {
        assert_eq!(
          response["result"],
          json!([
            {"name":"outer","kind":12,"range":range(0,8,42),"selectionRange":range(0,8,13),"children":[
              {"name":"x","kind":13,"range":range(0,18,21),"selectionRange":range(0,18,19)},
              {"name":"inner","kind":12,"range":range(0,23,39),"selectionRange":range(0,23,28),"children":[
                {"name":"y","kind":13,"range":range(0,33,36),"selectionRange":range(0,33,34)}
              ]}
            ]},
            {"name":"outer","kind":12,"range":range(1,0,16),"selectionRange":range(1,0,5),"children":[
              {"name":"z","kind":13,"range":range(1,10,13),"selectionRange":range(1,10,11)}
            ]}
          ])
        );
      } else {
        assert_eq!(
          response["result"],
          json!([
            {"name":"outer","kind":12,"location":{"uri":uri,"range":range(0,8,13)}},
            {"name":"x","kind":13,"location":{"uri":uri,"range":range(0,18,19)},"containerName":"outer"},
            {"name":"inner","kind":12,"location":{"uri":uri,"range":range(0,23,28)},"containerName":"outer"},
            {"name":"y","kind":13,"location":{"uri":uri,"range":range(0,33,34)},"containerName":"inner"},
            {"name":"outer","kind":12,"location":{"uri":uri,"range":range(1,0,5)}},
            {"name":"z","kind":13,"location":{"uri":uri,"range":range(1,10,11)},"containerName":"outer"}
          ])
        );
      }
      assert_eq!(
        server
          .request("workspace/symbol", json!({"query":"y"}))
          .await["result"],
        json!([
          {"name":"y","kind":13,"location":{"uri":uri,"range":range(0,33,34)},"containerName":"inner"}
        ])
      );
      server.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"y=9"}]})).await;
      let updated =
        request(&mut server, "textDocument/documentSymbol", &uri).await;
      let expected = if hierarchical {
        json!({"name":"y","kind":13,"range":range(0,0,3),"selectionRange":range(0,0,1)})
      } else {
        json!({"name":"y","kind":13,"location":{"uri":uri,"range":range(0,0,1)}})
      };
      assert_eq!(updated["result"], json!([expected]));
      assert_eq!(
        server
          .request("workspace/symbol", json!({"query":"y"}))
          .await["result"],
        json!([
          {"name":"y","kind":13,"location":{"uri":uri,"range":range(0,0,1)}}
        ])
      );
    }
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn locations_and_links_use_open_uris_and_refresh_them_on_close_and_reopen()
   {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "value=disk\n");
    let main = fixture.write("main.sh", ". ./lib.sh\n: \"$value\"\n");
    let first = fixture.root.join("first alias.sh");
    let second = fixture.root.join("second alias.sh");
    for alias in [&first, &second] {
      std::os::unix::fs::symlink(&library, alias).unwrap();
    }
    let main_uri = Url::from_file_path(main).unwrap().to_string();
    let library_uri = Url::from_file_path(library).unwrap().to_string();
    let first_uri = Url::from_file_path(first).unwrap().to_string();
    let second_uri = Url::from_file_path(second).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    let root_uri = Url::from_file_path(&fixture.root).unwrap();
    server.request("initialize", json!({"capabilities":{},"workspaceFolders":[{"uri":root_uri,"name":"fixture"}]})).await;
    server.notify("initialized", json!({})).await;
    let mut opened = None;
    for (next, expected_uri, line) in [
      (None, &library_uri, 0),
      (Some(&first_uri), &first_uri, 1),
      (None, &library_uri, 0),
      (Some(&second_uri), &second_uri, 1),
    ] {
      if let Some(previous) = opened {
        server
          .notify(
            "textDocument/didClose",
            json!({"textDocument":{"uri":previous}}),
          )
          .await;
      }
      if let Some(uri) = next {
        server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"sh","version":1,"text":"\nvalue=buffer\n"}})).await;
      }
      opened = next;
      let location = json!({"uri":expected_uri,"range":{"start":{"line":line,"character":0},"end":{"line":line,"character":5}}});
      let definitions = server.request("textDocument/definition", json!({"textDocument":{"uri":main_uri},"position":{"line":1,"character":4}})).await;
      assert_eq!(definitions["result"], json!([location]), "{definitions}");
      let references = server.request("textDocument/references", json!({"textDocument":{"uri":expected_uri},"position":{"line":line,"character":0},"context":{"includeDeclaration":true}})).await;
      assert_eq!(
        references["result"],
        json!([location, {"uri":main_uri,"range":{"start":{"line":1,"character":4},"end":{"line":1,"character":9}}}]),
        "{references}"
      );
      let links =
        request(&mut server, "textDocument/documentLink", &main_uri).await;
      assert_eq!(
        links["result"],
        json!([{"range":{"start":{"line":0,"character":2},"end":{"line":0,"character":10}},"target":expected_uri}]),
        "{links}"
      );
      let symbols = json!([{"name":"value","kind":13,"location":location}]);
      let document =
        request(&mut server, "textDocument/documentSymbol", &library_uri).await;
      assert_eq!(document["result"], symbols, "{document}");
      let workspace = server
        .request("workspace/symbol", json!({"query":"value"}))
        .await;
      assert_eq!(workspace["result"], symbols, "{workspace}");
    }
  }

  #[tokio::test]
  async fn rename_preserves_utf16_versions_original_uri_and_revalidates_after_prepare()
   {
    let fixture = Fixture::new();
    let path = fixture.write("main.sh", "disk=ignored");
    let alias = fixture.root.join("alias.sh");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    #[cfg(not(unix))]
    let alias = path;
    let uri = Url::from_file_path(alias).unwrap().to_string();
    for document_changes in [false, true] {
      let mut server = Client::new(Settings::new(fixture.root.clone()));
      let initialized = server.request("initialize", json!({"capabilities":{"workspace":{"workspaceEdit":{"documentChanges":document_changes}}}})).await;
      assert_eq!(
        initialized["result"]["capabilities"]["renameProvider"]["prepareProvider"],
        true
      );
      server.notify("initialized", json!({})).await;
      server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"sh","version":7,"text":": '😀'; x=1\r\n: ${x}\r\n"}})).await;
      let params =
        json!({"textDocument":{"uri":uri},"position":{"line":1,"character":4}});
      let prepared = server
        .request("textDocument/prepareRename", params.clone())
        .await;
      assert_eq!(
        prepared["result"],
        json!({"range":{"start":{"line":1,"character":4},"end":{"line":1,"character":5}},"placeholder":"x"})
      );
      let mut rename = params.clone();
      rename["newName"] = json!("value");
      let response =
        server.request("textDocument/rename", rename.clone()).await;
      let edits = json!([
        {"range":{"start":{"line":0,"character":8},"end":{"line":0,"character":9}},"newText":"value"},
        {"range":{"start":{"line":1,"character":4},"end":{"line":1,"character":5}},"newText":"value"}
      ]);
      if document_changes {
        assert_eq!(
          response["result"],
          json!({"documentChanges":[{"textDocument":{"uri":uri,"version":7},"edits":edits}]})
        );
      } else {
        assert_eq!(response["result"]["changes"][&uri], edits);
      }
      rename["newName"] = json!("not-a-name");
      assert_eq!(
        server.request("textDocument/rename", rename.clone()).await["error"]["code"],
        -32602
      );
      let invalid =
        json!({"textDocument":{"uri":uri},"position":{"line":0,"character":4}});
      assert_eq!(
        server.request("textDocument/prepareRename", invalid).await["error"]["code"],
        -32602
      );
      let delimiter =
        json!({"textDocument":{"uri":uri},"position":{"line":1,"character":2}});
      assert_eq!(
        server
          .request("textDocument/prepareRename", delimiter)
          .await["result"],
        Value::Null
      );
      server.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":8},"contentChanges":[{"text":": '😀'; x=1\r\n: ${x}; value=2\r\n"}]})).await;
      rename["newName"] = json!("value");
      let response = server.request("textDocument/rename", rename).await;
      assert_eq!(response["error"]["code"], -32803);
      assert!(response.get("result").is_none());
    }
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn requests_refuse_an_open_uri_whose_symlink_target_has_changed() {
    let fixture = Fixture::new();
    let original = fixture.write("original.sh", "x=1; : $x");
    let replacement = fixture.write("replacement.sh", "unrelated=2");
    let directory = fixture.root.join("directory");
    std::fs::create_dir(&directory).unwrap();
    let alias = fixture.root.join("alias.sh");
    std::os::unix::fs::symlink(&original, &alias).unwrap();
    let uri = Url::from_file_path(&alias).unwrap().to_string();
    let main = fixture.write("main.sh", ". ./original.sh\n: $x\n");
    let main_uri = Url::from_file_path(main).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, false).await;
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"sh","version":1,"text":"x=1; : $x"}})).await;
    server
      .request(
        "textDocument/prepareRename",
        json!({"textDocument":{"uri":uri},"position":{"line":0,"character":0}}),
      )
      .await;
    let document = json!({"textDocument":{"uri":uri}});
    let position =
      json!({"textDocument":{"uri":uri},"position":{"line":0,"character":8}});
    let original = Url::from_file_path(original).unwrap().to_string();
    let requests = [
      ("textDocument/semanticTokens/full", document.clone()),
      ("textDocument/documentSymbol", document.clone()),
      ("textDocument/documentLink", document),
      ("textDocument/definition", position.clone()),
      ("textDocument/prepareRename", position),
      (
        "textDocument/rename",
        json!({"textDocument":{"uri":uri},"position":{"line":0,"character":0},"newName":"y"}),
      ),
      (
        "textDocument/rename",
        json!({"textDocument":{"uri":original},"position":{"line":0,"character":0},"newName":"y"}),
      ),
      (
        "textDocument/definition",
        json!({"textDocument":{"uri":main_uri},"position":{"line":1,"character":3}}),
      ),
      (
        "textDocument/documentLink",
        json!({"textDocument":{"uri":main_uri}}),
      ),
      (
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":original}}),
      ),
      ("workspace/symbol", json!({"query":"x"})),
    ];
    for target in [&directory, &replacement] {
      std::fs::remove_file(&alias).unwrap();
      std::os::unix::fs::symlink(target, &alias).unwrap();
      for (method, params) in &requests {
        let response = server.request(method, params.clone()).await;
        assert_eq!(
          response["error"]["code"], -32801,
          "{target:?} {method} {params}"
        );
        assert!(
          response.get("result").is_none(),
          "{target:?} {method} {params}"
        );
      }
    }
    server
      .notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}))
      .await;
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"sh","version":2,"text":"z=2; : $z"}})).await;
    let response = server
      .request(
        "textDocument/prepareRename",
        json!({"textDocument":{"uri":uri},"position":{"line":0,"character":0}}),
      )
      .await;
    assert_eq!(response["result"]["placeholder"], "z");
  }

  #[tokio::test]
  async fn rename_returns_all_source_file_edits_without_preparing_first() {
    let fixture = Fixture::new();
    let main = fixture.write("main.sh", ". ./lib.sh\nf\n");
    let library = fixture.write("lib.sh", "f() { :; }\n");
    let main_uri = Url::from_file_path(main).unwrap().to_string();
    let library_uri = Url::from_file_path(library).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, false).await;
    let response = server.request("textDocument/rename", json!({"textDocument":{"uri":main_uri},"position":{"line":1,"character":0},"newName":"run"})).await;
    assert_eq!(
      response["result"]["changes"][&main_uri],
      json!([{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":1}},"newText":"run"}])
    );
    assert_eq!(
      response["result"]["changes"][&library_uri],
      json!([{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"run"}])
    );
  }

  #[tokio::test]
  async fn definitions_use_current_utf16_positions_and_reject_split_surrogates()
  {
    let fixture = Fixture::new();
    let source =
      ": '😀'; x=1 # CRLF\r\n: \"${x}\" # CRLF\r\nexport x # CRLF\r\n";
    let path = fixture.write("main.sh", source);
    let uri = Url::from_file_path(&path).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    let initialized = initialize(&mut server, true).await;
    assert_eq!(
      initialized["result"]["capabilities"]["definitionProvider"],
      true
    );
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"sh","version":1,"text":source}})).await;
    let expected = json!([{"uri":uri,"range":{"start":{"line":0,"character":8},"end":{"line":0,"character":9}}}]);
    for (line, character) in [(1, 5), (2, 7)] {
      let response = server.request("textDocument/definition", json!({"textDocument":{"uri":uri},"position":{"line":line,"character":character}})).await;
      assert_eq!(response["result"], expected, "{line}:{character}");
    }
    let delimiter = server
      .request(
        "textDocument/definition",
        json!({"textDocument":{"uri":uri},"position":{"line":1,"character":3}}),
      )
      .await;
    assert_eq!(delimiter["result"], Value::Null);
    let invalid = server
      .request(
        "textDocument/definition",
        json!({"textDocument":{"uri":uri},"position":{"line":0,"character":4}}),
      )
      .await;
    assert_eq!(invalid["error"]["code"], -32602);
    server.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"x=2\r\n: \"$x\"\r\n"}]})).await;
    let updated = server
      .request(
        "textDocument/definition",
        json!({"textDocument":{"uri":uri},"position":{"line":1,"character":4}}),
      )
      .await;
    assert_eq!(
      updated["result"],
      json!([{"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}])
    );
  }

  #[tokio::test]
  async fn definitions_return_all_complete_candidates_and_no_partial_candidate_list()
   {
    let fixture = Fixture::new();
    let source = "if condition; then\nx=1\nelse\nx=2\nfi\nexport x\n";
    let path = fixture.write("main.sh", source);
    let uri = Url::from_file_path(&path).unwrap().to_string();
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial =
      shellsmith::interface::application::engine::InitialEnvironment::empty();
    let mut server = Client::new(settings);
    initialize(&mut server, false).await;
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"sh","version":1,"text":source}})).await;
    let complete = server
      .request(
        "textDocument/definition",
        json!({"textDocument":{"uri":uri},"position":{"line":5,"character":7}}),
      )
      .await;
    assert_eq!(
      complete["result"],
      json!([
        {"uri":uri,"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":1}}},
        {"uri":uri,"range":{"start":{"line":3,"character":0},"end":{"line":3,"character":1}}}
      ])
    );
    server.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"if condition; then\nx=1\nfi\nexport x\n"}]})).await;
    let incomplete = server
      .request(
        "textDocument/definition",
        json!({"textDocument":{"uri":uri},"position":{"line":3,"character":7}}),
      )
      .await;
    assert_eq!(incomplete["result"], Value::Null);
  }

  #[tokio::test]
  async fn trap_bodies_project_utf16_tokens_without_inventing_bindings_or_origins()
   {
    let fixture = Fixture::new();
    let source = "f() { :; }; trap 'printf \"😀\"; f' EXIT; trap '(inside() { :; }; inside)' TERM; trap \"$BODY\" EXIT";
    let file = fixture.write("main.sh", source);
    let uri = Url::from_file_path(&file).unwrap().to_string();
    let mut settings = Settings::new(fixture.root.clone());
    settings
      .initial
      .environment
      .insert(b"BODY".to_vec(), b"hidden_name() { :; }".to_vec());
    let mut server = Client::new(settings);
    initialize(&mut server, true).await;
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"sh","version":1,"text":source}})).await;
    let symbols =
      request(&mut server, "textDocument/documentSymbol", &uri).await;
    assert_eq!(
      symbols["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|symbol| symbol["name"].as_str().unwrap())
        .collect::<Vec<_>>(),
      ["f", "inside"]
    );
    let workspace = server
      .request("workspace/symbol", json!({"query":"inside"}))
      .await;
    assert_eq!(workspace["result"].as_array().unwrap().len(), 1);
    let column = |offset| source[..offset].encode_utf16().count();
    let call = column(source.rfind("inside").unwrap());
    let definition = column(source.find("inside").unwrap());
    let response = server.request("textDocument/definition", json!({"textDocument":{"uri":uri},"position":{"line":0,"character":call}})).await;
    assert_eq!(
      response["result"][0]["range"]["start"]["character"],
      definition
    );
    let tokens = decoded(
      &request(&mut server, "textDocument/semanticTokens/full", &uri).await,
    );
    for (text, kind) in [("inside", 7), ("BODY", 5), ("f' EXIT", 7)] {
      let start = source.rfind(text).unwrap();
      let column =
        u64::try_from(source[..start].encode_utf16().count()).unwrap();
      assert!(
        tokens.iter().any(|token| token.0 == 0
          && token.1 <= column
          && token.1 + token.2 > column
          && token.3 == kind),
        "{text}: {tokens:?}"
      );
    }
    let response =
      request(&mut server, "textDocument/documentSymbol", &uri).await;
    assert_eq!(
      response["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|symbol| symbol["name"].as_str().unwrap())
        .collect::<Vec<_>>(),
      vec!["f", "inside"]
    );
    let edits = server.request("textDocument/rename", json!({"textDocument":{"uri":uri},"position":{"line":0,"character":call},"newName":"renamed"})).await;
    assert_eq!(
      edits["result"]["changes"][&uri].as_array().unwrap().len(),
      2
    );
    let response = server
      .request("workspace/symbol", json!({"query":"hidden_name"}))
      .await;
    assert_eq!(response["result"], json!([]));
    assert!(
      server
        .take_messages()
        .iter()
        .all(|message| !message.to_string().contains("hidden_name"))
    );
  }
  async fn request(server: &mut Client, method: &str, uri: &str) -> Value {
    server
      .request(method, json!({"textDocument":{"uri":uri}}))
      .await
  }
  async fn notify(server: &mut Client, method: &str, params: &Value) {
    server.notify(method, params.clone()).await;
  }
  async fn log_message(server: &mut Client) -> Value {
    if let Some(message) = server
      .take_messages()
      .into_iter()
      .find(|message| message["method"] == "window/logMessage")
    {
      return message;
    }
    loop {
      let message = server.receive().await;
      if message["method"] == "window/logMessage" {
        return message;
      }
      assert_eq!(message["method"], "workspace/semanticTokens/refresh");
    }
  }
  fn decoded(response: &Value) -> Vec<(u64, u64, u64, u64, u64)> {
    let data = response["result"]["data"]
      .as_array()
      .unwrap_or_else(|| panic!("{response}"));
    let (mut line, mut column) = (0, 0);
    let mut result = Vec::new();
    for token in data.as_chunks::<5>().0 {
      let delta = token[0].as_u64().unwrap();
      line += delta;
      column = if delta == 0 {
        column + token[1].as_u64().unwrap()
      } else {
        token[1].as_u64().unwrap()
      };
      result.push((
        line,
        column,
        token[2].as_u64().unwrap(),
        token[3].as_u64().unwrap(),
        token[4].as_u64().unwrap(),
      ));
    }
    result
  }

  #[tokio::test]
  async fn semantic_tokens_use_utf16_and_standard_modifiers_with_custom_command()
   {
    let fixture = Fixture::new();
    let path = fixture.write(
      "test.sh",
      "# 日本😀\r\nreadonly x=1\r\nf() { :; }; f\necho \"$x\" \"${x}\" $y ${y}\n",
    );
    let uri = Url::from_file_path(&path).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    let initialized = initialize(&mut server, true).await;
    assert_eq!(
      initialized["result"]["capabilities"]["positionEncoding"],
      "utf-16"
    );
    let tokens = decoded(
      &request(&mut server, "textDocument/semanticTokens/full", &uri).await,
    );
    assert!(tokens.contains(&(0, 0, 6, 0, 0)), "{tokens:?}");
    assert!(tokens.contains(&(1, 0, 8, 8, 16)), "{tokens:?}");
    assert!(tokens.contains(&(1, 9, 1, 5, 15)), "{tokens:?}");
    assert!(
      tokens.iter().any(|&(line, column, _, kind, mods)| line == 2
        && column == 12
        && kind == 7
        && mods == 0),
      "{tokens:?}"
    );
    assert_eq!(
      tokens
        .iter()
        .copied()
        .filter(|token| token.0 == 3 && token.3 == 5)
        .collect::<Vec<_>>(),
      [
        (3, 7, 1, 5, 8),
        (3, 13, 1, 5, 8),
        (3, 18, 1, 5, 0),
        (3, 22, 1, 5, 0)
      ]
    );
    for pair in tokens.windows(2) {
      assert!(pair[0].0 < pair[1].0 || pair[0].1 + pair[0].2 <= pair[1].1);
    }
    assert!(!initialized.to_string().contains("regexp"));
  }

  #[tokio::test]
  async fn argument_roles_reach_lsp_without_recoloring_quotes_or_expansions() {
    let fixture = Fixture::new();
    let file = fixture.write(
      "main.sh",
      "exit '12'\nexit \"$status\"\nexec 'cat' file\nreadonly 'name'\nf() { return '3'; }\n",
    );
    let uri = Url::from_file_path(&file).unwrap().to_string();
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial =
      shellsmith::interface::application::engine::InitialEnvironment::empty();
    let mut server = Client::new(settings);
    initialize(&mut server, true).await;
    let tokens = decoded(
      &request(&mut server, "textDocument/semanticTokens/full", &uri).await,
    );
    for expected in [
      (0, 6, 2, 4, 0),
      (1, 7, 6, 5, 0),
      (2, 6, 3, 8, 0),
      (3, 10, 4, 5, 9),
      (4, 14, 1, 4, 0),
    ] {
      assert!(tokens.contains(&expected), "{expected:?}: {tokens:?}");
    }
    assert_eq!(tokens.iter().filter(|token| token.3 == 4).count(), 2);
    for quote in [5, 8] {
      assert!(
        !tokens.iter().any(|&(line, column, length, _, _)| line == 0
          && column <= quote
          && quote < column + length),
        "{tokens:?}"
      );
    }
  }

  #[tokio::test]
  async fn environment_modifier_is_negotiated_and_source_values_reach_lsp() {
    let fixture = Fixture::new();
    let file = fixture.write(
      "main.sh",
      "base=$HOME\nfile=$base/lib.sh\nset -eu; false; exec replacement; . \"$file\"\nf\n: \"$HOME\"\n",
    );
    fixture.write("lib.sh", "f() { :; }\n");
    let uri = Url::from_file_path(&file).unwrap().to_string();
    for supported in [true, false] {
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial.environment.clear();
      settings.initial.environment.insert(
        b"HOME".to_vec(),
        fixture.root.as_os_str().as_encoded_bytes().to_vec(),
      );
      let mut server = Client::new(settings);
      let modifiers: &[&str] = if supported { &["environment"] } else { &[] };
      let result = server.request("initialize", json!({"capabilities":{"textDocument":{"semanticTokens":{
        "requests":{"full":true},"formats":["relative"],"tokenTypes":["variable","function","command"],"tokenModifiers":modifiers}}}})).await;
      server.notify("initialized", json!({})).await;
      assert_eq!(
        result["result"]["capabilities"]["semanticTokensProvider"]["legend"]["tokenModifiers"],
        json!(modifiers)
      );
      let tokens = decoded(
        &request(&mut server, "textDocument/semanticTokens/full", &uri).await,
      );
      assert!(tokens.contains(&(3, 0, 1, 1, 0)), "{tokens:?}");
      assert!(tokens.contains(&(2, 38, 4, 0, 0)), "{tokens:?}");
      assert!(
        tokens.contains(&(4, 4, 4, 0, u64::from(supported))),
        "{tokens:?}"
      );
    }
  }

  #[tokio::test]
  async fn unsupported_command_is_omitted_without_function_fallback() {
    let fixture = Fixture::new();
    let path = fixture.write("test.sh", "echo hi\nf() { :; }; f\n");
    let uri = Url::from_file_path(&path).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, false).await;
    let tokens = decoded(
      &request(&mut server, "textDocument/semanticTokens/full", &uri).await,
    );
    assert!(!tokens.iter().any(|token| token.0 == 0 && token.1 == 0));
    assert_eq!(tokens.iter().filter(|token| token.3 == 7).count(), 2);
  }

  #[tokio::test]
  async fn full_sync_versions_close_and_workspace_symbols_share_current_documents()
   {
    let fixture = Fixture::new();
    let path = fixture.write("test.sh", "disk=1");
    let uri = Url::from_file_path(&path).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, true).await;
    notify(&mut server,"textDocument/didOpen",&json!({"textDocument":{"uri":uri,"languageId":"shellscript","version":1,"text":"live=1"}})).await;
    let result =
      request(&mut server, "textDocument/documentSymbol", &uri).await;
    assert_eq!(result["result"][0]["name"], "live");
    notify(&mut server,"textDocument/didChange",&json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"latest=1"}]})).await;
    notify(&mut server,"textDocument/didChange",&json!({"textDocument":{"uri":uri,"version":1},"contentChanges":[{"text":"old=1"}]})).await;
    let result = server
      .request("workspace/symbol", json!({"query":"latest"}))
      .await;
    assert_eq!(result["result"][0]["name"], "latest");
    assert!(
      log_message(&mut server).await["params"]["message"]
        .as_str()
        .unwrap()
        .contains("Outdated document version")
    );
    notify(
      &mut server,
      "textDocument/didClose",
      &json!({"textDocument":{"uri":uri}}),
    )
    .await;
    assert_eq!(
      request(&mut server, "textDocument/documentSymbol", &uri).await["result"]
        [0]["name"],
      "disk"
    );
  }

  #[tokio::test]
  async fn lifecycle_and_invalid_uri_return_protocol_errors() {
    let fixture = Fixture::new();
    let file = fixture.write("test.sh", "x=1");
    let uri = Url::from_file_path(file).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    assert_eq!(
      request(&mut server, "textDocument/documentSymbol", &uri).await["error"]
        ["code"],
      -32002
    );
    initialize(&mut server, true).await;
    assert_eq!(
      request(
        &mut server,
        "textDocument/documentSymbol",
        "untitled:buffer"
      )
      .await["error"]["code"],
      -32602
    );
    assert!(server.request("shutdown", Value::Null).await["result"].is_null());
    assert_eq!(
      request(&mut server, "textDocument/documentSymbol", &uri).await["error"]
        ["code"],
      -32600
    );
  }

  #[tokio::test]
  async fn rejected_document_notifications_keep_text_and_version_until_a_valid_change()
   {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "disk=1");
    let uri = Url::from_file_path(file).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, true).await;
    notify(&mut server, "textDocument/didOpen", &json!({"textDocument":{"uri":uri,"languageId":"shellscript","version":1,"text":"kept=1"}})).await;
    for (method, params, message) in [
      (
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri,"languageId":"shellscript","version":2,"text":"wrong=1"}}),
        "Document is already open",
      ),
      (
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":4}},"text":"wrong"}]}),
        "Full document changes required",
      ),
    ] {
      notify(&mut server, method, &params).await;
      let response =
        request(&mut server, "textDocument/documentSymbol", &uri).await;
      assert_eq!(response["result"].as_array().unwrap().len(), 1);
      assert_eq!(response["result"][0]["name"], "kept");
      assert!(
        log_message(&mut server).await["params"]["message"]
          .as_str()
          .unwrap()
          .contains(message)
      );
    }
    notify(&mut server, "textDocument/didChange", &json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"changed=1"}]})).await;
    let response =
      request(&mut server, "textDocument/documentSymbol", &uri).await;
    assert_eq!(response["result"][0]["name"], "changed");
  }

  #[tokio::test]
  async fn saving_an_open_document_refreshes_source_values_from_disk() {
    let fixture = Fixture::new();
    let source = ". ./selector.sh\n. \"$target\"\n";
    let main = fixture.write("main.sh", source);
    fixture.write("selector.sh", "target=./first.sh\n");
    let first = fixture.write("first.sh", "first=1");
    let second = fixture.write("second.sh", "second=1");
    let uri = Url::from_file_path(main).unwrap().to_string();
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial =
      shellsmith::interface::application::engine::InitialEnvironment::empty();
    let mut server = Client::new(settings);
    initialize(&mut server, true).await;
    notify(&mut server, "textDocument/didOpen", &json!({"textDocument":{"uri":uri,"languageId":"shellscript","version":1,"text":source}})).await;
    let links = request(&mut server, "textDocument/documentLink", &uri).await;
    assert_eq!(
      links["result"][1]["target"],
      Url::from_file_path(first).unwrap().as_str()
    );
    fixture.write("selector.sh", "target=./second.sh\n");
    notify(
      &mut server,
      "textDocument/didSave",
      &json!({"textDocument":{"uri":uri}}),
    )
    .await;
    let links = request(&mut server, "textDocument/documentLink", &uri).await;
    assert_eq!(
      links["result"][1]["target"],
      Url::from_file_path(second).unwrap().as_str()
    );
  }

  #[tokio::test]
  async fn client_rejections_of_registration_and_refresh_are_logged_without_breaking_requests()
   {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "kept=1");
    let uri = Url::from_file_path(file).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    server.request("initialize", json!({"capabilities":{
      "workspace":{"semanticTokens":{"refreshSupport":true},"didChangeWatchedFiles":{"dynamicRegistration":true}},
      "textDocument":{"semanticTokens":{"requests":{"full":true},"formats":["relative"],"tokenTypes":["variable"],"tokenModifiers":[]}}
    }})).await;
    server.notify("initialized", json!({})).await;
    let registration = server.receive_unanswered().await;
    assert_eq!(registration["method"], "client/registerCapability");
    server.send(json!({"jsonrpc":"2.0","id":registration["id"],"error":{"code":-32603,"message":"registration rejected"}})).await;
    assert!(
      log_message(&mut server).await["params"]["message"]
        .as_str()
        .unwrap()
        .contains("registration rejected")
    );
    notify(&mut server, "textDocument/didOpen", &json!({"textDocument":{"uri":uri,"languageId":"shellscript","version":1,"text":"kept=2"}})).await;
    let refresh = server.receive_unanswered().await;
    assert_eq!(refresh["method"], "workspace/semanticTokens/refresh");
    server.send(json!({"jsonrpc":"2.0","id":refresh["id"],"error":{"code":-32603,"message":"refresh rejected"}})).await;
    assert!(
      log_message(&mut server).await["params"]["message"]
        .as_str()
        .unwrap()
        .contains("refresh rejected")
    );
    let symbols =
      request(&mut server, "textDocument/documentSymbol", &uri).await;
    assert_eq!(symbols["result"][0]["name"], "kept");
    let tokens =
      request(&mut server, "textDocument/semanticTokens/full", &uri).await;
    assert_eq!(tokens["result"]["data"], json!([0, 0, 4, 0, 0]));
  }

  #[tokio::test]
  async fn carriage_returns_use_lsp_lines_for_tokens_symbols_and_links() {
    let fixture = Fixture::new();
    fixture.write("lib.sh", "lib_value=1\n");
    let source = "value='a\r😀\r\nc\nd'; next=1; . ./lib.sh\n";
    let path = fixture.write("main.sh", source);
    let uri = Url::from_file_path(path).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, true).await;
    let tokens = decoded(
      &request(&mut server, "textDocument/semanticTokens/full", &uri).await,
    );
    let strings: Vec<_> = tokens
      .iter()
      .filter(|token| token.3 == 3)
      .map(|token| (token.0, token.1, token.2))
      .collect();
    assert_eq!(
      strings,
      [
        (0, 7, 1),
        (1, 0, 2),
        (2, 0, 1),
        (3, 0, 1),
        (3, 9, 1),
        (3, 14, 8)
      ]
    );
    let symbols =
      request(&mut server, "textDocument/documentSymbol", &uri).await;
    let next = symbols["result"]
      .as_array()
      .unwrap()
      .iter()
      .find(|symbol| symbol["name"] == "next")
      .unwrap();
    assert_eq!(
      next["location"]["range"],
      json!({"start":{"line":3,"character":4},"end":{"line":3,"character":8}})
    );
    let links = request(&mut server, "textDocument/documentLink", &uri).await;
    assert_eq!(
      links["result"][0]["range"],
      json!({"start":{"line":3,"character":14},"end":{"line":3,"character":22}})
    );
  }

  #[tokio::test]
  async fn file_uris_with_trailing_slashes_do_not_open_another_document() {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "original=1\n");
    let uri = Url::from_file_path(file).unwrap().to_string();
    let invalid_uri = format!("{uri}/");
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, true).await;
    let response =
      request(&mut server, "textDocument/documentSymbol", &invalid_uri).await;
    assert!(response.get("error").is_some(), "{response}");
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":invalid_uri,"languageId":"shellscript","version":1,"text":"wrong=1\n"}})).await;
    let response =
      request(&mut server, "textDocument/documentSymbol", &uri).await;
    assert_eq!(response["result"][0]["name"], "original");
  }

  #[tokio::test]
  async fn unsaved_new_documents_and_multiline_strings_work_without_disk_files()
  {
    let fixture = Fixture::new();
    let path = fixture.root.join("new.sh");
    let uri = Url::from_file_path(path).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, true).await;
    notify(
      &mut server,
      "textDocument/didOpen",
      &json!({"textDocument":{"uri":uri,"languageId":"shellscript","version":1,"text":"echo '日本😀\nnext'"}}),
    ).await;
    let tokens = decoded(
      &request(&mut server, "textDocument/semanticTokens/full", &uri).await,
    );
    assert!(
      tokens
        .iter()
        .any(|token| token.0 == 1 && token.1 == 0 && token.3 == 3),
      "{tokens:?}"
    );
  }

  #[tokio::test]
  async fn watching_refresh_requests_and_partial_failures_are_exposed_to_client()
   {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "x=1");
    let uri = Url::from_file_path(&file).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    server.request("initialize", json!({"rootUri":Url::from_file_path(&fixture.root).unwrap().to_string(),"capabilities":{
      "workspace":{"semanticTokens":{"refreshSupport":true},"didChangeWatchedFiles":{"dynamicRegistration":true}},
      "textDocument":{"semanticTokens":{"requests":{"full":true},"formats":["relative"],"tokenTypes":["variable"],"tokenModifiers":[]}}}})).await;
    server.notify("initialized", json!({})).await;
    assert_eq!(
      server.receive().await["method"],
      "client/registerCapability"
    );
    notify(&mut server, "textDocument/didOpen", &json!({"textDocument":{"uri":uri,"languageId":"shellscript","version":1,"text":"x=2"}})).await;
    assert_eq!(
      server.receive().await["method"],
      "workspace/semanticTokens/refresh"
    );
    let broken = fixture.root.join("broken.sh");
    std::fs::write(&broken, [0xff]).unwrap();
    server
      .notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":Url::from_file_path(&broken).unwrap(),"type":1}]}))
      .await;
    server
      .request("workspace/symbol", json!({"query":""}))
      .await;
    assert!(
      log_message(&mut server).await["params"]["message"]
        .as_str()
        .unwrap()
        .contains("broken.sh")
    );
  }

  #[tokio::test]
  async fn utf16_columns_after_non_bmp_text_and_repaired_documents_use_current_source()
   {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "echo '日本😀'; f() { :; }\n");
    let uri = Url::from_file_path(&file).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, true).await;
    let tokens = decoded(
      &request(&mut server, "textDocument/semanticTokens/full", &uri).await,
    );
    assert!(tokens.contains(&(0, 13, 1, 7, 2)), "{tokens:?}");
    notify(
      &mut server,
      "textDocument/didOpen",
      &json!({"textDocument":{"uri":uri,"languageId":"shellscript","version":1,"text":"f("}}),
    ).await;
    let tokens = decoded(
      &request(&mut server, "textDocument/semanticTokens/full", &uri).await,
    );
    assert!(tokens.iter().all(|token| token.1 + token.2 <= 2));
    notify(
      &mut server,
      "textDocument/didChange",
      &json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"g() { :; }; g"}]}),
    ).await;
    let tokens = decoded(
      &request(&mut server, "textDocument/semanticTokens/full", &uri).await,
    );
    assert_eq!(tokens.iter().filter(|token| token.3 == 7).count(), 2);
  }

  #[tokio::test]
  async fn cancelling_one_document_request_preserves_another_documents_response()
   {
    let fixture = Fixture::new();
    let first = Url::from_file_path(fixture.root.join("first.sh")).unwrap();
    let second = Url::from_file_path(fixture.root.join("second.sh")).unwrap();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, true).await;
    for (uri, text) in
      [(&first, "x=1\n".repeat(4000)), (&second, "second=2".into())]
    {
      server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"shellscript","version":1,"text":text}})).await;
    }
    server.send(json!({"jsonrpc":"2.0","id":100,"method":"textDocument/semanticTokens/full","params":{"textDocument":{"uri":first}}})).await;
    server.send(json!({"jsonrpc":"2.0","id":101,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":second}}})).await;
    server.notify("$/cancelRequest", json!({"id":100})).await;
    let mut responses = [server.receive().await, server.receive().await];
    responses.sort_by_key(|response| response["id"].as_u64().unwrap());
    assert_eq!(responses[0]["id"], 100);
    assert_eq!(responses[0]["error"]["code"], -32800);
    assert_eq!(responses[1]["id"], 101);
    assert_eq!(responses[1]["result"][0]["name"], "second");
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn workspace_folders_are_removed_by_their_client_paths() {
    async fn names(server: &mut Client) -> Vec<Value> {
      let result = server
        .request("workspace/symbol", json!({"query":""}))
        .await;
      result["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|symbol| symbol["name"].clone())
        .collect()
    }
    async fn folders(server: &mut Client, change: &str, folder: &Value) {
      let event = if change == "added" {
        json!({"added":[folder],"removed":[]})
      } else {
        json!({"added":[],"removed":[folder]})
      };
      server
        .notify(
          "workspace/didChangeWorkspaceFolders",
          json!({ "event": event }),
        )
        .await;
    }
    let fixture = Fixture::new();
    fixture.write("real/a.sh", "real=1");
    fixture.write("other/b.sh", "other=2");
    let link = fixture.root.join("link");
    std::os::unix::fs::symlink(fixture.root.join("real"), &link).unwrap();
    let link_folder =
      json!({"uri":Url::from_file_path(&link).unwrap(),"name":"link"});
    let real_folder = json!({"uri":Url::from_file_path(fixture.root.join("real")).unwrap(),"name":"real"});
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    server
      .request(
        "initialize",
        json!({"capabilities":{},"workspaceFolders":[link_folder,real_folder]}),
      )
      .await;
    server.notify("initialized", json!({})).await;
    assert_eq!(names(&mut server).await, ["real"]);
    folders(&mut server, "removed", &link_folder).await;
    assert_eq!(names(&mut server).await, ["real"]);
    folders(&mut server, "removed", &real_folder).await;
    assert_eq!(names(&mut server).await, Vec::<Value>::new());
    folders(&mut server, "added", &link_folder).await;
    assert_eq!(names(&mut server).await, ["real"]);
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(fixture.root.join("other"), &link).unwrap();
    server.notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":Url::from_file_path(&link).unwrap(),"type":2}]})).await;
    assert_eq!(names(&mut server).await, ["other"]);
    folders(&mut server, "removed", &link_folder).await;
    assert_eq!(names(&mut server).await, Vec::<Value>::new());
  }

  #[tokio::test]
  async fn workspace_folder_changes_and_file_notifications_refresh_collected_symbols()
   {
    let fixture = Fixture::new();
    let first = fixture.write("first/a.sh", "first=1");
    let second = fixture.write("second/b.sh", "second=2");
    let explicit = fixture.write("first/explicit.txt", "explicit=3");
    let first_folder = json!({"uri":Url::from_file_path(first.parent().unwrap()).unwrap(),"name":"first"});
    let second_folder = json!({"uri":Url::from_file_path(second.parent().unwrap()).unwrap(),"name":"second"});
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    server
      .request(
        "initialize",
        json!({"capabilities":{},"workspaceFolders":[first_folder]}),
      )
      .await;
    server.notify("initialized", json!({})).await;
    let result = server
      .request("workspace/symbol", json!({"query":""}))
      .await;
    assert_eq!(result["result"].as_array().unwrap().len(), 1);
    assert_eq!(result["result"][0]["name"], "first");
    let result = request(
      &mut server,
      "textDocument/documentSymbol",
      Url::from_file_path(&explicit).unwrap().as_str(),
    )
    .await;
    assert_eq!(result["result"][0]["name"], "explicit");
    // A request within a folder must not turn its file into a permanent root.
    request(
      &mut server,
      "textDocument/documentSymbol",
      Url::from_file_path(&first).unwrap().as_str(),
    )
    .await;
    let virtual_folder = json!({"uri":"untitled:folder","name":"virtual"});
    server
      .notify(
        "workspace/didChangeWorkspaceFolders",
        json!({"event":{"removed":[first_folder],"added":[virtual_folder,second_folder]}}),
      )
      .await;
    assert!(
      log_message(&mut server).await["params"]["message"]
        .as_str()
        .unwrap()
        .contains("Expected a file URI")
    );
    let result = server
      .request("workspace/symbol", json!({"query":""}))
      .await;
    assert_eq!(result["result"].as_array().unwrap().len(), 1);
    assert_eq!(result["result"][0]["name"], "second");
    std::fs::write(&second, "updated=3").unwrap();
    server.notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":"untitled:buffer","type":2},{"uri":Url::from_file_path(&second).unwrap(),"type":2}]})).await;
    assert!(
      log_message(&mut server).await["params"]["message"]
        .as_str()
        .unwrap()
        .contains("Expected a file URI")
    );
    let result = server
      .request("workspace/symbol", json!({"query":""}))
      .await;
    assert_eq!(result["result"][0]["name"], "updated");
    let ignore = fixture.root.join("second/.gitignore");
    for (content, change, included) in [
      (Some("b.sh\n"), 1, false),
      (Some("!b.sh\n"), 2, true),
      (Some("b.sh\n"), 2, false),
      (None, 3, true),
    ] {
      if let Some(content) = content {
        std::fs::write(&ignore, content).unwrap();
      } else {
        std::fs::remove_file(&ignore).unwrap();
      }
      server.notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":Url::from_file_path(&ignore).unwrap(),"type":change}]})).await;
      let result = server
        .request("workspace/symbol", json!({"query":""}))
        .await;
      let names = result["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|symbol| symbol["name"].as_str().unwrap())
        .collect::<Vec<_>>();
      let expected: &[&str] = if included { &["updated"] } else { &[] };
      assert_eq!(names, expected);
    }
  }

  #[tokio::test]
  async fn cancellation_reaches_requests_when_concurrent_workers_are_full() {
    let fixture = Fixture::new();
    let uri = Url::from_file_path(fixture.root.join("main.sh")).unwrap();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, true).await;
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"shellscript","version":1,"text":"x=1\n".repeat(4000)}})).await;
    let count = std::thread::available_parallelism().unwrap().get() + 1;
    for id in 100..100 + count {
      server.send(json!({"jsonrpc":"2.0","id":id.to_string(),"method":"textDocument/semanticTokens/full","params":{"textDocument":{"uri":uri}}})).await;
    }
    for id in 100..100 + count {
      server
        .notify("$/cancelRequest", json!({"id":id.to_string()}))
        .await;
    }
    for _ in 0..count {
      let response = server.receive().await;
      assert_eq!(response["error"]["code"], -32800, "{response}");
    }
  }

  #[tokio::test]
  async fn document_links_share_source_resolution_and_use_complete_utf16_operands()
   {
    let fixture = Fixture::new();
    let library = fixture.write("日本 😀.sh", "linked() { :; }\n");
    let source = "file='./日本 😀.sh'\n. \"$file\"\ncommand . './日本 😀.sh'\ntrap 'printf 😀; . \"./日本 😀.sh\"' EXIT\nlinked\n";
    let file = fixture.write("main.sh", source);
    let uri = Url::from_file_path(file).unwrap().to_string();
    let target = Url::from_file_path(library).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    let initialized = initialize(&mut server, true).await;
    assert_eq!(
      initialized["result"]["capabilities"]["documentLinkProvider"]["resolveProvider"],
      false
    );
    let response =
      request(&mut server, "textDocument/documentLink", &uri).await;
    assert_eq!(
      response["result"],
      json!([
        {"range":{"start":{"line":1,"character":2},"end":{"line":1,"character":9}},"target":target},
        {"range":{"start":{"line":2,"character":10},"end":{"line":2,"character":22}},"target":target},
        {"range":{"start":{"line":3,"character":19},"end":{"line":3,"character":31}},"target":target}
      ]),
      "{response}"
    );
    let tokens = decoded(
      &request(&mut server, "textDocument/semanticTokens/full", &uri).await,
    );
    assert!(
      tokens
        .iter()
        .any(|token| token.0 == 4 && token.1 == 0 && token.3 == 7),
      "{tokens:?}"
    );
  }

  #[tokio::test]
  async fn document_links_omit_conflicts_unknown_values_and_trap_bodies() {
    let fixture = Fixture::new();
    fixture.write("a.sh", ":\n");
    fixture.write("b.sh", ":\n");
    for source in [
      "f() { \"$1\" ./a.sh; }; f .; f printf",
      "f() { command \"$1\" ./a.sh; }; f .; f printf",
      "register() { trap \"$prefix\"'. ./a.sh' TERM; }; prefix=''; register; prefix='#'; register",
      "f() { . \"$1\"; }; f ./a.sh; f ./b.sh",
      "if condition; then p=./a.sh; else p=./b.sh; fi; . \"$p\"",
      "p=$(external); . \"$p\"",
      "trap \"$BODY\" EXIT",
      ". ./missing.sh",
      "trap 'echo 😀; . \"$path\"' TERM; path=./a.sh; :; path=./b.sh; :",
    ] {
      let path = fixture.write("main.sh", source);
      let uri = Url::from_file_path(path).unwrap().to_string();
      let mut settings = Settings::new(fixture.root.clone());
      settings
        .initial
        .environment
        .insert(b"BODY".to_vec(), b". ./a.sh".to_vec());
      let mut server = Client::new(settings);
      initialize(&mut server, true).await;
      let response =
        request(&mut server, "textDocument/documentLink", &uri).await;
      assert_eq!(response["result"], json!([]), "{source}: {response}");
    }
  }

  #[tokio::test]
  async fn document_links_follow_edits_target_removal_and_restoration() {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", ":\n");
    let file = fixture.write("main.sh", ". ./lib.sh\n");
    let uri = Url::from_file_path(file).unwrap().to_string();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, true).await;
    assert_eq!(
      request(&mut server, "textDocument/documentLink", &uri).await["result"]
        .as_array()
        .unwrap()
        .len(),
      1
    );
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"sh","version":1,"text":". ./missing.sh\n"}})).await;
    assert_eq!(
      request(&mut server, "textDocument/documentLink", &uri).await["result"],
      json!([])
    );
    server.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":". ./lib.sh\n"}]})).await;
    assert_eq!(
      request(&mut server, "textDocument/documentLink", &uri).await["result"]
        .as_array()
        .unwrap()
        .len(),
      1
    );
    std::fs::remove_file(&library).unwrap();
    assert_eq!(
      request(&mut server, "textDocument/documentLink", &uri).await["result"],
      json!([])
    );
    fixture.write("lib.sh", ":\n");
    server
      .notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}))
      .await;
    assert_eq!(
      request(&mut server, "textDocument/documentLink", &uri).await["result"]
        .as_array()
        .unwrap()
        .len(),
      1
    );
  }

  #[tokio::test]
  async fn trap_body_links_use_shared_sources_and_utf16_original_ranges() {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "body_only=1\n");
    let source = format!(
      "trap ': \"😀\"; . \"{}\"; . ./unknown-cwd.sh' EXIT",
      library.display()
    );
    let file = fixture.write("main.sh", &source);
    let uri = Url::from_file_path(&file).unwrap().to_string();
    let target = Url::from_file_path(&library).unwrap().to_string();
    let operand = format!("\"{}\"", library.display());
    let offset = source.find(&operand).unwrap();
    let start = source[..offset].encode_utf16().count();
    let end = start + operand.encode_utf16().count();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, true).await;
    let links = request(&mut server, "textDocument/documentLink", &uri).await;
    assert_eq!(
      links["result"],
      json!([{"range":{"start":{"line":0,"character":start},"end":{"line":0,"character":end}},"target":target}])
    );
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"sh","version":1,"text":"trap - EXIT"}})).await;
    assert_eq!(
      request(&mut server, "textDocument/documentLink", &uri).await["result"],
      json!([])
    );
  }

  #[tokio::test]
  async fn document_links_deduplicate_contexts_and_preserve_sources_after_excluded_effects()
   {
    let fixture = Fixture::new();
    let target = Url::from_file_path(fixture.write("a.sh", ":\n"))
      .unwrap()
      .to_string();
    for (source, operand) in [
      ("f() { . ./a.sh; }; f; f", "./a.sh"),
      ("alias a=if; eval \"$shellenv\"; . './a.sh'", "'./a.sh'"),
      (
        "trap 'cd /unknown; . ./hidden.sh' TERM EXIT; . ./a.sh",
        "./a.sh",
      ),
    ] {
      let uri = Url::from_file_path(fixture.write("main.sh", source))
        .unwrap()
        .to_string();
      let mut server = Client::new(Settings::new(fixture.root.clone()));
      initialize(&mut server, true).await;
      let response =
        request(&mut server, "textDocument/documentLink", &uri).await;
      let start = source.find(operand).unwrap();
      assert_eq!(
        response["result"],
        json!([{"range":{"start":{"line":0,"character":start},"end":{"line":0,"character":start + operand.len()}},"target":target}]),
        "{source}: {response}"
      );
    }
  }

  #[tokio::test]
  async fn document_links_use_unsaved_dependency_values_and_support_independent_cancellation()
   {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "next=./a.sh\n");
    fixture.write("a.sh", ":\n");
    let new_target = Url::from_file_path(fixture.write("b.sh", ":\n")).unwrap();
    let uri = Url::from_file_path(
      fixture.write("main.sh", ". ./lib.sh\n. \"$next\"\n"),
    )
    .unwrap();
    let mut server = Client::new(Settings::new(fixture.root.clone()));
    initialize(&mut server, true).await;
    assert_eq!(request(&mut server, "textDocument/documentLink", uri.as_str()).await["result"].as_array().unwrap().len(), 2);
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":Url::from_file_path(library).unwrap(),"languageId":"sh","version":1,"text":"next=./b.sh\n"}})).await;
    let response =
      request(&mut server, "textDocument/documentLink", uri.as_str()).await;
    assert_eq!(
      response["result"][1]["target"],
      new_target.as_str(),
      "{response}"
    );
    let busy = Url::from_file_path(fixture.root.join("busy.sh")).unwrap();
    server.notify("textDocument/didOpen", json!({"textDocument":{"uri":busy,"languageId":"sh","version":1,"text":"x=1\n".repeat(4000)}})).await;
    server.send(json!({"jsonrpc":"2.0","id":500,"method":"textDocument/documentLink","params":{"textDocument":{"uri":busy}}})).await;
    server.send(json!({"jsonrpc":"2.0","id":501,"method":"textDocument/semanticTokens/full","params":{"textDocument":{"uri":busy}}})).await;
    server.notify("$/cancelRequest", json!({"id":500})).await;
    let mut responses = vec![server.receive().await, server.receive().await];
    responses.sort_by_key(|response| response["id"].as_u64().unwrap());
    assert_eq!(responses[0]["id"], 500);
    assert_eq!(responses[0]["error"]["code"], -32800, "{responses:?}");
    assert_eq!(responses[1]["id"], 501);
    assert_ne!(decoded(&responses[1]), []);
    let response =
      request(&mut server, "textDocument/documentLink", uri.as_str()).await;
    assert_eq!(
      response["result"][1]["target"],
      new_target.as_str(),
      "{response}"
    );
  }
}

mod cli {
  use std::process::Command;

  #[test]
  fn cli_help_and_version_succeed_without_starting_lsp() {
    for (argument, expected) in [
      (
        "--help",
        "shellsmith lsp\nshellsmith --help | --version\n".to_owned(),
      ),
      (
        "--version",
        format!("shellsmith {}\n", env!("CARGO_PKG_VERSION")),
      ),
    ] {
      let output = Command::new(env!("CARGO_BIN_EXE_shellsmith"))
        .arg(argument)
        .output()
        .unwrap();
      assert_eq!(output.status.code(), Some(0));
      assert_eq!(output.stdout, expected.as_bytes());
      assert_eq!(output.stderr, Vec::<u8>::new());
    }
  }

  #[test]
  fn cli_rejects_removed_commands_and_invalid_arguments() {
    for args in [
      vec![],
      vec!["tokens", "main.sh"],
      vec!["symbols", "main.sh"],
      vec!["-x"],
      vec!["lsp", "main.sh"],
      vec!["--help", "extra"],
      vec!["--version", "extra"],
    ] {
      let output = Command::new(env!("CARGO_BIN_EXE_shellsmith"))
        .args(&args)
        .output()
        .unwrap();
      assert_eq!(output.status.code(), Some(1), "{args:?}");
      assert_eq!(output.stdout, Vec::<u8>::new(), "{args:?}");
      assert_eq!(
        output.stderr,
        b"shellsmith: Usage:\nshellsmith lsp\nshellsmith --help | --version\n\n",
        "{args:?}"
      );
    }
  }
}

mod transport {
  use serde_json::{Value, json};
  use std::io::{BufRead, BufReader, Read, Write};
  use std::process::{Command, Stdio};

  fn send(input: &mut impl Write, message: &Value) {
    let bytes = serde_json::to_vec(message).unwrap();
    write!(input, "Content-Length: {}\r\n\r\n", bytes.len()).unwrap();
    input.write_all(&bytes).unwrap();
    input.flush().unwrap();
  }
  fn receive(output: &mut impl BufRead) -> Value {
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    let length: usize = line
      .trim()
      .strip_prefix("Content-Length: ")
      .unwrap()
      .parse()
      .unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    assert_eq!(line, "\r\n");
    let mut bytes = vec![0; length];
    output.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
  }

  #[test]
  fn executable_stdio_uses_framing_and_exits_after_shutdown_with_stdin_open() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_shellsmith"))
      .arg("lsp")
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    send(
      &mut input,
      &json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{"capabilities":{}}}),
    );
    assert_eq!(receive(&mut output)["id"], "init");
    send(
      &mut input,
      &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    );
    send(
      &mut input,
      &json!({"jsonrpc":"2.0","id":"stop","method":"shutdown"}),
    );
    assert_eq!(receive(&mut output)["id"], "stop");
    send(&mut input, &json!({"jsonrpc":"2.0","method":"exit"}));
    assert!(child.wait().unwrap().success());
    drop(input);
    let mut errors = String::new();
    child
      .stderr
      .take()
      .unwrap()
      .read_to_string(&mut errors)
      .unwrap();
    assert_eq!(errors, "");
  }

  #[test]
  fn executable_rejects_invalid_json_truncated_frames_and_exit_without_shutdown()
   {
    for bytes in [
      b"Content-Length: 1\r\n\r\n{".as_slice(),
      b"Content-Length: 5\r\n\r\n{}",
      b"Content-Length: invalid\r\n\r\n",
      b"Content-Length: 33\r\n\r\n{\"jsonrpc\":\"2.0\",\"method\":\"exit\"}",
    ] {
      let mut child = Command::new(env!("CARGO_BIN_EXE_shellsmith"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
      child.stdin.take().unwrap().write_all(bytes).unwrap();
      let output = child.wait_with_output().unwrap();
      assert!(!output.status.success());
    }
  }

  #[test]
  fn cancelled_or_superseded_requests_do_not_return_document_results_or_edits()
  {
    for (changed, method) in [
      (false, "textDocument/semanticTokens/full"),
      (true, "textDocument/semanticTokens/full"),
      (false, "textDocument/definition"),
      (true, "textDocument/definition"),
      (false, "textDocument/prepareRename"),
      (true, "textDocument/prepareRename"),
      (false, "textDocument/rename"),
      (true, "textDocument/rename"),
    ] {
      let mut child = Command::new(env!("CARGO_BIN_EXE_shellsmith"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
      let mut input = child.stdin.take().unwrap();
      let mut output = BufReader::new(child.stdout.take().unwrap());
      send(
        &mut input,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{"textDocument":{"semanticTokens":{"requests":{"full":true},"formats":["relative"],"tokenTypes":["variable"],"tokenModifiers":[]}}}}}),
      );
      assert_eq!(receive(&mut output)["id"], 1);
      send(
        &mut input,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
      );
      let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/transport-unsaved.sh");
      let uri = url::Url::from_file_path(path).unwrap().to_string();
      let source = "x=1\n".repeat(4000);
      let mut batch = Vec::new();
      send(
        &mut batch,
        &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":"shellscript","version":1,"text":source}}}),
      );
      send(
        &mut batch,
        &json!({"jsonrpc":"2.0","id":2,"method":method,"params":{"textDocument":{"uri":uri},"position":{"line":0,"character":0},"newName":"value"}}),
      );
      if changed {
        send(
          &mut batch,
          &json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"x=2"}]}}),
        );
      } else {
        send(
          &mut batch,
          &json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":2}}),
        );
      }
      input.write_all(&batch).unwrap();
      input.flush().unwrap();
      let response = receive(&mut output);
      assert_eq!(response["id"], 2);
      assert_eq!(
        response["error"]["code"],
        if changed { -32801 } else { -32800 }
      );
      send(
        &mut input,
        &json!({"jsonrpc":"2.0","id":3,"method":"shutdown"}),
      );
      assert_eq!(receive(&mut output)["id"], 3);
      send(&mut input, &json!({"jsonrpc":"2.0","method":"exit"}));
      drop(input);
      assert!(child.wait().unwrap().success());
    }
  }
}
