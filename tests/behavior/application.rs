mod documents {
  use crate::fixtures;
  use fixtures::Fixture;
  use shellsmith::interface::application::engine::{
    DependencyState, Modifier, SemanticKind,
  };
  use shellsmith::interface::application::{Application, Settings};
  use std::sync::atomic::AtomicBool;

  #[test]
  fn symbol_hierarchy_preserves_redefinitions_and_skips_control_flow_containers()
   {
    let fixture = Fixture::new();
    let source = "top=0\nouter() { x=1; if :; then x=2; fi; (inner() { y=3; }); for item in a; do : \"${fallback:=4}\"; done; : \"$((n += 1))\"; }\nouter() { z=5; }\n";
    let file = fixture.write("main.sh", source);
    let cancel = AtomicBool::new(false);
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.open(&file, source, &cancel).unwrap();
    let id = app.document_id(&file).unwrap();
    let entries: Vec<_> = app.document_symbols(id).unwrap().collect();
    let actual: Vec<_> = entries
      .iter()
      .map(|entry| {
        let details = &entry.details;
        let range = details.range.as_ref().expect("known definition range");
        (
          details.symbol.name.as_slice(),
          entry.parent,
          &source[range.bytes.clone()],
        )
      })
      .collect();
    assert_eq!(
      actual,
      [
        (b"top".as_slice(), None, "top=0"),
        (
          b"outer",
          None,
          "outer() { x=1; if :; then x=2; fi; (inner() { y=3; }); for item in a; do : \"${fallback:=4}\"; done; : \"$((n += 1))\"; }"
        ),
        (b"x", Some(1), "x=1"),
        (b"x", Some(1), "x=2"),
        (b"inner", Some(1), "inner() { y=3; }"),
        (b"y", Some(4), "y=3"),
        (b"item", Some(1), "item"),
        (b"fallback", Some(1), "${fallback:=4}"),
        (b"n", Some(1), "n += 1"),
        (b"outer", None, "outer() { z=5; }"),
        (b"z", Some(9), "z=5"),
      ]
    );
    assert!(app.workspace().analysis().is_none());
    let found: Vec<_> = app
      .workspace_symbols("y")
      .map(|entry| {
        (
          entry.symbol.name.as_slice(),
          entry.container.map(|parent| parent.name.as_slice()),
        )
      })
      .collect();
    assert_eq!(found, [(b"y".as_slice(), Some(b"inner".as_slice()))]);
    assert_eq!(
      app
        .workspace_symbols("")
        .map(|entry| std::str::from_utf8(&entry.symbol.name).unwrap())
        .collect::<Vec<_>>(),
      [
        "fallback", "inner", "item", "n", "outer", "outer", "top", "x", "x",
        "y", "z"
      ]
    );
  }

  #[test]
  fn nested_trap_symbols_share_containers_and_refresh_after_edits() {
    let fixture = Fixture::new();
    let source = "outer() { trap 'middle() { trap \"leaf() { value=1; }\" EXIT; }; sibling=2' EXIT; }";
    let file = fixture.write("main.sh", source);
    let cancel = AtomicBool::new(false);
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.open(&file, source, &cancel).unwrap();
    let id = app.document_id(&file).unwrap();
    let entries: Vec<_> = app
      .document_symbols(id)
      .unwrap()
      .map(|entry| {
        (
          entry.details.symbol.name.clone(),
          entry.parent,
          source[entry.details.range.unwrap().bytes].to_owned(),
        )
      })
      .collect();
    assert_eq!(
      entries,
      [
        (b"outer".to_vec(), None, source.to_owned()),
        (
          b"middle".to_vec(),
          Some(0),
          "middle() { trap \"leaf() { value=1; }\" EXIT; }".into()
        ),
        (b"leaf".to_vec(), Some(1), "leaf() { value=1; }".into()),
        (b"value".to_vec(), Some(2), "value=1".into()),
        (b"sibling".to_vec(), Some(0), "sibling=2".into()),
      ]
    );
    assert!(app.workspace().analysis().is_none());
    let found: Vec<_> = app
      .workspace_symbols("value")
      .map(|entry| {
        (
          entry.symbol.name.as_slice(),
          entry.container.map(|parent| parent.name.as_slice()),
        )
      })
      .collect();
    assert_eq!(found, [(b"value".as_slice(), Some(b"leaf".as_slice()))]);
    app.change(&file, "trap 'value=2' EXIT", &cancel).unwrap();
    assert_eq!(app.workspace_symbols("leaf").count(), 0);
    let updated: Vec<_> = app
      .document_symbols(id)
      .unwrap()
      .map(|entry| {
        (
          entry.details.symbol.name.as_slice(),
          entry.parent,
          entry.details.container.map(|parent| parent.name.as_slice()),
        )
      })
      .collect();
    assert_eq!(updated, [(b"value".as_slice(), None, None)]);
  }

  #[test]
  fn trap_body_symbols_and_definitions_share_the_index_before_any_semantic_request()
   {
    let fixture = Fixture::new();
    let source = "outer() { :; }; trap '(inside() { :; }; inside); outer' EXIT";
    let file = fixture.write("main.sh", source);
    let cancel = AtomicBool::new(false);
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.open(&file, source, &cancel).unwrap();
    let id = app.document_id(&file).unwrap();
    let symbols = |app: &Application| {
      app
        .document_symbols(id)
        .unwrap()
        .map(|entry| {
          (
            entry.details.symbol.name.clone(),
            entry.details.location.bytes,
          )
        })
        .collect::<Vec<_>>()
    };
    let inside = source.find("inside()").unwrap();
    let expected = vec![
      (b"outer".to_vec(), 0..5),
      (b"inside".to_vec(), inside..inside + 6),
    ];
    assert_eq!(symbols(&app), expected);
    assert!(app.workspace().analysis().is_none());
    assert_eq!(
      app
        .workspace_symbols("inside")
        .map(|entry| (entry.location.bytes.start, entry.location.bytes.end))
        .collect::<Vec<_>>(),
      [(inside, inside + 6)]
    );
    let call = source.find("inside);").unwrap();
    assert_eq!(
      app
        .definitions(id, call, &cancel)
        .unwrap()
        .into_iter()
        .map(|location| (location.bytes.start, location.bytes.end))
        .collect::<Vec<_>>(),
      [(inside, inside + 6)]
    );
    assert_eq!(
      app
        .definitions(id, source.rfind("outer").unwrap(), &cancel)
        .unwrap(),
      [shellsmith::interface::application::engine::Location {
        snapshot: app.workspace().document(id).unwrap().snapshot(),
        bytes: 0..5,
      }]
    );
    let spans = app.classify(id, &cancel).unwrap();
    assert!(
      spans
        .iter()
        .any(|span| span.location.bytes == (call..call + 6)
          && span.kind == SemanticKind::Function)
    );
    assert_eq!(symbols(&app), expected);
    let updated = "trap 'replacement() { :; }' EXIT";
    app.change(&file, updated, &cancel).unwrap();
    assert_eq!(app.workspace_symbols("inside").count(), 0);
    assert_eq!(
      app
        .document_symbols(id)
        .unwrap()
        .map(|entry| entry.details.symbol.name.as_slice())
        .collect::<Vec<_>>(),
      [b"replacement".as_slice()]
    );
  }

  #[test]
  fn syntax_symbols_do_not_depend_on_value_analysis_or_speculative_utilities() {
    let fixture = Fixture::new();
    let cancel = AtomicBool::new(false);
    for source in [
      "prefix=''; trap \"$prefix\"'derived() { :; }' EXIT",
      "idle() { command trap 'derived() { :; }' EXIT; }",
      "command() { :; }; command trap 'derived() { :; }' EXIT",
    ] {
      let file = fixture.write("main.sh", source);
      let mut app =
        Application::new(Settings::new(fixture.root.clone())).unwrap();
      app.open(&file, source, &cancel).unwrap();
      let id = app.document_id(&file).unwrap();
      assert_eq!(app.workspace_symbols("derived").count(), 0, "{source}");
      let before = app
        .document_symbols(id)
        .unwrap()
        .map(|entry| {
          (entry.details.symbol.name.clone(), entry.details.location)
        })
        .collect::<Vec<_>>();
      app.classify(id, &cancel).unwrap();
      assert_eq!(app.workspace_symbols("derived").count(), 0, "{source}");
      assert_eq!(
        app
          .document_symbols(id)
          .unwrap()
          .map(|entry| (
            entry.details.symbol.name.clone(),
            entry.details.location
          ))
          .collect::<Vec<_>>(),
        before,
        "{source}"
      );
    }
  }

  #[test]
  fn body_recovery_preserves_prior_bindings_and_requires_a_later_definition_to_restore_them()
   {
    let fixture = Fixture::new();
    let source = "trap 'f() { :; }; f; )\nf; f() { :; }; f' EXIT";
    let file = fixture.write("main.sh", source);
    let cancel = AtomicBool::new(false);
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.open(&file, source, &cancel).unwrap();
    let id = app.document_id(&file).unwrap();
    let first = source.find("f()").unwrap();
    let last = source.rfind("f()").unwrap();
    for (offset, expected) in [
      (source.find("f; )").unwrap(), vec![(first, first + 1)]),
      (source.find("\nf;").unwrap() + 1, vec![]),
      (source.rfind("f'").unwrap(), vec![(last, last + 1)]),
    ] {
      assert_eq!(
        app
          .definitions(id, offset, &cancel)
          .unwrap()
          .into_iter()
          .map(|location| (location.bytes.start, location.bytes.end))
          .collect::<Vec<_>>(),
        expected
      );
      assert_eq!(
        app.classify(id, &cancel).unwrap().iter().any(|span| span
          .location
          .bytes
          == (offset..offset + 1)
          && span.kind == SemanticKind::Function),
        !expected.is_empty()
      );
    }
  }

  #[test]
  fn shared_resolution_keeps_builtin_names_separate_from_variable_names() {
    let fixture = Fixture::new();
    let source = "trap=1; trap ': \"$trap\"' EXIT; : \"$trap\"";
    let file = fixture.write("main.sh", source);
    let cancel = AtomicBool::new(false);
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.open(&file, source, &cancel).unwrap();
    let id = app.document_id(&file).unwrap();
    for (offset, expected) in [
      (source.find("$trap").unwrap() + 1, vec![(0, 4)]),
      (source.rfind("$trap").unwrap() + 1, vec![(0, 4)]),
    ] {
      assert_eq!(
        app
          .definitions(id, offset, &cancel)
          .unwrap()
          .into_iter()
          .map(|location| (location.bytes.start, location.bytes.end))
          .collect::<Vec<_>>(),
        expected
      );
      assert!(
        app.classify(id, &cancel).unwrap().iter().any(|span| span
          .location
          .bytes
          == (offset..offset + 4)
          && span.kind == SemanticKind::Variable)
      );
    }
  }

  #[test]
  fn deferred_source_links_share_collection_and_use_only_invariant_cwd() {
    let fixture = Fixture::new();
    let library = fixture.write("library.sh", "changed=1; supplied() { :; }\n");
    fixture.write("relative.sh", "must_not_load=1\n");
    let source = format!(
      "trap '. \"{}\"; . ./relative.sh' EXIT; idle() {{ . \"{}\"; }}; : \"$changed\"; supplied; cd \"$unknown\"\n",
      library.display(),
      library.display()
    );
    let file = fixture.write("main.sh", &source);
    let cancel = AtomicBool::new(false);
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial =
      shellsmith::interface::application::engine::InitialEnvironment::empty();
    let mut app = Application::new(settings).unwrap();
    app.open(&file, &source, &cancel).unwrap();
    let id = app.document_id(&file).unwrap();
    let links = app.document_links(id, &cancel).unwrap();
    assert_eq!(links.len(), 2);
    for (location, path) in &links {
      assert_eq!(path, &library);
      assert_eq!(
        &source[location.bytes.clone()],
        format!("\"{}\"", library.display())
      );
    }
    let analysis = app.workspace().analysis().unwrap();
    assert_eq!(
      analysis
        .contexts()
        .iter()
        .filter(|context| !crate::support::deferred(analysis, context.id))
        .count(),
      1
    );
    assert!(app.document_id(&fixture.root.join("relative.sh")).is_none());
    assert_eq!(app.workspace_symbols("supplied").count(), 1);
    for name in ["changed", "supplied"] {
      assert_eq!(
        app
          .definitions(id, source.rfind(name).unwrap(), &cancel)
          .unwrap(),
        []
      );
    }
    assert_eq!(app.document_links(id, &cancel).unwrap(), links);
  }

  #[test]
  fn opening_a_missing_source_invalidates_failed_path_lookup() {
    let fixture = Fixture::new();
    let main = fixture.write("main.sh", ". ./lib.sh\nf\n");
    let lib = fixture.root.join("lib.sh");
    let cancel = AtomicBool::new(false);
    let settings = Settings::new(fixture.root.clone());
    let mut app = Application::new(settings).unwrap();
    app.open(&main, ". ./lib.sh\nf\n", &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    assert_eq!(app.document_links(id, &cancel).unwrap(), []);
    app.open(&lib, "f() { :; }\n", &cancel).unwrap();
    assert_eq!(app.document_links(id, &cancel).unwrap().len(), 1);
    assert!(
      app
        .classify(id, &cancel)
        .unwrap()
        .iter()
        .any(|span| span.kind == SemanticKind::Function)
    );
    assert!(app.close(&lib, &cancel).is_err());
    assert_eq!(app.document_links(id, &cancel).unwrap(), []);
    assert!(app.document_id(&lib).is_none());
  }

  #[test]
  fn invalid_source_bytes_preserve_other_links_and_recover_after_repair() {
    let fixture = Fixture::new();
    let good = fixture.write("good.sh", "good=known\n");
    let main = fixture.write(
      "main.sh",
      &format!(
        ". ./good.sh\n. ./broken.sh\n. '{}'\nkept=after\n",
        good.display()
      ),
    );
    let broken = fixture.write("broken.sh", "");
    std::fs::write(&broken, [0xff]).unwrap();
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial =
      shellsmith::interface::application::engine::InitialEnvironment::empty();
    let mut app = Application::new(settings).unwrap();
    app.add_input(&main);
    let cancelled = AtomicBool::new(false);
    app.refresh(&cancelled).unwrap();
    let id = app.document_id(&main).unwrap();
    assert_eq!(
      app
        .document_links(id, &cancelled)
        .unwrap()
        .into_iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>(),
      [good.clone(), good.clone()]
    );
    assert_eq!(app.failures().len(), 1);
    assert_eq!(app.failures()[0].path, broken);
    assert!(app.failures()[0].message.contains("Invalid UTF-8"));
    assert_eq!(app.workspace().symbols_named(b"good").count(), 1);
    assert_eq!(app.workspace().symbols_named(b"kept").count(), 1);
    fixture.write("broken.sh", "repaired=known\n");
    app.refresh(&cancelled).unwrap();
    assert_eq!(app.failures(), []);
    assert_eq!(
      app
        .document_links(id, &cancelled)
        .unwrap()
        .into_iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>(),
      [good.clone(), broken, good]
    );
    assert_eq!(app.workspace().symbols_named(b"repaired").count(), 1);
  }

  #[test]
  fn closing_a_deleted_source_does_not_restore_cached_text() {
    let fixture = Fixture::new();
    let main = fixture.write("main.sh", ". ./lib.sh\n");
    let lib = fixture.write("lib.sh", "x=1\n");
    let cancel = AtomicBool::new(false);
    let settings = Settings::new(fixture.root.clone());
    let mut app = Application::new(settings).unwrap();
    app.open(&main, ". ./lib.sh\n", &cancel).unwrap();
    app.open(&lib, "x=2\n", &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    assert_eq!(app.document_links(id, &cancel).unwrap().len(), 1);
    std::fs::remove_file(&lib).unwrap();
    assert!(app.close(&lib, &cancel).is_err());
    assert_eq!(app.document_links(id, &cancel).unwrap(), []);
    assert!(app.document_id(&lib).is_none());
  }

  #[cfg(unix)]
  #[test]
  fn unsaved_sources_under_directory_links_share_their_real_path() {
    let fixture = Fixture::new();
    let main = fixture.write("real/main.sh", ". ./lib.sh\n");
    let real = fixture.root.join("real/lib.sh");
    let alias = fixture.root.join("alias");
    std::os::unix::fs::symlink(fixture.root.join("real"), &alias).unwrap();
    let cancel = AtomicBool::new(false);
    let settings = Settings::new(fixture.root.clone());
    let mut app = Application::new(settings).unwrap();
    app.open(&main, ". ./lib.sh\n", &cancel).unwrap();
    app.open(&alias.join("lib.sh"), "x=1\n", &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    let links = app.document_links(id, &cancel).unwrap();
    assert_eq!(
      links.iter().map(|(_, path)| path).collect::<Vec<_>>(),
      vec![&real]
    );
    assert_eq!(
      app.document_id(&real),
      app.document_id(&alias.join("lib.sh"))
    );
  }

  #[test]
  fn source_paths_cannot_traverse_files_or_missing_directories() {
    let fixture = Fixture::new();
    fixture.write("regular", "not a directory");
    let library = fixture.write("lib.sh", "wrong=1\n");
    let main = fixture.write("main.sh", ". ./regular/../lib.sh\n");
    let cancel = AtomicBool::new(false);
    let settings = Settings::new(fixture.root.clone());
    let mut app = Application::new(settings).unwrap();
    app.open(&main, ". ./regular/../lib.sh\n", &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    assert_eq!(app.document_links(id, &cancel).unwrap(), []);
    assert_eq!(app.workspace().symbols_named(b"wrong").count(), 0);
    app.open(&library, "wrong=1\n", &cancel).unwrap();
    for source in [". ./regular/../lib.sh\n", ". ./missing/../lib.sh\n"] {
      app.open(&main, source, &cancel).unwrap();
      assert_eq!(app.document_links(id, &cancel).unwrap(), [], "{source}");
    }
  }

  #[test]
  fn failed_parameter_assignments_do_not_create_source_links() {
    let fixture = Fixture::new();
    fixture.write("lib.sh", "wrong=1\n");
    let cancel = AtomicBool::new(false);
    for source in [
      "readonly target=; . \"${target:=./lib.sh}\"\n",
      "readonly target; . \"${target=./lib.sh}\"\n",
      "set --; . \"${1:=./lib.sh}\"\n",
    ] {
      let main = fixture.write("main.sh", source);
      let settings = Settings::new(fixture.root.clone());
      let mut app = Application::new(settings).unwrap();
      app.open(&main, source, &cancel).unwrap();
      let id = app.document_id(&main).unwrap();
      assert_eq!(app.document_links(id, &cancel).unwrap(), [], "{source}");
      assert_eq!(
        app.workspace().symbols_named(b"wrong").count(),
        0,
        "{source}"
      );
    }
  }

  #[test]
  fn symbol_features_separate_document_scope_from_workspace_name_search() {
    let fixture = Fixture::new();
    let main =
      fixture.write("main.sh", "local_name=1\nmain_function() { :; }\n");
    let library = fixture.write("lib.sh", "library_name=2\n");
    let cancel = AtomicBool::new(false);
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&fixture.root);
    app.refresh(&cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    let symbols = app
      .document_symbols(id)
      .unwrap()
      .map(|entry| {
        assert_eq!(entry.details.location.snapshot.document, id);
        String::from_utf8(entry.details.symbol.name.clone()).unwrap()
      })
      .collect::<Vec<_>>();
    assert_eq!(symbols, ["local_name", "main_function"]);
    let matches = app
      .workspace_symbols("name")
      .map(|entry| String::from_utf8(entry.symbol.name.clone()).unwrap())
      .collect::<Vec<_>>();
    assert_eq!(matches, ["library_name", "local_name"]);
    assert_eq!(app.workspace_symbols("missing").count(), 0);
    let removed = app.document_id(&library).unwrap();
    std::fs::remove_file(library).unwrap();
    app.refresh(&cancel).unwrap();
    assert!(
      matches!(app.document_symbols(removed), Err(shellsmith::interface::application::Error::Engine(shellsmith::interface::application::engine::Error::UnknownDocument(missing))) if missing == removed)
    );
    assert_eq!(app.workspace_symbols("library").count(), 0);
  }

  #[test]
  fn recursive_collection_selects_sh_files_and_hidden_entries() {
    let fixture = Fixture::new();
    let cases = [
      ("a.sh", "a=1", true),
      (".hidden/b.sh", "b=2", true),
      ("executable", "#!/bin/sh\nc=3", true),
      ("env-script", "#!/usr/bin/env sh\nd=4", true),
      ("with-options", "#!/bin/sh -eu\ne=5", true),
      ("wrong.sh", "#!/bin/bash\nf=6", false),
      ("env-options.sh", "#!/usr/bin/env -S sh\ng=7", false),
      ("env-assignment.sh", "#!/usr/bin/env LANG=C sh\nh=8", false),
      ("relative.sh", "#!sh\ni=9", false),
      ("other.txt", "j=10", false),
    ];
    for (path, source, _) in cases {
      fixture.write(path, source);
    }
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&fixture.root);
    app.refresh(&AtomicBool::new(false)).unwrap();
    assert_eq!(app.workspace().documents().count(), 5);
    for (path, _, selected) in cases {
      assert_eq!(
        app.document_id(&fixture.root.join(path)).is_some(),
        selected,
        "{path}"
      );
    }
    assert_eq!(app.failures(), []);
  }

  #[test]
  fn workspace_collection_applies_nested_gitignores_without_other_ignore_files()
  {
    for repository in [false, true] {
      let fixture = Fixture::new();
      fixture.write(".gitignore", "*.sh\n!keep.sh\ntarget/\n");
      fixture.write("nested/.gitignore", "!included.sh\n");
      fixture.write(".ignore", "keep.sh\n");
      if repository {
        fixture.write(".git/info/exclude", "keep.sh\n");
        fixture.write(".git/.gitignore", "[z-a]\n");
        fixture.write(".git/keep.sh", "metadata=1\n");
      }
      fixture.write("target/.gitignore", "[z-a]\n");
      let invalid = fixture.write("target/keep.sh", "");
      std::fs::write(invalid, [0xff]).unwrap();
      let cases = [
        ("keep.sh", true),
        ("excluded.sh", false),
        ("nested/included.sh", true),
        ("nested/excluded.sh", false),
        (".hidden/keep.sh", true),
        ("target/other.sh", false),
      ];
      for (path, _) in cases {
        fixture.write(path, "value=1\n");
      }
      let mut app =
        Application::new(Settings::new(fixture.root.clone())).unwrap();
      app.add_input(&fixture.root);
      app.refresh(&AtomicBool::new(false)).unwrap();
      assert_eq!(app.workspace().documents().count(), 3);
      for (path, selected) in cases {
        assert_eq!(
          app.document_id(&fixture.root.join(path)).is_some(),
          selected,
          "repository={repository}, {path}"
        );
      }
      assert_eq!(app.failures(), []);
    }
  }

  #[test]
  fn ignored_documents_remain_available_when_opened_selected_or_sourced() {
    let fixture = Fixture::new();
    fixture.write(".gitignore", "ignored/\n");
    let main = fixture.write("main.sh", ". ./ignored/lib.sh\n");
    let library = fixture.write("ignored/lib.sh", "library=1\n");
    let opened = fixture.write("ignored/open.sh", "disk=1\n");
    let selected = fixture.write("ignored/explicit.txt", "explicit=1\n");
    let skipped = fixture.write("ignored/skipped.sh", "skipped=1\n");
    let settings = Settings::new(fixture.root.clone());
    let mut app = Application::new(settings).unwrap();
    let cancel = AtomicBool::new(false);
    app.add_input(&fixture.root);
    app.add_input(&selected);
    app.open(&opened, "live=1\n", &cancel).unwrap();
    app.refresh(&cancel).unwrap();
    assert_eq!(app.workspace().documents().count(), 4);
    assert!(app.document_id(&skipped).is_none());
    let id = app.document_id(&opened).unwrap();
    assert_eq!(app.workspace().document(id).unwrap().source(), "live=1\n");
    let id = app.document_id(&main).unwrap();
    assert!(matches!(
      app.workspace().dependencies(id).next().unwrap().1,
      DependencyState::Resolved(_)
    ));
    assert!(app.document_id(&library).is_some());
    assert!(app.document_id(&selected).is_some());
    assert_eq!(app.failures(), []);
  }

  #[test]
  fn invalid_gitignore_patterns_report_the_file_and_keep_valid_rules() {
    let fixture = Fixture::new();
    let ignore = fixture.write(".gitignore", "[z-a]\nexcluded.sh\n");
    let included = fixture.write("included.sh", "included=1\n");
    let excluded = fixture.write("excluded.sh", "excluded=1\n");
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&fixture.root);
    app.refresh(&AtomicBool::new(false)).unwrap();
    assert!(app.document_id(&included).is_some());
    assert!(app.document_id(&excluded).is_none());
    assert_eq!(app.failures().len(), 1);
    assert_eq!(app.failures()[0].path, ignore);
    assert!(app.failures()[0].message.contains("line 1"));
  }

  #[tokio::test]
  async fn workspace_notifications_update_only_affected_files_and_keep_overlays()
   {
    use shellsmith::interface::application::{RequestScope, Session};

    async fn names(session: &Session) -> Vec<String> {
      session
        .request(RequestScope::Workspace, |app, _| {
          Ok(
            app
              .workspace_symbols("")
              .map(|entry| {
                String::from_utf8(entry.symbol.name.clone()).unwrap()
              })
              .collect(),
          )
        })
        .await
        .unwrap()
    }

    let fixture = Fixture::new();
    fixture.write(".gitignore", "first/new/ignored.sh\n");
    let first = fixture.write("first/main.sh", "alpha=1\n");
    let second = fixture.write("second/main.sh", "beta=1\n");
    let runner = fixture.write("first/runner", "text\n");
    let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
    session.add_input(&fixture.root).unwrap();
    session.set_watching(true).unwrap();
    assert_eq!(names(&session).await, ["alpha", "beta"]);
    fixture.write("second/unnotified.sh", "unnotified=1\n");
    session
      .set_overlay(&first, Some("live=1\n".into()))
      .unwrap();
    assert_eq!(names(&session).await, ["beta", "live"]);
    fixture.write("first/new/child.sh", "child=1\n");
    fixture.write("first/new/ignored.sh", "ignored=1\n");
    session
      .files_changed(&[fixture.root.join("first/new")])
      .unwrap();
    assert_eq!(names(&session).await, ["beta", "child", "live"]);
    fixture.write("first/runner", "#!/bin/sh\nutility=1\n");
    session
      .files_changed(std::slice::from_ref(&runner))
      .unwrap();
    assert_eq!(names(&session).await, ["beta", "child", "live", "utility"]);
    std::fs::remove_file(&second).unwrap();
    session.files_changed(&[second]).unwrap();
    assert_eq!(names(&session).await, ["child", "live", "utility"]);
    session.remove_input(&fixture.root).unwrap();
    assert_eq!(names(&session).await, ["live"]);
  }

  #[test]
  fn overlays_close_and_disk_failures_never_reuse_stale_results() {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "disk=1");
    let cancel = AtomicBool::new(false);
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&file);
    app.open(&file, "live=1", &cancel).unwrap();
    app.refresh(&cancel).unwrap();
    let id = app.document_id(&file).unwrap();
    assert_eq!(app.workspace().document(id).unwrap().source(), "live=1");
    std::fs::write(&file, "changed=1").unwrap();
    app.refresh(&cancel).unwrap();
    assert_eq!(app.workspace().document(id).unwrap().source(), "live=1");
    app.close(&file, &cancel).unwrap();
    assert_eq!(app.workspace().document(id).unwrap().source(), "changed=1");
    std::fs::write(&file, [0xff]).unwrap();
    app.refresh(&cancel).unwrap();
    assert!(app.workspace().document(id).is_none());
    assert_eq!(app.failures().len(), 1);
    fixture.write("main.sh", "fixed=1");
    app.refresh(&cancel).unwrap();
    assert_eq!(app.document_id(&file), Some(id));
    std::fs::remove_file(&file).unwrap();
    app.refresh(&cancel).unwrap();
    assert!(app.document_id(&file).is_none());
  }

  #[test]
  fn sources_follow_entry_directory_and_cycles_with_current_revisions() {
    let fixture = Fixture::new();
    let entry = fixture.write("sub/main.sh", ". ./lib.sh\nf\n");
    let lib = fixture.write("sub/lib.sh", "f() { :; }\n. ./main.sh\n");
    let settings = Settings::new(fixture.root.clone());
    let mut app = Application::new(settings).unwrap();
    app.add_input(&entry);
    let cancel = AtomicBool::new(false);
    app.refresh(&cancel).unwrap();
    let id = app.document_id(&entry).unwrap();
    assert!(matches!(
      app.workspace().dependencies(id).next().unwrap().1,
      DependencyState::Resolved(_)
    ));
    assert_eq!(app.workspace().documents().count(), 2);
    app.open(&lib, "f() { :; }", &cancel).unwrap();
    app.refresh(&cancel).unwrap();
    let spans = app.classify(id, &cancel).unwrap();
    assert!(spans.iter().any(|span| span.kind == SemanticKind::Function));
    app.change(&lib, "g() { :; }", &cancel).unwrap();
    app.refresh(&cancel).unwrap();
    assert!(
      !app
        .classify(id, &cancel)
        .unwrap()
        .iter()
        .any(|span| span.kind == SemanticKind::Function)
    );
    app.close(&lib, &cancel).unwrap();
    std::fs::remove_file(&lib).unwrap();
    app.refresh(&cancel).unwrap();
    assert!(matches!(
      app.workspace().dependencies(id).next().unwrap().1,
      DependencyState::Unresolved(_)
    ));
  }

  #[test]
  fn failed_cd_keeps_the_current_directory_for_source_resolution() {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "cd sub; . ./lib.sh\n");
    let library = fixture.write("lib.sh", "readonly x=1");
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&file);
    let cancel = AtomicBool::new(false);
    app.refresh(&cancel).unwrap();
    assert_eq!(app.workspace().documents().count(), 2);
    let id = app.document_id(&file).unwrap();
    assert!(matches!(
      app.workspace().dependencies(id).next().unwrap().1,
      DependencyState::Resolved(_)
    ));
    assert_eq!(app.document_links(id, &cancel).unwrap()[0].1, library);
    assert!(
      app
        .classify(id, &cancel)
        .unwrap()
        .iter()
        .any(|span| span.modifiers.contains(&Modifier::DefaultLibrary))
    );
  }

  #[test]
  fn cancellation_does_not_publish_partial_analysis() {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "x=1");
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&file);
    app.refresh(&AtomicBool::new(false)).unwrap();
    let id = app.document_id(&file).unwrap();
    let completed =
      std::ptr::from_ref(app.workspace().analysis().unwrap()).addr();
    assert!(app.classify(id, &AtomicBool::new(true)).is_err());
    assert_eq!(
      std::ptr::from_ref(app.workspace().analysis().unwrap()).addr(),
      completed
    );
    app.open(&file, "x=2", &AtomicBool::new(false)).unwrap();
    assert!(app.classify(id, &AtomicBool::new(true)).is_err());
    assert!(app.workspace().analysis().is_none());
  }

  #[cfg(unix)]
  #[test]
  fn symbolic_links_share_documents_and_directory_cycles_terminate() {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "x=1");
    let link = fixture.root.join("link.sh");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&fixture.root);
    app.refresh(&AtomicBool::new(false)).unwrap();
    assert_eq!(app.workspace().documents().count(), 1);
    std::os::unix::fs::symlink(&fixture.root, fixture.root.join("cycle"))
      .unwrap();
    app.add_input(&link);
    app.refresh(&AtomicBool::new(false)).unwrap();
    assert_eq!(app.failures(), []);
    assert_eq!(app.document_id(&link), app.document_id(&file));
    app.open(&link, "x=2", &AtomicBool::new(false)).unwrap();
    app.refresh(&AtomicBool::new(false)).unwrap();
    assert_eq!(app.workspace().documents().count(), 1);
  }

  #[test]
  fn unsaved_source_and_path_entries_use_the_request_directory() {
    let fixture = Fixture::new();
    let entry = fixture.write("main.sh", ". lib\nf\n");
    let library = fixture.root.join("lib");
    let mut settings = Settings::new(fixture.root.clone());
    settings
      .initial
      .environment
      .insert(b"PATH".to_vec(), b"./missing:".to_vec());
    let mut app = Application::new(settings).unwrap();
    let cancelled = AtomicBool::new(false);
    app.add_input(&entry);
    app.open(&library, "f() { :; }", &cancelled).unwrap();
    app.refresh(&cancelled).unwrap();
    let id = app.document_id(&entry).unwrap();
    assert!(matches!(
      app.workspace().dependencies(id).next().unwrap().1,
      DependencyState::Resolved(_)
    ));
    assert!(
      app
        .classify(id, &cancelled)
        .unwrap()
        .iter()
        .any(|span| span.kind == SemanticKind::Function)
    );
    let mut settings = app.settings().clone();
    settings.initial.environment.remove(b"PATH".as_slice());
    app.configure(settings).unwrap();
    assert!(app.workspace().analysis().is_none());
    app.refresh(&cancelled).unwrap();
    assert!(matches!(
      app.workspace().dependencies(id).next().unwrap().1,
      DependencyState::Unresolved(_)
    ));
  }

  #[test]
  fn sourced_cd_changes_the_directory_for_later_references() {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "cd elsewhere\n");
    let entry = fixture.write(
      "main.sh",
      &format!(". '{}'\n. ./next.sh\n", library.display()),
    );
    fixture.write("next.sh", "wrong=1");
    fixture.write("elsewhere/next.sh", "right=1");
    let settings = Settings::new(fixture.root.clone());
    let mut app = Application::new(settings).unwrap();
    app.add_input(&entry);
    app.refresh(&AtomicBool::new(false)).unwrap();
    let id = app.document_id(&entry).unwrap();
    let states: Vec<_> = app
      .workspace()
      .dependencies(id)
      .map(|(_, state)| state)
      .collect();
    assert!(matches!(states[0], DependencyState::Resolved(_)));
    assert!(matches!(states[1], DependencyState::Resolved(_)));
    assert_eq!(app.workspace().symbols_named(b"wrong").count(), 0);
    assert_eq!(app.workspace().symbols_named(b"right").count(), 1);
  }

  #[cfg(unix)]
  #[test]
  fn parent_components_are_resolved_after_symbolic_links() {
    let fixture = Fixture::new();
    let file = fixture.write("real/a.sh", "x=1");
    std::fs::create_dir(fixture.root.join("real/sub")).unwrap();
    std::os::unix::fs::symlink(
      fixture.root.join("real/sub"),
      fixture.root.join("link"),
    )
    .unwrap();
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&fixture.root.join("link/../a.sh"));
    app.refresh(&AtomicBool::new(false)).unwrap();
    assert_eq!(app.failures(), []);
    assert!(app.document_id(&file).is_some());
  }

  #[test]
  fn source_search_uses_only_the_conditions_required_by_the_path() {
    for (prefix, resolved) in [
      (": \"$(( $expr ))\"", true),
      ("getopts x PATH", true),
      ("local PATH=elsewhere", false),
      ("fc", false),
    ] {
      let fixture = Fixture::new();
      let file = fixture.write("main.sh", &format!("{prefix}; . ./lib.sh\n"));
      fixture.write("lib.sh", "x=1");
      let settings = Settings::new(fixture.root.clone());
      let mut app = Application::new(settings).unwrap();
      app.add_input(&file);
      app.refresh(&AtomicBool::new(false)).unwrap();
      let id = app.document_id(&file).unwrap();
      assert_eq!(
        matches!(
          app.workspace().dependencies(id).next().unwrap().1,
          DependencyState::Resolved(_)
        ),
        resolved,
        "{prefix}"
      );
    }
  }

  #[test]
  fn entry_directories_are_independent_and_sources_inherit_directory_changes() {
    let fixture = Fixture::new();
    let cancel = AtomicBool::new(false);
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial =
      shellsmith::interface::application::engine::InitialEnvironment::empty();
    settings
      .initial
      .environment
      .insert(b"PATH".to_vec(), b".:".to_vec());
    let mut app = Application::new(settings).unwrap();
    let mut expected = Vec::new();
    for (directory, operand) in
      [("a", "\"$PWD/lib/first.sh\""), ("b", "./lib/first.sh")]
    {
      let source = format!(". {operand}\nf\n. done.sh\n");
      let main = fixture.write(&format!("{directory}/main.sh"), &source);
      let first = fixture.write(
        &format!("{directory}/lib/first.sh"),
        ". ./second.sh\ncd ./moved\n",
      );
      let second =
        fixture.write(&format!("{directory}/second.sh"), "f() { :; }\n");
      let done =
        fixture.write(&format!("{directory}/moved/done.sh"), "done=1\n");
      app.open(&main, &source, &cancel).unwrap();
      expected.push((main, first, second, done));
    }
    app.refresh(&cancel).unwrap();
    assert_eq!(app.failures(), []);
    assert_eq!(app.workspace().documents().count(), 8);
    for (main, first, second, done) in expected {
      let id = app.document_id(&main).unwrap();
      let links = app.document_links(id, &cancel).unwrap();
      assert_eq!(
        links.into_iter().map(|(_, path)| path).collect::<Vec<_>>(),
        vec![first.clone(), done]
      );
      let first_id = app.document_id(&first).unwrap();
      let second_id = app.document_id(&second).unwrap();
      assert!(matches!(
        app.workspace().dependencies(first_id).next().unwrap().1,
        DependencyState::Resolved(snapshot) if snapshot.document == second_id
      ));
      assert!(
        app
          .classify(id, &cancel)
          .unwrap()
          .iter()
          .any(|span| { span.kind == SemanticKind::Function })
      );
    }
  }
}

mod environment {
  use crate::fixtures;

  use fixtures::Fixture;
  use shellsmith::interface::application::engine::{
    DependencyState, InitialBinding, Modifier, Value,
  };
  use shellsmith::interface::application::{Application, Settings};
  use std::sync::atomic::AtomicBool;

  fn application(fixture: &Fixture) -> Application {
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial.environment.clear();
    settings.initial.environment.insert(
      b"HOME".to_vec(),
      fixture.root.as_os_str().as_encoded_bytes().to_vec(),
    );
    Application::new(settings).unwrap()
  }

  #[test]
  fn missing_startup_variables_resolve_defaults_until_their_values_become_unknown()
   {
    let fixture = Fixture::new();
    fixture.write("default.sh", ":\n");
    fixture.write("assigned.sh", ":\n");
    for (prefix, operand, target) in [
      ("", "${CONFIG-./default.sh}", Some("default.sh")),
      ("", "${CONFIG:-./default.sh}", Some("default.sh")),
      ("", "${CONFIG:=./default.sh}", Some("default.sh")),
      ("CONFIG=; ", "${CONFIG-./default.sh}", None),
      ("CONFIG=; ", "${CONFIG:-./default.sh}", Some("default.sh")),
      ("CONFIG=./assigned.sh; ", "$CONFIG", Some("assigned.sh")),
      (
        "if condition; then unset CONFIG; fi; ",
        "${CONFIG:-./default.sh}",
        Some("default.sh"),
      ),
      (
        "if condition; then CONFIG=./assigned.sh; fi; ",
        "${CONFIG:-./default.sh}",
        None,
      ),
      ("read CONFIG; ", "${CONFIG:-./default.sh}", None),
      (
        "read CONFIG; unset CONFIG; ",
        "${CONFIG:-./default.sh}",
        Some("default.sh"),
      ),
      (
        "read CONFIG; CONFIG=./assigned.sh; ",
        "$CONFIG",
        Some("assigned.sh"),
      ),
    ] {
      let source = format!("{prefix}. \"{operand}\"\n");
      let main = fixture.write("main.sh", &source);
      let mut app = application(&fixture);
      app.add_input(&main);
      let cancelled = AtomicBool::new(false);
      app.refresh(&cancelled).unwrap();
      let id = app.document_id(&main).unwrap();
      let targets: Vec<_> = app
        .document_links(id, &cancelled)
        .unwrap()
        .into_iter()
        .map(|(_, path)| path)
        .collect();
      assert_eq!(
        targets,
        target
          .map(|path| fixture.root.join(path))
          .into_iter()
          .collect::<Vec<_>>(),
        "{source}"
      );
    }
  }

  #[test]
  fn explicit_initial_uncertainty_is_not_replaced_with_unset() {
    let fixture = Fixture::new();
    fixture.write("default.sh", ":\n");
    for name in [
      "CONFIG",
      "LANG",
      "LC_ALL",
      "LC_CTYPE",
      "LC_COLLATE",
      "CDPATH",
    ] {
      let main =
        fixture.write("main.sh", &format!(". \"${{{name}-./default.sh}}\"\n"));
      let mut app = application(&fixture);
      let mut settings = app.settings().clone();
      settings.initial.other_variables = InitialBinding::Unknown;
      app.configure(settings).unwrap();
      app.add_input(&main);
      let cancelled = AtomicBool::new(false);
      app.refresh(&cancelled).unwrap();
      let id = app.document_id(&main).unwrap();
      assert!(
        app.document_links(id, &cancelled).unwrap().is_empty(),
        "{name}"
      );
      assert_eq!(app.workspace().documents().count(), 1, "{name}");
    }
  }

  #[test]
  fn shell_initialized_variables_skip_unset_defaults_and_held_ones_are_set() {
    let fixture = Fixture::new();
    fixture.write("default.sh", ":\n");
    for (name, held) in [
      ("LINENO", true),
      ("PPID", true),
      ("PS1", false),
      ("PS2", false),
      ("PS4", false),
    ] {
      for (source, linked) in [
        (format!(". \"${{{name}-./default.sh}}\"\n"), false),
        (format!(". \"${{{name}+./default.sh}}\"\n"), held),
      ] {
        let main = fixture.write("main.sh", &source);
        let mut app = application(&fixture);
        app.add_input(&main);
        let cancelled = AtomicBool::new(false);
        app.refresh(&cancelled).unwrap();
        let id = app.document_id(&main).unwrap();
        assert_eq!(
          app.document_links(id, &cancelled).unwrap().len(),
          usize::from(linked),
          "{source}"
        );
        assert_eq!(
          app.workspace().documents().count(),
          1 + usize::from(linked),
          "{source}"
        );
      }
    }
  }

  #[test]
  fn quoted_environment_sources_are_loaded_and_feed_semantic_classification() {
    let fixture = Fixture::new();
    let file = fixture.write(
      "main.sh",
      ". ./prefix.sh; . \"$HOME/lib.sh\"; f; : \"${HOME}\"\n",
    );
    fixture.write("prefix.sh", "prefix=1\n");
    fixture.write("lib.sh", "f() { :; }\nreadonly HOME\n");
    let mut app = application(&fixture);
    app.add_input(&file);
    let cancel = AtomicBool::new(false);
    app.refresh(&cancel).unwrap();
    assert_eq!(app.workspace().documents().count(), 3);
    let id = app.document_id(&file).unwrap();
    assert!(matches!(
      app.workspace().dependencies(id).next().unwrap().1,
      DependencyState::Resolved(_)
    ));
    let spans = app.classify(id, &cancel).unwrap();
    assert!(spans.iter().any(|span| span.kind
      == shellsmith::interface::application::engine::SemanticKind::Function));
    assert_eq!(
      spans
        .iter()
        .filter(|span| span.modifiers.contains(&Modifier::Environment))
        .count(),
      2
    );
    assert!(
      spans
        .iter()
        .any(|span| span.modifiers.contains(&Modifier::Readonly)
          && span.modifiers.contains(&Modifier::Environment))
    );
  }

  #[test]
  fn source_operand_uses_the_environment_before_command_prefix_assignments() {
    let fixture = Fixture::new();
    let file = fixture.write(
      "main.sh",
      "HOME=/other . \"$HOME/lib.sh\"; . \"$HOME/after.sh\"\n",
    );
    fixture.write("lib.sh", "loaded=1\n");
    fixture.write("after.sh", "unreachable=1\n");
    let mut app = application(&fixture);
    app.add_input(&file);
    let cancelled = AtomicBool::new(false);
    app.refresh(&cancelled).unwrap();
    let id = app.document_id(&file).unwrap();
    let states: Vec<_> = app
      .workspace()
      .dependencies(id)
      .map(|(_, state)| state)
      .collect();
    assert!(matches!(states[0], DependencyState::Resolved(_)));
    assert!(matches!(states[1], DependencyState::Unresolved(_)));
    assert_eq!(app.workspace().symbols_named(b"loaded").count(), 1);
    assert_eq!(app.workspace().symbols_named(b"unreachable").count(), 0);
  }

  #[test]
  fn source_discovery_never_uses_overwritten_or_uncertain_environment_values() {
    let fixture = Fixture::new();
    fixture.write("lib.sh", "from_library=1\n");
    for (prefix, library_count) in [
      ("HOME=/other; ", 0),
      ("unset HOME; ", 0),
      ("if condition; then HOME=/other; fi; ", 1),
      ("read HOME; ", 0),
      ("f() { HOME=/other; }; f; ", 0),
      (". missing; ", 0),
    ] {
      let file = fixture.write(
        "main.sh",
        &format!("{prefix}. \"$HOME/lib.sh\"\n: \"$from_library\"\n"),
      );
      let mut app = application(&fixture);
      app.add_input(&file);
      let cancelled = AtomicBool::new(false);
      app.refresh(&cancelled).unwrap();
      let id = app.document_id(&file).unwrap();
      assert!(
        app.document_links(id, &cancelled).unwrap().is_empty(),
        "{prefix}"
      );
      assert!(
        app
          .workspace()
          .dependencies(id)
          .all(|(_, state)| matches!(state, DependencyState::Unresolved(_))),
        "{prefix}"
      );
      assert_eq!(
        app
          .workspace()
          .documents()
          .filter(|doc| app.path(doc.snapshot().document).is_some())
          .count(),
        1 + library_count,
        "{prefix}"
      );
      assert_eq!(
        app.workspace().symbols_named(b"from_library").count(),
        library_count,
        "{prefix}"
      );
      let read = app
        .workspace()
        .document(id)
        .unwrap()
        .occurrences()
        .iter()
        .find(|site| site.name == Value::Literal(b"from_library".to_vec()))
        .unwrap()
        .id;
      assert!(
        !matches!(
          app
            .workspace()
            .analysis()
            .unwrap()
            .summarize(shellsmith::interface::application::engine::UseSite::Syntax(
              read
            ))
            .status(),
          shellsmith::interface::application::engine::ResolutionStatus::Resolved(_)
        ),
        "{prefix}"
      );
    }
  }

  #[test]
  fn excluded_eval_and_alias_effects_do_not_block_later_source_discovery() {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "f() { :; }\nreadonly HOME\n");
    let hidden = fixture.write("hidden.sh", "hidden=1\n");
    for call in [
      "eval '. ./hidden.sh; cd /; PATH=/other; HOME=/other'",
      "eval \"$(brew shellenv)\"",
      "alias .='echo'; alias f=printf",
      "alias .='echo'; unalias .",
    ] {
      let source = format!("{call}; . \"$HOME/lib.sh\"; f; : \"$HOME\"");
      let file = fixture.write("main.sh", &source);
      let mut app = application(&fixture);
      app.add_input(&file);
      let cancel = AtomicBool::new(false);
      app.refresh(&cancel).unwrap();
      assert!(app.document_id(&library).is_some(), "{call}");
      assert!(app.document_id(&hidden).is_none(), "{call}");
      assert_eq!(app.workspace().documents().count(), 2, "{call}");
      let id = app.document_id(&file).unwrap();
      let spans = app.classify(id, &cancel).unwrap();
      let function = source.rfind("f; :").unwrap();
      assert!(
        spans
          .iter()
          .any(|span| span.location.bytes.start == function
            && span.kind
              == shellsmith::interface::application::engine::SemanticKind::Function),
        "{call}"
      );
      let read = source.rfind("HOME").unwrap();
      assert!(
        spans.iter().any(|span| span.location.bytes.start == read
          && span.modifiers.contains(&Modifier::Environment)
          && span.modifiers.contains(&Modifier::Readonly)),
        "{call}"
      );
    }
  }

  #[test]
  fn nested_sources_recheck_environment_after_each_file_and_after_updates() {
    let fixture = Fixture::new();
    let file =
      fixture.write("main.sh", ". \"$HOME/first.sh\"; . \"$HOME/last.sh\"\n");
    let first = fixture.write("first.sh", ". \"${HOME}/nested.sh\"\n");
    fixture.write("nested.sh", "nested=1\n");
    let last = fixture.write("last.sh", "last=1\n");
    let mut app = application(&fixture);
    app.add_input(&file);
    let cancel = AtomicBool::new(false);
    app.refresh(&cancel).unwrap();
    assert_eq!(app.workspace().documents().count(), 4);
    assert!(app.document_id(&last).is_some());
    std::fs::write(&first, "unset HOME\n").unwrap();
    app.refresh(&cancel).unwrap();
    assert_eq!(app.workspace().documents().count(), 2);
    assert!(app.document_id(&last).is_none());
    std::fs::write(&first, ":\n").unwrap();
    app.refresh(&cancel).unwrap();
    assert!(app.document_id(&last).is_some());
  }

  #[test]
  fn quoted_paths_preserve_spaces_defaults_and_unsupported_expansions() {
    let fixture = Fixture::new();
    fixture.write("with space/lib.sh", "found=1\n");
    for (operand, expected) in [
      ("\"$HOME/with space/lib.sh\"", 2),
      ("\"${HOME}/with space/\"'lib.sh'", 2),
      ("\"${HOME:-fallback}/with space/lib.sh\"", 2),
      ("\"${#HOME}/with space/lib.sh\"", 1),
      ("$HOME/with\\ space/lib.sh", 2),
      ("'${HOME}/with space/lib.sh'", 1),
      ("\"$HOME/with space/\"*.sh", 2),
      ("\"$(echo ignored)/lib.sh\"", 1),
    ] {
      let file = fixture.write("main.sh", &format!(". {operand}\n"));
      let mut app = application(&fixture);
      app.add_input(&file);
      app.refresh(&AtomicBool::new(false)).unwrap();
      assert_eq!(app.workspace().documents().count(), expected, "{operand}");
    }
  }

  #[test]
  fn function_and_loop_contexts_cannot_share_an_unproven_source_target() {
    let fixture = Fixture::new();
    fixture.write("lib.sh", "HOME=/changed\n");
    for source in [
      "f() { . \"$HOME/lib.sh\"; }; f; f\n",
      "while condition; do . \"$HOME/lib.sh\"; done\n",
    ] {
      let file = fixture.write("main.sh", source);
      let mut app = application(&fixture);
      app.add_input(&file);
      app.refresh(&AtomicBool::new(false)).unwrap();
      let id = app.document_id(&file).unwrap();
      assert!(
        app
          .workspace()
          .dependencies(id)
          .all(|(_, state)| matches!(state, DependencyState::Unresolved(_)))
      );
      assert_eq!(app.workspace().documents().count(), 2);
      assert_eq!(app.document_links(id, &AtomicBool::new(false)).unwrap(), []);
    }
  }

  #[test]
  fn workspace_entries_keep_shared_source_contexts_separate() {
    let fixture = Fixture::new();
    let first = fixture.write("first.sh", ". \"$HOME/shared.sh\"\n");
    let second =
      fixture.write("second.sh", "HOME=/other; . \"$CONFIG/shared.sh\"\n");
    let shared = fixture.write("shared.sh", ". \"$HOME/leaf.sh\"\n");
    fixture.write("leaf.sh", "leaf=1\n");
    let mut app = application(&fixture);
    let mut settings = app.settings().clone();
    settings.initial.environment.insert(
      b"CONFIG".to_vec(),
      fixture.root.as_os_str().as_encoded_bytes().to_vec(),
    );
    app.configure(settings).unwrap();
    app.add_input(&first);
    app.add_input(&second);
    app.refresh(&AtomicBool::new(false)).unwrap();
    let id = app.document_id(&shared).unwrap();
    assert!(matches!(
      app.workspace().dependencies(id).next().unwrap().1,
      DependencyState::Unresolved(_)
    ));
    assert_eq!(app.workspace().symbols_named(b"leaf").count(), 1);
  }

  #[test]
  fn environment_changes_invalidate_discovery() {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", ". \"${CONFIG-./lib.sh}\"\n");
    let first = fixture.write("lib.sh", "old=1\n");
    let second = fixture.write("other/lib.sh", "new=1\n");
    let mut app = application(&fixture);
    app.add_input(&file);
    let cancel = AtomicBool::new(false);
    for (value, target) in [
      (None, Some(&first)),
      (Some("./other/lib.sh"), Some(&second)),
      (Some("./lib.sh"), Some(&first)),
      (Some(""), None),
      (None, Some(&first)),
    ] {
      let mut settings = app.settings().clone();
      if let Some(value) = value {
        settings
          .initial
          .environment
          .insert(b"CONFIG".to_vec(), value.as_bytes().to_vec());
      } else {
        settings.initial.environment.remove(b"CONFIG".as_slice());
      }
      app.configure(settings).unwrap();
      app.refresh(&cancel).unwrap();
      for path in [&first, &second] {
        assert_eq!(
          app.document_id(path).is_some(),
          target == Some(path),
          "{value:?}"
        );
      }
    }
  }
}

mod paths {
  use crate::fixtures;
  use fixtures::Fixture;
  use shellsmith::interface::application::engine::{
    DependencyState, InitialEnvironment,
  };
  use shellsmith::interface::application::{Application, Settings};
  use std::sync::atomic::AtomicBool;

  #[test]
  fn glob_sources_respect_hidden_entries_and_directory_boundaries() {
    let fixture = Fixture::new();
    fixture.write("lib/visible.sh", "visible=1");
    fixture.write("lib/.hidden.sh", "hidden=1");
    fixture.write("lib/nested/leaf.sh", "nested=1");
    fixture.write("dot/x.hidden.sh", "visible=1");
    fixture.write("dot/.hidden.sh", "hidden=1");
    fixture.write("escaped/\\x.sh", "escaped=1");
    for (source, targets) in [
      (". ./lib/*.sh", vec!["lib/visible.sh"]),
      (". ./lib/.*.sh", vec!["lib/.hidden.sh"]),
      (". ./lib/*/leaf.sh", vec!["lib/nested/leaf.sh"]),
      (". ./lib/visible.sh/*.sh", vec![]),
      (". ./lib/missing*.sh", vec![]),
      (". './lib/*.sh'", vec![]),
      (". ./dot/*[.]hidden.sh", vec![]),
      (
        r"pattern='./lib/\v*.sh'; . $pattern",
        vec!["lib/visible.sh"],
      ),
      (r"pattern='./\lib/*.sh'; . $pattern", vec!["lib/visible.sh"]),
      (r"pattern='./escaped/\*.sh'; . $pattern", vec![]),
    ] {
      let main = fixture.write("main.sh", source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      let cancelled = AtomicBool::new(false);
      app.refresh(&cancelled).unwrap();
      let id = app.document_id(&main).unwrap();
      let actual: Vec<_> = app
        .document_links(id, &cancelled)
        .unwrap()
        .into_iter()
        .map(|(_, path)| path)
        .collect();
      let expected: Vec<_> = targets
        .into_iter()
        .map(|path| fixture.root.join(path))
        .collect();
      assert_eq!(actual, expected, "{source}");
    }
  }

  #[test]
  fn computed_words_and_globs_resolve_sources_from_the_current_directory() {
    let fixture = Fixture::new();
    let library = fixture.write("sub/library2.sh", "found=1\n");
    for source in [
      "cd ./sub; . ./library2.sh",
      "eval 'cd /'; . ./sub/library2.sh",
      "PATH=./sub; eval 'PATH=/other; cd /'; . library2.sh",
      "alias .='echo'; . ./sub/library2.sh",
      "n=1; . ./sub/library$(( $n + 1 )).sh",
      "base=./sub/file; . \"${base%/*}/library$((1+1)).sh\"",
      "set -- ./sub/library2.sh; . \"$1\"",
      "read tail; set -- ./sub/library2.sh \"$tail\"; . \"$@\"",
      "set -- ./sub/library2.sh; for lib; do . \"$lib\"; done",
      "f() { . \"$1\"; }; f ./sub/library2.sh",
      "HOME=./sub; . ~/library2.sh",
      "cd ./sub; . ./library[0-9].sh",
      "pattern='./sub/*.sh'; . $pattern",
      "set -- ./sub/*.sh; . \"$1\"",
      "CDPATH=.; cd sub; . ./library2.sh",
      "HOME=./sub; cd; . ./library2.sh",
      "cd sub; cd ..; cd -; . ./library2.sh",
      "cd sub/; . ./library2.sh",
      "cd sub; (cd ..); . ./library2.sh",
    ] {
      let main = fixture.write("main.sh", source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      app.refresh(&AtomicBool::new(false)).unwrap();
      assert!(
        app.document_id(&library).is_some(),
        "{source}: {:?}",
        app.failures()
      );
      assert_eq!(
        app.workspace().symbols_named(b"found").count(),
        1,
        "{source}"
      );
    }
  }

  #[test]
  fn cd_sets_pwd_and_pwd_output_only_for_spellings_posix_fixes() {
    use shellsmith::interface::application::engine::{
      OccurrenceKind, Value, ValueIssue,
    };
    let fixture = Fixture::new();
    fixture.write("sub/a/file.sh", "");
    let root = fixture.root.to_str().unwrap();
    let exact = |path: String| Value::Literal(path.into_bytes());
    let unspecified = Value::Unknown(ValueIssue::Unspecified);
    for (source, expected) in [
      (
        "cd sub; result=$PWD".to_owned(),
        exact(format!("{root}/sub")),
      ),
      (
        "cd ./sub/./a; result=$PWD".to_owned(),
        exact(format!("{root}/sub/a")),
      ),
      (
        "CDPATH=sub/; cd a; result=$PWD".to_owned(),
        exact(format!("{root}/sub/a")),
      ),
      (
        format!("cd /{root}/sub; result=$PWD"),
        exact(format!("/{root}/sub")),
      ),
      ("cd /..; result=$PWD".to_owned(), exact("/..".to_owned())),
      (
        "cd sub; cd ..; result=$OLDPWD".to_owned(),
        exact(format!("{root}/sub")),
      ),
      (
        "cd sub; result=$(command -p pwd)".to_owned(),
        exact(format!("{root}/sub")),
      ),
      ("cd sub/; result=$PWD".to_owned(), unspecified.clone()),
      ("cd sub; cd ..; result=$PWD".to_owned(), unspecified.clone()),
      ("cd sub; cd .; result=$PWD".to_owned(), unspecified.clone()),
      ("cd sub//a; result=$PWD".to_owned(), unspecified.clone()),
      (
        "cd sub/; cd a; result=$OLDPWD".to_owned(),
        unspecified.clone(),
      ),
      (
        "cd sub/; result=$(command -p pwd)".to_owned(),
        unspecified.clone(),
      ),
      (
        "cd /..; result=$(command -p pwd)".to_owned(),
        Value::Unknown(ValueIssue::UnsupportedExpansion),
      ),
      (
        "readonly PWD; PWD=/x; result=$(command -p pwd)".to_owned(),
        exact(root.to_owned()),
      ),
      (
        "readonly PWD; cd sub; result=$(command -p pwd)".to_owned(),
        Value::Unknown(ValueIssue::Dynamic),
      ),
    ] {
      let source = format!("{source}; : \"$result\"");
      let main = fixture.write("main.sh", &source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      app.refresh(&AtomicBool::new(false)).unwrap();
      let id = app.document_id(&main).unwrap();
      app.classify(id, &AtomicBool::new(false)).unwrap();
      let read = app
        .workspace()
        .document(id)
        .unwrap()
        .occurrences()
        .iter()
        .rev()
        .find(|site| {
          site.kind == OccurrenceKind::VariableExpansion
            && site.name == Value::Literal(b"result".to_vec())
        })
        .unwrap()
        .id;
      assert_eq!(
        app
          .workspace()
          .analysis()
          .unwrap()
          .values(read)
          .map(|(_, value)| value.clone())
          .collect::<Vec<_>>(),
        [expected],
        "{source}"
      );
    }
  }

  #[test]
  fn absolute_cd_recovers_the_directory_after_an_unknown_cd() {
    let fixture = Fixture::new();
    let library = fixture.write("sub/library.sh", "found=1");
    let directory = library.parent().unwrap().to_str().unwrap();
    for (source, resolved) in [
      (
        format!(
          "read target; cd \"$target\"; cd '{directory}'; . ./library.sh"
        ),
        true,
      ),
      (
        format!(
          "trap \"cd '{directory}'; . ./library.sh\" EXIT; read target; cd \"$target\""
        ),
        true,
      ),
      (
        format!("trap \"cd '{directory}'; . ./library.sh\" EXIT; cd ./sub"),
        true,
      ),
      (
        format!(
          "trap \"cd '{directory}'; . ./library.sh\" EXIT; PWD=/elsewhere"
        ),
        false,
      ),
      (
        format!("read name; read \"$name\"; cd '{directory}'; . ./library.sh"),
        false,
      ),
      (
        format!("read name; read \"x$name\"; cd '{directory}'; . ./library.sh"),
        true,
      ),
      (
        format!(
          "f() {{ cd \"$1\"; f x; }}; f a; cd '{directory}'; . ./library.sh"
        ),
        true,
      ),
    ] {
      let main = fixture.write("main.sh", &source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      app.refresh(&AtomicBool::new(false)).unwrap();
      assert_eq!(app.document_id(&library).is_some(), resolved, "{source}");
    }
  }

  #[test]
  fn filesystem_queries_are_refreshed_and_dynamic_paths_remain_unresolved() {
    let fixture = Fixture::new();
    let main = fixture.write("main.sh", ". ./lib/*.sh\n");
    let a = fixture.write("lib/a.sh", "a=1\n");
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    let mut app = Application::new(settings).unwrap();
    app.add_input(&main);
    let cancel = AtomicBool::new(false);
    app.refresh(&cancel).unwrap();
    assert!(app.document_id(&a).is_some());
    std::fs::remove_file(&a).unwrap();
    let b = fixture.write("lib/b.sh", "b=1\n");
    app.refresh(&cancel).unwrap();
    assert!(app.document_id(&a).is_none());
    assert!(app.document_id(&b).is_some());
    app
      .open(
        &main,
        "if choice; then d=./lib; else d=./elsewhere; fi; cd \"$d\"; . ./b.sh",
        &cancel,
      )
      .unwrap();
    app.refresh(&cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    assert!(matches!(
      app.workspace().dependencies(id).next().unwrap().1,
      DependencyState::Unresolved(_)
    ));
  }

  #[cfg(unix)]
  #[test]
  fn logical_and_physical_cd_use_different_parents_of_a_symbolic_link() {
    let fixture = Fixture::new();
    let logical = fixture.write("library.sh", "logical=1\n");
    let physical = fixture.write("real/library.sh", "physical=1\n");
    std::fs::create_dir(fixture.root.join("real/sub")).unwrap();
    std::os::unix::fs::symlink("real/sub", fixture.root.join("link")).unwrap();
    for (option, expected) in [("-L", &logical), ("-P", &physical)] {
      let main = fixture
        .write("main.sh", &format!("cd {option} link/..; . ./library.sh"));
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      app.refresh(&AtomicBool::new(false)).unwrap();
      assert!(app.document_id(expected).is_some(), "{option}");
      assert_eq!(app.workspace().documents().count(), 2);
    }
  }
}

mod session {
  use crate::fixtures;

  use fixtures::Fixture;
  use shellsmith::interface::application::{
    Error, RequestScope, Session, Settings,
  };
  use std::sync::atomic::Ordering;
  use std::time::Duration;
  use tokio::sync::oneshot;

  #[tokio::test]
  async fn changed_inputs_cancel_workers_and_preserve_the_current_overlay() {
    let fixture = Fixture::new();
    let path = fixture.write("main.sh", "disk=1");
    let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
    session.set_overlay(&path, Some("old=1".into())).unwrap();
    let (started, start) = oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let old = tokio::spawn(session.request(
      RequestScope::Analysis(path.clone()),
      move |_, cancelled| {
        started.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(cancelled.load(Ordering::Relaxed));
        Ok("old result")
      },
    ));
    start.await.unwrap();
    session
      .set_overlay(&path, Some("current=2".into()))
      .unwrap();
    release.send(()).unwrap();
    assert!(matches!(old.await.unwrap(), Err(Error::Superseded)));
    let current = session
      .request(RequestScope::Workspace, move |application, _| {
        let id = application.document_id(&path).unwrap();
        Ok(
          application
            .workspace()
            .document(id)
            .unwrap()
            .source()
            .to_owned(),
        )
      })
      .await
      .unwrap();
    assert_eq!(current, "current=2");
  }

  #[tokio::test]
  async fn cancelling_one_request_leaves_another_request_running_and_discards_changes()
   {
    let fixture = Fixture::new();
    let mut settings = Settings::new(fixture.root.clone());
    settings.limits.value_bytes = 1024;
    let session = Session::new(settings.clone()).unwrap();
    let (started, start) = oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let (stopped, stop) = oneshot::channel();
    let cancelled = tokio::spawn(session.request(
      RequestScope::Workspace,
      move |application, flag| {
        settings.limits.value_bytes = 512;
        application.configure(settings)?;
        started.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(10)).unwrap();
        stopped.send(flag.load(Ordering::Relaxed)).unwrap();
        Ok(())
      },
    ));
    start.await.unwrap();
    let independent =
      session.request(RequestScope::Workspace, |application, flag| {
        assert!(!flag.load(Ordering::Relaxed));
        Ok(application.settings().limits.value_bytes)
      });
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    assert!(stop.await.unwrap());
    assert_eq!(independent.await.unwrap(), 1024);
    assert_eq!(
      session
        .request(RequestScope::Workspace, |application, _| Ok(
          application.settings().limits.value_bytes
        ))
        .await
        .unwrap(),
      1024
    );
  }

  #[tokio::test]
  async fn dropping_a_session_cancels_workers_that_outlive_it() {
    let fixture = Fixture::new();
    let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
    let (started, start) = oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let task =
      tokio::spawn(session.request(RequestScope::Workspace, move |_, flag| {
        started.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(flag.load(Ordering::Relaxed));
        Ok(())
      }));
    start.await.unwrap();
    drop(session);
    release.send(()).unwrap();
    assert!(matches!(task.await.unwrap(), Err(Error::Superseded)));
  }

  #[tokio::test]
  async fn concurrent_document_requests_share_inputs_without_superseding_each_other()
   {
    let fixture = Fixture::new();
    let first = fixture.write("first.sh", "first=1");
    let second = fixture.write("second.sh", "second=2");
    for opened in [false, true] {
      let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
      if opened {
        session.set_overlay(&first, Some("first=1".into())).unwrap();
        session
          .set_overlay(&second, Some("second=2".into()))
          .unwrap();
      } else {
        session.add_input(&fixture.root).unwrap();
      }
      let first = session
        .request(RequestScope::Analysis(first.clone()), |application, _| {
          Ok(application.workspace().documents().count())
        });
      let second = session
        .request(RequestScope::Analysis(second.clone()), |application, _| {
          Ok(application.workspace().documents().count())
        });
      let (first, second) = tokio::join!(first, second);
      assert_eq!(first.unwrap(), 1);
      assert_eq!(second.unwrap(), 1);
    }
  }

  #[tokio::test]
  async fn failed_requests_do_not_commit_mutations_or_poison_the_session() {
    let fixture = Fixture::new();
    let mut settings = Settings::new(fixture.root.clone());
    settings.limits.value_bytes = 1024;
    let session = Session::new(settings.clone()).unwrap();
    for panics in [false, true] {
      let mut settings = settings.clone();
      let result = session
        .request(RequestScope::Workspace, move |application, _| {
          settings.limits.value_bytes = 512;
          application.configure(settings)?;
          assert!(!panics, "test worker failure");
          Err::<(), _>(Error::Message("test operation failure".into()))
        })
        .await;
      assert!(matches!(result, Err(Error::Message(_))));
      let actual = session
        .request(RequestScope::Workspace, |application, _| {
          Ok(application.settings().limits.value_bytes)
        })
        .await
        .unwrap();
      assert_eq!(actual, 1024);
    }
  }

  #[tokio::test]
  async fn failed_shared_preparation_is_retried_after_a_file_is_created() {
    let fixture = Fixture::new();
    let path = fixture.root.join("missing.sh");
    let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
    let first =
      session.request(RequestScope::Analysis(path.clone()), |_, _| Ok(()));
    let second =
      session.request(RequestScope::Analysis(path.clone()), |_, _| Ok(()));
    let (first, second) = tokio::join!(first, second);
    for result in [first, second] {
      assert!(
        matches!(result, Err(Error::Message(message)) if message.contains("Cannot load"))
      );
    }
    fixture.write("missing.sh", "value=1\n");
    session
      .request(RequestScope::Analysis(path.clone()), |_, _| Ok(()))
      .await
      .unwrap();
  }

  #[tokio::test]
  async fn document_requests_do_not_wait_for_the_workspace_worker() {
    let fixture = Fixture::new();
    let main = fixture.write("main.sh", ". ./lib.sh\nf\n");
    fixture.write("lib.sh", "f() { :; }\n");
    fixture.write("unrelated.sh", ". ./missing.sh\n");
    std::fs::write(fixture.root.join("broken.sh"), [0xff]).unwrap();
    let settings = Settings::new(fixture.root.clone());
    let session = Session::new(settings).unwrap();
    session.add_input(&fixture.root).unwrap();
    session.start_index().unwrap();
    let (started, start) = oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let index =
      tokio::spawn(session.request(RequestScope::Workspace, move |app, _| {
        assert!(app.workspace().analysis().is_none());
        assert_eq!(app.workspace().documents().count(), 3);
        assert_eq!(app.failures().len(), 1);
        started.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(10)).unwrap();
        Ok(())
      }));
    start.await.unwrap();
    let path = main.clone();
    let document =
      session.request(RequestScope::Analysis(main), move |app, cancelled| {
        let id = app.document_id(&path).unwrap();
        assert_eq!(app.workspace().documents().count(), 2);
        assert_eq!(app.failures(), []);
        assert_ne!(app.classify(id, cancelled)?, []);
        assert_eq!(app.document_links(id, cancelled)?.len(), 1);
        Ok(())
      });
    let result = tokio::time::timeout(Duration::from_secs(5), document).await;
    release.send(()).unwrap();
    result
      .expect("document must finish while the index worker is occupied")
      .unwrap();
    index.await.unwrap().unwrap();
  }
}

mod sources {
  use crate::fixtures;
  use fixtures::Fixture;
  use shellsmith::interface::application::engine::{
    DependencyState, InitialEnvironment, OccurrenceKind, ResolutionIssue, Value,
  };
  use shellsmith::interface::application::{Application, Settings};
  use std::sync::atomic::AtomicBool;

  fn application(fixture: &Fixture) -> Application {
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    Application::new(settings).unwrap()
  }

  #[test]
  fn source_links_require_a_terminated_heredoc_and_recover_after_repair() {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", ":\n");
    let main = fixture.root.join("main.sh");
    let cancel = AtomicBool::new(false);
    for (opening, delimiter, body, closing) in [
      ("", "END", "./lib.sh\nEND", ""),
      ("", "'END'", "./lib.sh\nEND", ""),
      ("", "-END", "\t./lib.sh\n\tEND", ""),
      ("", "''", "./lib.sh\n", ""),
      ("ignored=$(", "END", "./lib.sh\nEND", ")\n"),
      ("ignored=`", "END", "./lib.sh\nEND", "`\n"),
    ] {
      let prefix = format!(
        "{opening}{{ read -r path; . \"$path\"; }} <<{delimiter}\n{body}"
      );
      let source = format!("{prefix}{closing}");
      let mut app = application(&fixture);
      app.open(&main, &source, &cancel).unwrap();
      let id = app.document_id(&main).unwrap();
      assert!(app.workspace().document(id).unwrap().has_syntax_errors());
      assert_eq!(app.document_links(id, &cancel).unwrap(), [], "{source}");

      let repaired = format!("{prefix}\n{closing}");
      app.change(&main, &repaired, &cancel).unwrap();
      assert!(!app.workspace().document(id).unwrap().has_syntax_errors());
      let links = app.document_links(id, &cancel).unwrap();
      assert_eq!(links.len(), 1, "{repaired}");
      assert_eq!(links[0].1, library);
      assert_eq!(&repaired[links[0].0.bytes.clone()], "\"$path\"");
    }
  }

  #[test]
  fn source_annotations_share_links_definitions_tokens_and_rename_without_inventing_values()
   {
    use shellsmith::interface::application::engine::{Modifier, SemanticKind};
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "readonly value=ok\nf() { :; }\n");
    let source = "read path\n# shellsmith source=\"./lib.sh\"\n. \"$path\"\nf\n: \"$value\" \"$path\"\n";
    let main = fixture.write("main.sh", source);
    let cancel = AtomicBool::new(false);
    let mut app = application(&fixture);
    app.add_input(&main);
    app.refresh(&cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    let links = app.document_links(id, &cancel).unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].1, library);
    assert_eq!(&source[links[0].0.bytes.clone()], "\"$path\"");
    let call = source.find("\nf\n").unwrap() + 1;
    let definitions = app.definitions(id, call, &cancel).unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(
      app.path(definitions[0].snapshot.document),
      Some(library.as_path())
    );
    assert_eq!(definitions[0].bytes, 18..19);
    let spans = app.classify(id, &cancel).unwrap();
    assert!(
      spans
        .iter()
        .any(|span| span.location.bytes == (call..call + 1)
          && span.kind == SemanticKind::Function)
    );
    let value = source.rfind("$value").unwrap() + 1;
    assert!(
      spans
        .iter()
        .any(|span| span.location.bytes == (value..value + 5)
          && span.modifiers.contains(&Modifier::Readonly))
    );
    let analysis = app.workspace().analysis().unwrap();
    for site in app
      .workspace()
      .document(id)
      .unwrap()
      .occurrences()
      .iter()
      .filter(|site| {
        site.kind == OccurrenceKind::VariableExpansion
          && site.name == Value::Literal(b"path".to_vec())
      })
    {
      assert!(
        analysis
          .values(site.id)
          .all(|(_, value)| matches!(value, Value::Unknown(_)))
      );
    }
    let edits = app.rename(&main, call, "renamed", &cancel).unwrap();
    assert_eq!(edits.len(), 2);
    let mut edited_main = source.to_owned();
    let mut edited_library = "readonly value=ok\nf() { :; }\n".to_owned();
    for edit in edits.iter().rev() {
      let text =
        if app.path(edit.location.snapshot.document) == Some(main.as_path()) {
          &mut edited_main
        } else {
          &mut edited_library
        };
      text.replace_range(edit.location.bytes.clone(), &edit.new_text);
    }
    assert_eq!(edited_main, source.replace("\nf\n", "\nrenamed\n"));
    assert_eq!(edited_library, "readonly value=ok\nrenamed() { :; }\n");
  }

  #[test]
  fn source_annotations_prefer_known_paths_and_do_not_hide_invalid_calls_or_failures()
   {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", ":\n");
    let automatic = fixture.write("auto.sh", ":\n");
    for (body, expected) in [
      (". ./auto.sh", Some(&automatic)),
      (". ./missing.sh", None),
      (". ''", None),
      (".", None),
      (". --", None),
      (". -- ''", None),
      (". $path", None),
      (". \"$@\"", None),
      (". \"$path\" >", None),
      ("printf '%s' \"$path\"", None),
      ("\"$command\" \"$path\"", None),
      (". \"$path\"", Some(&library)),
      (". -- \"$path\"", Some(&library)),
      (". \"./$path\"", Some(&library)),
      (". \"${path:-./auto.sh}\"", Some(&library)),
      (". \"$(dirname \"$0\")/lib.sh\"", Some(&library)),
      (". \"$path\"; . \"$path\"", Some(&library)),
      ("command . \"$path\"", Some(&library)),
      (
        "dot=.;\n# shellsmith source=./lib.sh\n\"$dot\" \"$path\"",
        Some(&library),
      ),
    ] {
      let source =
        format!("read path command\n# shellsmith source=./lib.sh\n{body}\n");
      let main = fixture.write("main.sh", &source);
      let mut app = application(&fixture);
      let cancel = AtomicBool::new(false);
      app.open(&main, &source, &cancel).unwrap();
      let id = app.document_id(&main).unwrap();
      let links = app.document_links(id, &cancel).unwrap();
      assert_eq!(
        links.iter().map(|(_, path)| path).collect::<Vec<_>>(),
        expected.into_iter().collect::<Vec<_>>(),
        "{source}"
      );
    }
  }

  #[test]
  fn source_annotations_use_literal_paths_relative_to_their_document() {
    let fixture = Fixture::new();
    let local = fixture.write("nested/lib space.sh", ":\n");
    let literal = fixture.write("nested/$HOME\\file.sh", ":\n");
    fixture.write("elsewhere/placeholder", "");
    for (annotation, expected) in [
      ("\"lib space.sh\"".to_owned(), &local),
      ("'./lib space.sh'".to_owned(), &local),
      (format!("\"{}\"", local.display()), &local),
      ("\"$HOME\\file.sh\"".to_owned(), &literal),
    ] {
      let source = format!(
        "read path\ncd ../elsewhere\n  # shellsmith source={annotation}\n  . \"$path\"\n"
      );
      let main = fixture.write("nested/main.sh", &source);
      let mut app = application(&fixture);
      let cancel = AtomicBool::new(false);
      app.open(&main, &source, &cancel).unwrap();
      let id = app.document_id(&main).unwrap();
      assert_eq!(
        app
          .document_links(id, &cancel)
          .unwrap()
          .iter()
          .map(|(_, path)| path)
          .collect::<Vec<_>>(),
        [&expected.clone()],
        "{annotation}"
      );
    }
  }

  #[test]
  fn source_annotations_resolve_a_leading_tilde_with_the_initial_home() {
    let fixture = Fixture::new();
    let home = fixture.write("home/lib.sh", ":\n");
    let literal = fixture.write("nested/~/lib.sh", ":\n");
    let user = fixture.write("nested/~user/lib.sh", ":\n");
    let home_directory = fixture.root.join("home");
    let home_bytes = home_directory.as_os_str().as_encoded_bytes().to_vec();
    for (annotation, assignment, initial_home, expected) in [
      ("~/lib.sh", "", Some(home_bytes.clone()), vec![&home]),
      ("\"~/lib.sh\"", "", Some(home_bytes.clone()), vec![&home]),
      (
        "~/lib.sh",
        "HOME=/elsewhere\n",
        Some(home_bytes.clone()),
        vec![&home],
      ),
      ("~user/lib.sh", "", Some(home_bytes.clone()), vec![&user]),
      ("./~/lib.sh", "", Some(home_bytes), vec![&literal]),
      ("~/lib.sh", "", None, vec![]),
      ("~/lib.sh", "", Some(Vec::new()), vec![]),
      ("~/lib.sh", "", Some(b"home".to_vec()), vec![]),
    ] {
      let source = format!(
        "read path\n{assignment}# shellsmith source={annotation}\n. \"$path\"\n"
      );
      let main = fixture.write("nested/main.sh", &source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      if let Some(value) = initial_home.clone() {
        settings.initial.environment.insert(b"HOME".to_vec(), value);
      }
      let mut app = Application::new(settings).unwrap();
      let cancel = AtomicBool::new(false);
      app.open(&main, &source, &cancel).unwrap();
      let id = app.document_id(&main).unwrap();
      assert_eq!(
        app
          .document_links(id, &cancel)
          .unwrap()
          .iter()
          .map(|(_, path)| path)
          .collect::<Vec<_>>(),
        expected,
        "{annotation} {:?}",
        initial_home.as_deref().map(String::from_utf8_lossy)
      );
    }
  }

  #[test]
  fn source_annotations_at_the_start_of_a_document_preserve_bom_and_ranges() {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "f() { :; }\n");
    let cancel = AtomicBool::new(false);
    for prefix in ["", "\u{feff}", "\u{feff}\t"] {
      let source = format!(
        "{prefix}# shellsmith source=./lib.sh\n. \"$(dirname \"$0\")/lib.sh\"\nf\n"
      );
      let main = fixture.write("main.sh", &source);
      let mut app = application(&fixture);
      app.open(&main, &source, &cancel).unwrap();
      let id = app.document_id(&main).unwrap();
      let links = app.document_links(id, &cancel).unwrap();
      assert_eq!(links.len(), 1, "{source:?}");
      assert_eq!(links[0].1, library);
      assert_eq!(
        &source[links[0].0.bytes.clone()],
        "\"$(dirname \"$0\")/lib.sh\""
      );
      assert_eq!(
        app
          .definitions(id, source.len() - 2, &cancel)
          .unwrap()
          .len(),
        1
      );
    }
  }

  #[test]
  fn source_annotations_require_a_valid_adjacent_comment_and_preserve_missing_files()
   {
    let fixture = Fixture::new();
    fixture.write("lib.sh", ":\n");
    for prefix in [
      "# shellsmith source=./lib.sh\n\n",
      ": # shellsmith source=./lib.sh\n",
      "# shellsmith source=./lib.sh\n# other\n",
      "# shellsmith source=\n",
      "# shellsmith source=''\n",
      "# shellsmith source='\n",
      "# shellsmith source=\"\n",
      "# shellsmith source=\"./lib.sh\n",
      "# shellsmith source=./lib.sh extra\n",
      "# shellsmith source='lib'\".sh\"\n",
      "# shellsmith source=./missing.sh\n",
      "# shellsmith source=.\n",
      "# shellsmith source=./lib.sh\0\n",
      "cat <<'# shellsmith source=./lib.sh'\n# shellsmith source=./lib.sh\n",
      ": '\n# shellsmith source=./lib.sh\n. \"$path\"\n'\n",
    ] {
      let source = format!("read path\n{prefix}. \"$path\"\n");
      let main = fixture.write("main.sh", &source);
      let mut app = application(&fixture);
      let cancel = AtomicBool::new(false);
      app.open(&main, &source, &cancel).unwrap();
      let id = app.document_id(&main).unwrap();
      assert!(
        app.document_links(id, &cancel).unwrap().is_empty(),
        "{source}"
      );
    }
  }

  #[test]
  fn source_annotations_keep_automatic_choices_across_contexts_and_unknown_search_conditions()
   {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", ":\n");
    let automatic = fixture.write("auto.sh", ":\n");
    for (source, expected) in [
      (
        "# shellsmith source=./lib.sh\n. unavailable_in_PATH\n",
        vec![&library],
      ),
      (
        "PATH=.;\n# shellsmith source=./lib.sh\n. auto.sh\n",
        vec![&automatic],
      ),
      (
        "PATH=.;\n# shellsmith source=./lib.sh\n. absent.sh\n",
        vec![],
      ),
      (
        "PATH=.;\n# shellsmith source=./lib.sh\nf() { . \"$path\"; }\nread path\nf\n",
        vec![],
      ),
      (
        "read path\nf() {\n# shellsmith source=./lib.sh\n. \"$1\"\n}\nf ./lib.sh\nf \"$path\"\n",
        vec![&library],
      ),
      (
        "read path\nf() {\n# shellsmith source=./lib.sh\n. \"$1\"\n}\nf ./auto.sh\nf \"$path\"\n",
        vec![],
      ),
      (
        "read target\nif :; then path=./lib.sh; else path=./auto.sh; fi\n# shellsmith source=./lib.sh\n. \"$path\"\n",
        vec![],
      ),
    ] {
      let main = fixture.write("main.sh", source);
      let mut app = application(&fixture);
      let cancel = AtomicBool::new(false);
      app.open(&main, source, &cancel).unwrap();
      let id = app.document_id(&main).unwrap();
      let links = app.document_links(id, &cancel).unwrap();
      assert_eq!(
        links.iter().map(|(_, path)| path).collect::<Vec<_>>(),
        expected,
        "{source}"
      );
    }
  }

  #[test]
  fn source_annotations_preserve_loading_failures_and_accept_unsaved_targets() {
    let fixture = Fixture::new();
    let fallback = fixture.write("fallback.sh", "fallback() { :; }\n");
    let broken = fixture.write("broken.sh", "");
    std::fs::write(&broken, [0xff]).unwrap();
    let cancel = AtomicBool::new(false);
    for operand in ["./broken.sh", "\"$path\""] {
      let annotation = if operand == "./broken.sh" {
        "./fallback.sh"
      } else {
        "./broken.sh"
      };
      let source =
        format!("read path\n# shellsmith source={annotation}\n. {operand}\n");
      let main = fixture.write("main.sh", &source);
      let mut app = application(&fixture);
      app.open(&main, &source, &cancel).unwrap();
      let id = app.document_id(&main).unwrap();
      assert_eq!(app.document_links(id, &cancel).unwrap(), []);
      assert_eq!(app.failures().len(), 1);
      assert_eq!(app.failures()[0].path, broken);
      assert!(app.document_id(&fallback).is_none());
    }
    let source = "read path\n# shellsmith source=./new.sh\n. \"$path\"\nf\n";
    let main = fixture.write("main.sh", source);
    let target = fixture.root.join("new.sh");
    let mut app = application(&fixture);
    app.open(&main, source, &cancel).unwrap();
    app.open(&target, "f() { :; }\n", &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    let links = app.document_links(id, &cancel).unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].1, target);
    assert_eq!(
      app
        .definitions(id, source.len() - 2, &cancel)
        .unwrap()
        .len(),
      1
    );
    assert!(app.close(&target, &cancel).is_err());
    let id = app.document_id(&main).unwrap();
    assert_eq!(app.document_links(id, &cancel).unwrap(), []);
  }

  #[test]
  fn source_annotations_preserve_shared_environment_and_validate_renames() {
    use shellsmith::interface::application::Error;
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "other=kept\ncd ./nested\n");
    let next = fixture.write("nested/next.sh", "f() { :; }\n");
    let source =
      "read path\n# shellsmith source=./lib.sh\n. \"$path\"\n. ./next.sh\nf\n";
    let main = fixture.write("main.sh", source);
    let mut app = application(&fixture);
    let cancel = AtomicBool::new(false);
    app.open(&main, source, &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    assert_eq!(
      app
        .document_links(id, &cancel)
        .unwrap()
        .into_iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>(),
      [library, next]
    );
    let edits = app.rename(&main, 5, "source_path", &cancel).unwrap();
    assert_eq!(edits.len(), 2);
    let mut changed = source.to_owned();
    for edit in edits.iter().rev() {
      changed.replace_range(edit.location.bytes.clone(), &edit.new_text);
    }
    assert_eq!(changed, source.replace("path", "source_path"));
    assert!(matches!(
      app.rename(&main, 5, "other", &cancel),
      Err(Error::RenameRefused(_))
    ));
  }

  #[test]
  fn source_annotations_follow_nested_and_deferred_bodies_and_refresh_with_overlays()
   {
    let fixture = Fixture::new();
    let source =
      "read path\n# shellsmith source=./nested/first.sh\n. \"$path\"\n";
    let main = fixture.write("main.sh", source);
    let first_source =
      "read path\n# shellsmith source=./second.sh\n. \"$path\"\n";
    let first = fixture.write("nested/first.sh", first_source);
    let second = fixture.write("nested/second.sh", "f() { :; }\n");
    let third = fixture.write("nested/third.sh", "g() { :; }\n");
    let cancel = AtomicBool::new(false);
    let mut app = application(&fixture);
    app.add_input(&main);
    app.refresh(&cancel).unwrap();
    let id = app.document_id(&first).unwrap();
    assert_eq!(app.workspace().symbols_named(b"f").count(), 1);
    assert_eq!(
      app
        .document_links(id, &cancel)
        .unwrap()
        .iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>(),
      [&second]
    );
    app
      .open(
        &first,
        "read path\n# shellsmith source=./third.sh\n. \"$path\"\n",
        &cancel,
      )
      .unwrap();
    app.refresh(&cancel).unwrap();
    assert_eq!(app.workspace().symbols_named(b"f").count(), 0);
    assert_eq!(app.workspace().symbols_named(b"g").count(), 1);
    let id = app.document_id(&first).unwrap();
    assert_eq!(
      app
        .document_links(id, &cancel)
        .unwrap()
        .iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>(),
      [&third]
    );
    for source in [
      "read path\nf() {\n# shellsmith source=./nested/second.sh\n. \"$path\"\n}\nf\n",
      "f() {\n# shellsmith source=./nested/second.sh\n. \"$path\"\n}\n",
      "read path\ntrap '# shellsmith source=./nested/second.sh\n. \"$path\"' EXIT\n",
    ] {
      let mut app = application(&fixture);
      app.open(&main, source, &cancel).unwrap();
      let id = app.document_id(&main).unwrap();
      assert_eq!(
        app
          .document_links(id, &cancel)
          .unwrap()
          .iter()
          .map(|(_, path)| path)
          .collect::<Vec<_>>(),
        [&second],
        "{source}"
      );
    }
  }

  #[test]
  fn source_discovery_continues_after_termination_in_entries_and_sourced_files()
  {
    let fixture = Fixture::new();
    let first =
      fixture.write("first.sh", "set -u; exit; next=./second.sh; f() { :; }\n");
    let second = fixture.write("second.sh", "exec replacement; g() { :; }\n");
    for source in [
      "lib=./first.sh; set -eu; false; : \"$missing\"; exit; . \"$lib\"; . \"$next\"; f; g",
      "lib=./first.sh; set -o errexit -o nounset; exec replacement; . \"$lib\"; . \"$next\"; f; g",
    ] {
      let main = fixture.write("main.sh", source);
      let mut app = application(&fixture);
      app.add_input(&main);
      let cancelled = AtomicBool::new(false);
      app.refresh(&cancelled).unwrap();
      assert!(app.document_id(&first).is_some(), "{source}");
      assert!(app.document_id(&second).is_some(), "{source}");
      let id = app.document_id(&main).unwrap();
      assert!(
        app
          .workspace()
          .dependencies(id)
          .all(|(_, state)| { matches!(state, DependencyState::Resolved(_)) }),
        "{source}"
      );
      let spans = app.classify(id, &cancelled).unwrap();
      assert_eq!(
        spans
          .iter()
          .filter(|span| {
            span.kind
              == shellsmith::interface::application::engine::SemanticKind::Function
          })
          .count(),
        2,
        "{source}"
      );
    }
  }

  #[test]
  fn assigned_source_paths_load_nested_files_and_recompute_after_changes() {
    let fixture = Fixture::new();
    let main = fixture.write(
      "main.sh",
      "base=./lib; file=$base/first.sh; . \"$file\"; . \"$next\"; f\n",
    );
    let first = fixture.write("lib/first.sh", "next=./lib/second.sh\n");
    let second = fixture.write("lib/second.sh", "f() { :; }\n");
    let third = fixture.write("lib/third.sh", "other() { :; }\n");
    let mut app = application(&fixture);
    app.add_input(&main);
    let cancelled = AtomicBool::new(false);
    app.refresh(&cancelled).unwrap();
    assert!(app.document_id(&second).is_some());
    assert!(app.document_id(&third).is_none());
    let id = app.document_id(&main).unwrap();
    assert!(app.classify(id, &cancelled).unwrap().iter().any(|span| {
      span.kind
        == shellsmith::interface::application::engine::SemanticKind::Function
    }));
    app
      .open(&first, "next=./lib/third.sh\n", &cancelled)
      .unwrap();
    app.refresh(&cancelled).unwrap();
    assert!(app.document_id(&second).is_none());
    assert!(app.document_id(&third).is_some());
  }

  #[test]
  fn assigned_source_operands_use_pre_prefix_values_and_merge_call_contexts() {
    let fixture = Fixture::new();
    let first = fixture.write("first.sh", "first=1\n");
    let second = fixture.write("second.sh", "second=1\n");
    for (source, expected) in [
      (
        "lib=./first.sh; lib=./second.sh . \"$lib\"; . \"$lib\"",
        vec![first.clone(), second.clone()],
      ),
      (
        "if condition; then lib=./first.sh; else lib=./first.sh; fi; . \"$lib\"",
        vec![first.clone()],
      ),
      (
        "if condition; then lib=a/first.sh; else lib=b/first.sh; fi; . \"./${lib#*/}\"",
        vec![first.clone()],
      ),
      (
        "if condition; then base=fi; tail=rst.sh; else base=f; tail=irst.sh; fi; copied=$base; . \"./$copied$tail\"",
        vec![first.clone()],
      ),
      (
        "if condition; then lib=./first.sh; else lib=./second.sh; fi; . \"$lib\"",
        vec![],
      ),
      (
        "f() { . \"$lib\"; }; lib=./first.sh; f; lib=./second.sh; f",
        vec![],
      ),
      ("lib=./first.sh; . $lib", vec![first.clone()]),
      (
        "for lib in ./first.sh; do . \"$lib\"; done",
        vec![first.clone()],
      ),
      (
        "for lib in ./first.sh ./second.sh; do . \"$lib\"; done",
        vec![],
      ),
    ] {
      let main = fixture.write("main.sh", source);
      let mut app = application(&fixture);
      app.add_input(&main);
      app.refresh(&AtomicBool::new(false)).unwrap();
      let id = app.document_id(&main).unwrap();
      let actual: Vec<_> = app
        .workspace()
        .dependencies(id)
        .filter_map(|(_, state)| {
          if let DependencyState::Resolved(snapshot) = state {
            app
              .path(snapshot.document)
              .map(std::path::Path::to_path_buf)
          } else {
            None
          }
        })
        .collect();
      assert_eq!(actual, expected, "{source}");
    }
  }

  #[test]
  fn copied_environment_values_survive_overwrites_without_environment_modifiers()
   {
    let fixture = Fixture::new();
    let main = fixture.write(
      "main.sh",
      "base=$HOME; HOME=./elsewhere; . \"$base/lib.sh\"; : \"$HOME\"\n",
    );
    let library = fixture.write("lib.sh", "f() { :; }\n");
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    settings.initial.environment.insert(
      b"HOME".to_vec(),
      fixture.root.as_os_str().as_encoded_bytes().to_vec(),
    );
    let mut app = Application::new(settings).unwrap();
    app.add_input(&main);
    let cancelled = AtomicBool::new(false);
    app.refresh(&cancelled).unwrap();
    assert!(app.document_id(&library).is_some());
    let id = app.document_id(&main).unwrap();
    let spans = app.classify(id, &cancelled).unwrap();
    let marked: Vec<_> = spans
      .iter()
      .filter(|span| {
        span.modifiers.contains(
          &shellsmith::interface::application::engine::Modifier::Environment,
        )
      })
      .map(|span| span.location.bytes.clone())
      .collect();
    assert_eq!(marked, vec![6..10]);
  }

  #[test]
  fn limited_or_unsupported_assignment_values_never_become_resolved_sources() {
    let fixture = Fixture::new();
    fixture.write("lib.sh", "found=1\n");
    for (source, limit, expected) in [
      ("lib=./lib.sh; . \"$lib\"", 7, 0),
      ("lib=./lib.sh; . \"$lib\"", 8, 1),
      ("lib=$(printf ./lib.sh); . \"$lib\"", 8, 0),
      ("lib=./lib.sh; eval change; . \"$lib\"", 8, 1),
    ] {
      let main = fixture.write("main.sh", source);
      let mut app = application(&fixture);
      let mut settings = app.settings().clone();
      settings.limits.value_bytes = limit;
      app.configure(settings).unwrap();
      app.add_input(&main);
      let cancelled = AtomicBool::new(false);
      app.refresh(&cancelled).unwrap();
      assert_eq!(
        app.workspace().symbols_named(b"found").count(),
        expected,
        "{source} with {limit}"
      );
      if limit == 7 {
        let id = app.document_id(&main).unwrap();
        app.classify(id, &cancelled).unwrap();
        assert!(
          app
            .workspace()
            .analysis()
            .unwrap()
            .gaps()
            .iter()
            .any(|gap| gap.issue == ResolutionIssue::ValueLimit)
        );
      }
    }
  }

  #[test]
  fn source_invocations_use_expanded_names_operands_and_search_conditions() {
    let fixture = fixtures::Fixture::new();
    fixture.write("lib.sh", "loaded=yes\n");
    fixture.write("other/lib.sh", "loaded=other\n");
    for (source, expected) in [
      ("op=.; \"$op\" ./lib.sh; : \"$loaded\"", "yes"),
      ("command . ./lib.sh; : \"$loaded\"", "yes"),
      (". -- ./lib.sh; : \"$loaded\"", "yes"),
      ("p=./lib.sh; . $p; : \"$loaded\"", "yes"),
      (". \"${p:-./lib.sh}\"; : \"$loaded\"", "yes"),
      ("PATH=./other; . lib.sh; : \"$loaded\"", "other"),
      (
        "unused() { cd /unknown; }; . ./lib.sh; : \"$loaded\"",
        "yes",
      ),
      ("(cd /unknown); . ./lib.sh; : \"$loaded\"", "yes"),
    ] {
      let main = fixture.write("main.sh", source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      app.refresh(&AtomicBool::new(false)).unwrap();
      let id = app.document_id(&main).unwrap();
      app.classify(id, &AtomicBool::new(false)).unwrap();
      let analysis = app.workspace().analysis().unwrap();
      let read = app
        .workspace()
        .document(id)
        .unwrap()
        .occurrences()
        .iter()
        .rev()
        .find(|site| {
          site.kind == OccurrenceKind::VariableExpansion
            && site.name == Value::Literal(b"loaded".to_vec())
        })
        .unwrap();
      assert_eq!(
        analysis
          .values(read.id)
          .map(|(_, value)| value.clone())
          .collect::<Vec<_>>(),
        vec![Value::Literal(expected.as_bytes().to_vec())],
        "{source}"
      );
      assert!(
        app.workspace().symbols_named(b"loaded").any(|symbol| app
          .path(symbol.id.snapshot.document)
          .is_some_and(|path| path.ends_with(if expected == "other" {
            "other/lib.sh"
          } else {
            "lib.sh"
          }))),
        "{source}"
      );
      let references: Vec<_> = app
        .workspace()
        .dependencies(id)
        .map(|(_, state)| state.clone())
        .collect();
      assert!(
        references.iter().any(|state| matches!(
          state,
          shellsmith::interface::application::engine::DependencyState::Resolved(
            _
          )
        )),
        "{source}: {references:?}"
      );
    }
  }

  #[test]
  fn search_conditions_follow_execution_order_and_join_only_needed_inputs() {
    use shellsmith::interface::application::engine::DependencyState;
    let fixture = fixtures::Fixture::new();
    let first = fixture.write("first/lib.sh", "loaded=first\n");
    let second = fixture.write("second/lib.sh", "loaded=second\n");
    let current = fixture.write("lib.sh", "loaded=current\n");
    fixture.write("percent%/lib.sh", "loaded=percent\n");
    for (source, expected) in [
      (
        "PATH=./first; . lib.sh; PATH=./second; . lib.sh",
        vec![first.clone(), second.clone()],
      ),
      ("PATH=:./first; . lib.sh", vec![current.clone()]),
      ("PATH=; . lib.sh", vec![]),
      ("PATH=./percent%; . lib.sh", vec![]),
      (
        "PATH=./first; PATH=./second x=$(. lib.sh) command :",
        vec![],
      ),
      ("PATH=./first; PATH=./second . lib.sh", vec![second.clone()]),
      (
        "PATH=./first; name=lib.sh; PATH=./second . \"$name\"",
        vec![second.clone()],
      ),
      (
        "PATH=./first; (PATH=./second); . lib.sh",
        vec![first.clone()],
      ),
      (
        "f() { PATH=./second; }; PATH=./first; f; . lib.sh",
        vec![second.clone()],
      ),
      (
        "if condition; then PATH=./first; else PATH=./second; fi; . lib.sh",
        vec![],
      ),
      (
        "if condition; then PATH=./first; else PATH=./first; fi; . lib.sh",
        vec![first.clone()],
      ),
      (". ./missing; . ./first/lib.sh", vec![]),
      ("command command -- . ./first/lib.sh", vec![first.clone()]),
    ] {
      let main = fixture.write("main.sh", source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      app.refresh(&AtomicBool::new(false)).unwrap();
      let id = app.document_id(&main).unwrap();
      let actual: Vec<_> = app
        .workspace()
        .dependencies(id)
        .filter_map(|(_, state)| match state {
          DependencyState::Resolved(snapshot) => {
            Some(app.path(snapshot.document).unwrap().to_path_buf())
          }
          _ => None,
        })
        .collect();
      assert_eq!(actual, expected, "{source}");
    }
  }

  #[test]
  fn source_following_shares_trap_links_without_changing_the_callers_dependencies()
   {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "target=./first.sh\n");
    let first = fixture.write("first.sh", ":\n");
    let second = fixture.write("second.sh", ":\n");
    let hidden = fixture.write("hidden.sh", "hidden() { :; }\n");
    let source = ". ./lib.sh\ntrap 'target=./hidden.sh; . ./hidden.sh' INT EXIT\n. \"$target\"\n";
    let main = fixture.write("main.sh", source);
    let mut application =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    application.add_input(&main);
    let cancelled = AtomicBool::new(false);
    for expected in [first, second] {
      application.refresh(&cancelled).unwrap();
      let id = application.document_id(&main).unwrap();
      let links = application.document_links(id, &cancelled).unwrap();
      assert_eq!(
        links.iter().map(|(_, target)| target).collect::<Vec<_>>(),
        vec![&library, &hidden, &expected]
      );
      assert!(application.document_id(&hidden).is_some());
      assert_eq!(application.workspace().documents().count(), 5);
      assert_eq!(application.workspace().symbols_named(b"hidden").count(), 1);
      fixture.write("lib.sh", "target=./second.sh\n");
    }
  }
}

mod definitions {
  use crate::fixtures::Fixture;
  use shellsmith::interface::application::engine::InitialEnvironment;
  use shellsmith::interface::application::{Application, Settings};
  use std::sync::atomic::AtomicBool;

  #[test]
  fn trap_definitions_use_only_bindings_invariant_after_registration() {
    assert_definitions(&[
      ("f() { :; }; trap 'f' EXIT", "f'", &["f()"]),
      ("f() { :; }; trap 'f' INT; : done", "f'", &["f()"]),
      (
        "f() { :; }; f() { :; }; trap 'f' EXIT",
        "f'",
        &["f() { :; }; trap"],
      ),
      ("trap 'f' EXIT; f() { :; }", "f'", &[]),
      ("f() { :; }; trap 'f' EXIT; f() { :; }", "f'", &[]),
      ("f() { :; }; trap 'f' EXIT; unset -f f", "f'", &[]),
      (
        "f() { :; }; trap 'f' EXIT; change() { unset -f f; }; change",
        "f'",
        &[],
      ),
      (
        "f() { :; }; trap 'f' EXIT; read target; unset -f \"$target\"",
        "f'",
        &[],
      ),
      ("f() { :; }; trap 'f' EXIT; (unset -f f)", "f'", &["f()"]),
      (
        "f() { :; }; trap 'f' EXIT; idle() { unset -f f; }",
        "f'",
        &["f()"],
      ),
      ("f() { :; }; trap 'f' EXIT; unset -f other", "f'", &["f()"]),
      ("f() { :; }; trap 'f' EXIT; . ./missing.sh", "f'", &[]),
      (
        "f() { :; }; trap 'f' EXIT; if condition; then unset -f f; fi",
        "f'",
        &[],
      ),
      (
        "f() { :; }; while condition; do trap 'f' EXIT; unset -f f; done",
        "f'",
        &[],
      ),
      (
        "f() { :; }; trap 'trap '\\''f'\\'' TERM' EXIT",
        "f'",
        &["f()"],
      ),
      ("x=1; trap ': \"$x\"' EXIT", "x\"", &["x=1"]),
      ("x=1; trap ': \"$x\"' EXIT; x=2; x=1", "x\"", &[]),
      ("x=1; trap ': \"$x\"' EXIT; x=2 /bin/utility", "x\"", &[]),
      ("x=1; trap ': \"$x\"' EXIT; (x=2)", "x\"", &["x=1"]),
      ("readonly x=1; trap ': \"$x\"' EXIT; x=2", "x\"", &["x=1"]),
      ("x=1; f() { : \"$x\"; }; trap 'f' EXIT", "x\"", &["x=1"]),
      ("x=1; f() { : \"$x\"; }; trap 'f' EXIT; x=2", "x\"", &[]),
      ("f() { :; }; idle() { trap 'f' EXIT; }", "f'", &[]),
      (
        "if condition; then f() { :; }; else f() { : :; }; fi; trap 'f' EXIT",
        "f'",
        &["f() { :; }", "f() { : :; }"],
      ),
      ("f() { :; }; trap 'unset -f f; f' EXIT", "f'", &[]),
      ("f() { :; }; trap 'f; unset -f f' EXIT", "f;", &["f()"]),
      (
        "x=1; trap ': \"$x\"' EXIT; : \"$((x=2))$((x=1))\"",
        "x\"",
        &[],
      ),
      ("x=1; trap ': \"$x\"' EXIT; x=2 & wait", "x\"", &["x=1"]),
      ("x=1; trap ': \"$x\"' EXIT; : \"$(x=2)\"", "x\"", &["x=1"]),
      ("x=1; trap ': \"$x\"' EXIT; x=2 | :", "x\"", &[]),
      (
        "x=1; register() { trap ': \"$x\"' EXIT; }; x=2 register",
        "x\"",
        &[],
      ),
      (
        "x=1; trap ': \"$x\"' EXIT; read target; unset -v \"other_$target\"",
        "x\"",
        &["x=1"],
      ),
      (
        "x=1; register() { trap ': \"$x\"' EXIT; }; register; x=2; register",
        "x\"",
        &[],
      ),
    ]);
  }

  #[test]
  fn trap_callers_share_values_tokens_links_and_update_invalidation() {
    use shellsmith::interface::application::engine::{Modifier, SemanticKind};
    let fixture = Fixture::new();
    let library = fixture.write("library.sh", "from_library=1\n");
    let source = "readonly path=./library.sh; cleanup() { . \"${base:-.}/$path\"; }; trap 'cleanup' EXIT; read suffix; read \"other_$suffix\"; unset -f \"other_$suffix\"; PWD=/elsewhere";
    let file = fixture.write("main.sh", source);
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    let mut app = Application::new(settings).unwrap();
    let cancel = AtomicBool::new(false);
    app.open(&file, source, &cancel).unwrap();
    let id = app.document_id(&file).unwrap();
    let call = source.rfind("cleanup").unwrap();
    let read = source.find("$path").unwrap() + 1;
    assert_eq!(app.definitions(id, call, &cancel).unwrap()[0].bytes, 28..35);
    assert_eq!(app.definitions(id, read, &cancel).unwrap()[0].bytes, 9..13);
    let spans = app.classify(id, &cancel).unwrap();
    assert!(
      spans
        .iter()
        .any(|span| span.location.bytes == (call..call + 7)
          && span.kind == SemanticKind::Function)
    );
    assert!(
      spans
        .iter()
        .any(|span| span.location.bytes == (read..read + 4)
          && span.modifiers.contains(&Modifier::Readonly))
    );
    let links = app.document_links(id, &cancel).unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(
      (&source[links[0].0.bytes.clone()], &links[0].1),
      ("\"${base:-.}/$path\"", &library)
    );
    assert_eq!(
      app
        .document_symbols(id)
        .unwrap()
        .filter(|entry| entry.details.symbol.name == b"cleanup")
        .count(),
      1
    );
    assert_eq!(app.workspace_symbols("cleanup").count(), 1);
    let updated = format!("{source}; unset -f cleanup");
    app.change(&file, &updated, &cancel).unwrap();
    assert_eq!(app.definitions(id, call, &cancel).unwrap(), []);
    assert_eq!(app.definitions(id, read, &cancel).unwrap(), []);
    assert_eq!(app.document_links(id, &cancel).unwrap(), []);
    assert!(
      app
        .classify(id, &cancel)
        .unwrap()
        .iter()
        .any(|span| span.location.bytes == (call..call + 7)
          && span.kind == SemanticKind::Command)
    );
  }

  #[test]
  fn definitions_follow_bindings_and_argument_roles_without_guessing() {
    assert_definitions(&[
      ("x=1; : \"$x\"", "x\"", &["x=1"]),
      ("x=1; x=2; : \"${x}\"", "x}", &["x=2"]),
      ("x=1; export x", "x", &["x=1"]),
      ("x=1; readonly x", "x", &["x=1"]),
      ("x=1; unset -v x", "x", &["x=1"]),
      ("x=1; unset x x", "x", &[]),
      ("x=1; unset -v x; : \"$x\"", "x\"", &[]),
      ("f() { :; }; unset -f f", "f", &["f()"]),
      ("f() { :; }; command -v f", "f", &["f()"]),
      ("f() { :; }; command -p -V f", "f", &["f()"]),
      ("f() { :; }; command f", "f", &[]),
      ("f() { :; }; exec f", "f", &[]),
      ("x=1; read x; : \"$x\"", "x\"", &["x; :"]),
      ("read x", "x", &["x"]),
      ("export 'x=1'; : \"$x\"", "x\"", &["x=1"]),
      ("getopts a x; : \"$x\"", "x\"", &["x; :"]),
      ("readonly x=1; read x; : \"$x\"", "x\"", &["x=1"]),
      (
        "if condition; then x=1; else x=2; fi; : \"$x\"",
        "x\"",
        &["x=1", "x=2"],
      ),
      ("if condition; then x=1; fi; : \"$x\"", "x\"", &[]),
      ("x=1; . \"$unknown_path\"; : \"$x\"", "x\"", &[]),
      ("x=1; f() { : \"$x\"; }", "x\"", &[]),
      ("x=1; f() { read x; }; f; : \"$x\"", "x\"", &["x; }"]),
      ("x=0; : $((x+=1))", "x+=", &["x=0"]),
      ("f() { :; }; f", "f", &["f()"]),
      ("f() { :; }", "f", &["f()"]),
      ("f() { :; }; 'f'", "f'", &["f()"]),
      ("f() { :; }; 'f'", "'f'", &[]),
      ("f() { :; }; \\f", "f", &["f()"]),
      ("f() { :; }; x=f; \"$x\"", "x\"", &["x=f"]),
      ("f() { :; }; x=f; \"$x\"", "$x", &[]),
      ("f() { :; }; x=f; \"$x\"", "\"$x", &[]),
      ("x=1; : \"$x\"", "$x", &[]),
      ("x=1; : \"${x}\"", "}", &[]),
      ("x=1; export 'x'", "x'", &["x=1"]),
      ("x=1; export 'x'", "'x'", &[]),
      ("x=1; export \"x\"", "x\"", &["x=1"]),
      ("k=x; x=1; readonly \"$k\"", "k\"", &["k=x"]),
      ("x=1; read y; : \"$x\"", "x\"", &["x=1"]),
      ("read 'x'; : \"$x\"", "x\"", &["x'"]),
      ("x=1; . \"$unknown\"; x=2; : \"$x\"", "x\"", &[]),
      (
        "if condition; then f() { :; }; else f() { : 2; }; fi; f",
        "f",
        &["f() { :;", "f() { : 2;"],
      ),
      ("f() { :; }; q=f; command -v \"$q\"", "q\"", &["q=f"]),
      ("f() { \"$1\" x; }; x=1; f export; f printf", "x; }", &[]),
      ("f() { \"$1\" x; }; x=1; f read; f export", "x; }", &[]),
      ("getopts a x; : \"$OPTIND\"", "OPTIND\"", &[]),
      ("x=1; : x", "x", &[]),
      ("x=1; command() { :; }; command -v x", "x", &[]),
    ]);
  }

  #[test]
  fn definitions_of_expanded_arithmetic_require_complete_consistent_roles() {
    assert_definitions(&[
      ("x=1; n=2; : $((x+$n))", "x+$n", &["x=1"]),
      ("x=1; n=2; : $((x=$n)); : \"$x\"", "x\"", &["x=$n"]),
      ("x=1; f() { : $((x+$1)); }; f 1; f 2", "x+$1", &["x=1"]),
      ("x=1; f() { : $((x $1 2)); }; f +; f =", "x $1", &[]),
      (
        "x=1; f() { : $((x $1 2)); }; f +; read op; f \"$op\"",
        "x $1",
        &[],
      ),
      ("x=1; xy=2; f() { : $((x$1)); }; f ''; f y", "x$1", &[]),
      ("x=1; n=2; : $((x+$((n+1))))", "n+1", &["n=2"]),
      ("x=1; n=2; : $((x+$((n+$n))))", "n+$n", &["n=2"]),
    ]);
  }

  fn assert_definitions(cases: &[(&str, &str, &[&str])]) {
    for &(source, selected, expected) in cases {
      let fixture = Fixture::new();
      let path = fixture.write("main.sh", source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      let cancel = AtomicBool::new(false);
      app.open(&path, source, &cancel).unwrap();
      let id = app.document_id(&path).unwrap();
      let locations = app
        .definitions(id, source.rfind(selected).unwrap(), &cancel)
        .unwrap();
      let expected: Vec<_> = expected
        .iter()
        .map(|text| {
          let start = source.find(text).unwrap();
          start..start + 1
        })
        .collect();
      assert_eq!(
        locations
          .iter()
          .map(|location| location.bytes.clone())
          .collect::<Vec<_>>(),
        expected,
        "{source}"
      );
      assert!(
        locations
          .iter()
          .all(|location| location.snapshot.document == id)
      );
    }
  }

  #[test]
  fn definitions_keep_complete_source_candidates_and_ignore_unrelated_files() {
    let fixture = Fixture::new();
    let source = "if condition; then . ./b.sh; else . ./a.sh; fi\nf\n";
    let main = fixture.write("main.sh", source);
    let first = fixture.write("a.sh", "f() { :; }\n");
    let second = fixture.write("b.sh", "f() { : 2; }\n");
    fixture.write("unrelated.sh", "f() { : 3; }\n");
    let cancel = AtomicBool::new(false);
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    let mut app = Application::new(settings).unwrap();
    app.add_input(&fixture.root);
    app.refresh(&cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    let offset = source.rfind("f\n").unwrap();
    let definitions = app.definitions(id, offset, &cancel).unwrap();
    assert_eq!(
      definitions
        .iter()
        .map(|location| {
          (
            app.path(location.snapshot.document).unwrap(),
            location.bytes.clone(),
          )
        })
        .collect::<Vec<_>>(),
      [(first.as_path(), 0..1), (second.as_path(), 0..1)]
    );
    app.open(&first, "g() { :; }\n", &cancel).unwrap();
    assert_eq!(app.definitions(id, offset, &cancel).unwrap(), []);
  }

  #[test]
  fn projections_use_the_current_entry_without_reusing_previous_callers() {
    use shellsmith::interface::application::engine::{Modifier, SemanticKind};
    let fixture = Fixture::new();
    let source = ". ./lib.sh\nf\n: \"$x\"\n";
    let main = fixture.write("main.sh", source);
    let library_source = "f() { read x; }\n";
    let library = fixture.write("lib.sh", library_source);
    let cancel = AtomicBool::new(false);
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    let mut app = Application::new(settings).unwrap();
    app.open(&main, source, &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    assert_eq!(
      app
        .definitions(id, source.rfind('x').unwrap(), &cancel)
        .unwrap()
        .len(),
      1
    );
    let library_id = app.document_id(&library).unwrap();
    let offset = library_source.find('x').unwrap();
    assert_eq!(app.definitions(library_id, offset, &cancel).unwrap(), []);
    assert!(
      !app
        .classify(library_id, &cancel)
        .unwrap()
        .iter()
        .any(|span| {
          span.location.bytes == (offset..offset + 1)
            && span.kind == SemanticKind::Variable
            && span.modifiers.contains(&Modifier::Definition)
        })
    );
    assert_eq!(
      app
        .definitions(id, source.rfind('x').unwrap(), &cancel)
        .unwrap()
        .len(),
      1
    );
  }

  #[test]
  fn definitions_use_source_overlays_and_invalidate_changed_or_unavailable_targets()
   {
    let fixture = Fixture::new();
    let main_source = ". ./lib.sh\nf\n";
    let main = fixture.write("main.sh", main_source);
    let library = fixture.write("lib.sh", "f() { :; }\n");
    let cancel = AtomicBool::new(false);
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    let mut app = Application::new(settings).unwrap();
    app.open(&main, main_source, &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    let first = app.definitions(id, 11, &cancel).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(
      app.path(first[0].snapshot.document),
      Some(library.as_path())
    );
    assert_eq!(first[0].bytes, 0..1);
    app
      .open(&library, "# unsaved\nf() { :; }\n", &cancel)
      .unwrap();
    let updated = app.definitions(id, 11, &cancel).unwrap();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].bytes, 10..11);
    assert_ne!(updated[0].snapshot, first[0].snapshot);
    app.open(&library, "g() { :; }\n", &cancel).unwrap();
    assert_eq!(app.definitions(id, 11, &cancel).unwrap(), []);
    app.close(&library, &cancel).unwrap();
    assert_eq!(app.definitions(id, 11, &cancel).unwrap()[0].bytes, 0..1);
    std::fs::remove_file(&library).unwrap();
    app.refresh(&cancel).unwrap();
    assert_eq!(app.definitions(id, 11, &cancel).unwrap(), []);
  }
}

mod rename {
  use crate::fixtures::Fixture;
  use shellsmith::interface::application::engine::InitialEnvironment;
  use shellsmith::interface::application::{Application, Error, Settings};
  use std::sync::atomic::AtomicBool;

  fn application(fixture: &Fixture) -> Application {
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    Application::new(settings).unwrap()
  }

  fn assert_renamed(source: &str, query: &str, new_name: &str, expected: &str) {
    let fixture = Fixture::new();
    let path = fixture.write("main.sh", source);
    let mut app = application(&fixture);
    let cancel = AtomicBool::new(false);
    let offset = source.find(query).unwrap();
    let target = app
      .prepare_rename(&path, offset, &cancel)
      .unwrap_or_else(|error| panic!("{source}: {error}"));
    assert!(target.is_some(), "{source}");
    let edits = app
      .rename(&path, offset, new_name, &cancel)
      .unwrap_or_else(|error| panic!("{source}: {error}"));
    let mut result = source.to_string();
    for edit in edits.iter().rev() {
      result.replace_range(edit.location.bytes.clone(), &edit.new_text);
    }
    assert_eq!(result, expected, "{source}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
  }

  #[test]
  fn rename_resolves_closed_function_names_without_inventing_a_caller() {
    for (source, query, expected) in [
      (
        "trap '(helper() { :; }; helper)' EXIT",
        "helper()",
        "trap '(local_helper() { :; }; local_helper)' EXIT",
      ),
      (
        "trap \"trap '(helper() { :; }; helper)' TERM\" EXIT",
        "helper()",
        "trap \"trap '(local_helper() { :; }; local_helper)' TERM\" EXIT",
      ),
      (
        "idle() { (helper() { :; }; helper); }",
        "helper()",
        "idle() { (local_helper() { :; }; local_helper); }",
      ),
      (
        "helper() { :; }; idle() { (helper() { :; }; helper); }; helper",
        "helper() { :; }; helper",
        "helper() { :; }; idle() { (local_helper() { :; }; local_helper); }; helper",
      ),
      (
        "helper() { :; }; idle() { (helper() { :; }; helper); }; helper",
        "helper() { :; }; idle",
        "local_helper() { :; }; idle() { (helper() { :; }; helper); }; local_helper",
      ),
      (
        "idle() { (helper() { :; }; (helper)); }",
        "helper()",
        "idle() { (local_helper() { :; }; (local_helper)); }",
      ),
      (
        "idle() { (helper() { :; }; helper) & }",
        "helper()",
        "idle() { (local_helper() { :; }; local_helper) & }",
      ),
      (
        "idle() { { helper() { :; }; helper; } & }",
        "helper()",
        "idle() { { local_helper() { :; }; local_helper; } & }",
      ),
      (
        "idle() { value=$(helper() { :; }; helper); }",
        "helper()",
        "idle() { value=$(local_helper() { :; }; local_helper); }",
      ),
      (
        "idle() { (helper() { :; }; trap 'other' EXIT; helper); }",
        "helper()",
        "idle() { (local_helper() { :; }; trap 'other' EXIT; local_helper); }",
      ),
      (
        "idle() { (helper() { : $unknown; }; unset -f other; helper); }",
        "helper()",
        "idle() { (local_helper() { : $unknown; }; unset -f other; local_helper); }",
      ),
    ] {
      assert_renamed(source, query, "local_helper", expected);
    }
  }

  #[test]
  fn rename_shares_invariant_names_and_body_local_writes() {
    for (source, query, expected) in [
      (
        "f() { :; }; trap 'f' EXIT; f",
        "f()",
        "next() { :; }; trap 'next' EXIT; next",
      ),
      (
        "f() { :; }; trap f EXIT",
        "f()",
        "next() { :; }; trap next EXIT",
      ),
      (
        "x=1; trap ': \"$x\"; x=2; : \"$x\"' EXIT; : $x",
        "x=1",
        "next=1; trap ': \"$next\"; next=2; : \"$next\"' EXIT; : $next",
      ),
      (
        "x=outer; idle() { : $x; }; trap 'idle' EXIT",
        "x=outer",
        "next=outer; idle() { : $next; }; trap 'idle' EXIT",
      ),
      (
        "x=body; trap \"$x\" INT; trap '$x' EXIT",
        "x=body",
        "next=body; trap \"$next\" INT; trap '$next' EXIT",
      ),
      (
        "x=1; trap 'x=2' EXIT; : $x",
        "x=1",
        "next=1; trap 'next=2' EXIT; : $next",
      ),
    ] {
      assert_renamed(source, query, "next", expected);
    }
  }

  #[test]
  fn rename_respects_proven_functions_that_shadow_utilities() {
    for (source, query, expected) in [
      (
        "idle() { (read() { :; }; read x); }",
        "read",
        "idle() { (helper() { :; }; helper x); }",
      ),
      (
        "idle() { (command() { :; }; command -v x); }",
        "command",
        "idle() { (helper() { :; }; helper -v x); }",
      ),
      (
        "idle() { (command() { :; }; x() { :; }; command trap 'x; trap \"$unknown\" EXIT' EXIT; x); }",
        "x()",
        "idle() { (command() { :; }; helper() { :; }; command trap 'x; trap \"$unknown\" EXIT' EXIT; helper); }",
      ),
      (
        "idle() { (getopts() { :; }; getopts a x); }",
        "getopts",
        "idle() { (helper() { :; }; helper a x); }",
      ),
    ] {
      assert_renamed(source, query, "helper", expected);
    }
  }

  #[test]
  fn rename_uses_structural_background_operators() {
    for (source, query, expected) in [
      (
        "x=0; { x=1 # &\n: $x; }; : $x",
        "x=0",
        "renamed=0; { renamed=1 # &\n: $renamed; }; : $renamed",
      ),
      (
        "x=0; { x=1; # &\n: $x; }; : $x",
        "x=0",
        "renamed=0; { renamed=1; # &\n: $renamed; }; : $renamed",
      ),
      (
        "idle() { (helper() { :; } # &\nhelper); }",
        "helper()",
        "idle() { (renamed() { :; } # &\nrenamed); }",
      ),
      (
        "x=0; { x=1 <<EOF\n&\nEOF\n: $x; }; : $x",
        "x=0",
        "renamed=0; { renamed=1 <<EOF\n&\nEOF\n: $renamed; }; : $renamed",
      ),
      (
        "x=0; { x=1 \\\n& : $x; }; : $x",
        "x=0",
        "renamed=0; { x=1 \\\n& : $renamed; }; : $renamed",
      ),
    ] {
      assert_renamed(source, query, "renamed", expected);
    }
  }

  #[test]
  fn rename_inherits_functions_through_empty_isolated_scopes() {
    for (source, expected) in [
      (
        "idle() { (helper() { :; }; ((helper))); }",
        "idle() { (renamed() { :; }; ((renamed))); }",
      ),
      (
        "idle() { (helper() { :; }; (value=$(helper))); }",
        "idle() { (renamed() { :; }; (value=$(renamed))); }",
      ),
      (
        "idle() { (helper() { :; }; { (helper); } &); }",
        "idle() { (renamed() { :; }; { (renamed); } &); }",
      ),
    ] {
      assert_renamed(source, "helper()", "renamed", expected);
    }
  }

  #[test]
  fn rename_does_not_apply_deferred_calls_to_unrelated_environments() {
    for body in [
      "uname",
      "(uname)",
      "{ uname; }",
      "{ uname; } &",
      "value=$(uname)",
      "local_fn() { uname; }; local_fn",
      "command -p uname",
      "command -v other",
      "command trap 'exit 1' EXIT",
      "command command -v other",
      "\"tool_$suffix\"",
    ] {
      assert_renamed(
        &format!("f() {{ :; }}; idle() {{ {body}\n }}; f"),
        "f()",
        "renamed",
        &format!("renamed() {{ :; }}; idle() {{ {body}\n }}; renamed"),
      );
      assert_renamed(
        &format!("x=1; idle() {{ {body}\n }}; : $x"),
        "x=1",
        "renamed",
        &format!("renamed=1; idle() {{ {body}\n }}; : $renamed"),
      );
      assert_renamed(
        &format!("closed() {{ (f() {{ :; }}; f); }}; idle() {{ {body}\n }}"),
        "f()",
        "renamed",
        &format!(
          "closed() {{ (renamed() {{ :; }}; renamed); }}; idle() {{ {body}\n }}"
        ),
      );
    }
  }

  #[test]
  fn rename_keeps_deferred_name_dependencies_and_local_call_uncertainty() {
    for (source, query, new_name) in [
      ("f() { :; }; idle() { (f); }; f", "f()", "renamed"),
      ("f() { :; }; idle() { (uname); }; f", "f()", "uname"),
      ("x=1; idle() { (: $x); }; : $x", "x=1", "renamed"),
      ("x=1; idle() { (read x); }; : $x", "x=1", "renamed"),
      ("f() { :; }; idle() { command -v f; }; f", "f()", "renamed"),
      (
        "f() { :; }; idle() { command -v other; }; f",
        "f()",
        "other",
      ),
      (
        "tool_f() { :; }; idle() { \"tool_$suffix\"; }; tool_f",
        "tool_f()",
        "renamed",
      ),
      (
        "f() { :; }; idle() { \"tool_$suffix\"; }; f",
        "f()",
        "tool_f",
      ),
      ("idle() { (f() { :; }; uname; f); }", "f()", "renamed"),
      (
        "idle() { (f() { :; }; command -v other; f); }",
        "f()",
        "renamed",
      ),
    ] {
      let fixture = Fixture::new();
      let path = fixture.write("main.sh", source);
      let mut app = application(&fixture);
      assert!(
        matches!(
          app.rename(
            &path,
            source.find(query).unwrap(),
            new_name,
            &AtomicBool::new(false)
          ),
          Err(Error::RenameRefused(_))
        ),
        "{source} -> {new_name}"
      );
    }
  }

  #[test]
  fn rename_preserves_functions_outside_known_call_effects() {
    for body in [
      "unset -f other",
      "unset -f \"tmp_$suffix\"",
      "other() { :; }",
      "if :; then unset -f other; else other() { :; }; fi",
      "(unset -f other)",
      "{ unset -f other; } &",
      "\"/bin/$tool\"",
    ] {
      assert_renamed(
        &format!(
          "idle() {{ (clean() {{ {body}\n }}; helper() {{ :; }}; clean; helper; clean; helper); }}"
        ),
        "helper()",
        "renamed",
        &format!(
          "idle() {{ (clean() {{ {body}\n }}; renamed() {{ :; }}; clean; renamed; clean; renamed); }}"
        ),
      );
    }
  }

  #[test]
  fn rename_refuses_overlapping_or_unknown_call_effects() {
    for (body, new_name) in [
      ("unset -f helper", "renamed"),
      ("unset -f other", "other"),
      ("unset -f \"tmp_$suffix\"", "tmp_helper"),
      ("unset -f \"$target\"", "renamed"),
      ("uname", "renamed"),
      ("\"$callback\"", "renamed"),
      ("\".$suffix\" ./unknown", "renamed"),
      ("unset -f \"$target\" | :", "renamed"),
    ] {
      let source = format!(
        "idle() {{ (clean() {{ {body}; }}; helper() {{ :; }}; clean; helper); }}"
      );
      let fixture = Fixture::new();
      let path = fixture.write("main.sh", &source);
      let mut app = application(&fixture);
      assert!(
        matches!(
          app.rename(
            &path,
            source.find("helper()").unwrap(),
            new_name,
            &AtomicBool::new(false)
          ),
          Err(Error::RenameRefused(_))
        ),
        "{body} -> {new_name}"
      );
    }
  }

  #[test]
  fn rename_refuses_incomplete_trap_references_without_registration_time_bindings()
   {
    for (source, query, new_name) in [
      (
        "f() { :; }; idle() { command trap 'f' EXIT; }; f",
        "f()",
        "next",
      ),
      ("f() { :; }; trap 'next' INT; f", "f()", "next"),
      (
        "f() { :; }; read action; trap \"$action\" EXIT; f",
        "f()",
        "next",
      ),
      ("f() { :; }; trap 'if then' EXIT; f", "f()", "next"),
      (
        "idle() { (helper() { :; }; trap 'helper' EXIT; helper); }",
        "helper()",
        "next",
      ),
    ] {
      assert_rename_refused(source, query, new_name);
    }
  }

  fn assert_rename_refused(source: &str, query: &str, new_name: &str) {
    let fixture = Fixture::new();
    let path = fixture.write("main.sh", source);
    let mut app = application(&fixture);
    assert!(
      matches!(
        app.rename(
          &path,
          source.find(query).unwrap(),
          new_name,
          &AtomicBool::new(false)
        ),
        Err(Error::RenameRefused(_))
      ),
      "{source}"
    );
  }

  #[test]
  fn rename_refuses_caller_dependencies_and_unproved_local_function_bindings() {
    for (source, query, new_name) in [
      ("idle() { x=inner; : $x; }", "x=inner", "y"),
      (
        "readonly x=outer; idle() { (x=inner; : $x); }",
        "x=inner",
        "y",
      ),
      ("idle() { helper() { :; }; helper; }", "helper()", "next"),
      (
        "idle() { (helper; helper() { :; }; helper); }",
        "helper()",
        "next",
      ),
      (
        "idle() { (if :; then helper() { :; }; fi; helper); }",
        "helper()",
        "next",
      ),
      (
        "idle() { (: && helper() { :; }; helper); }",
        "helper()",
        "next",
      ),
      (
        "idle() { (for value in a; do helper() { :; }; done; helper); }",
        "helper()",
        "next",
      ),
      ("idle() { (helper() { :; } & helper); }", "helper()", "next"),
      ("idle() { (helper() { :; } | helper); }", "helper()", "next"),
      (
        "idle() { (helper() { :; }; unset -f helper; helper); }",
        "helper()",
        "next",
      ),
      (
        "idle() { (helper() { :; }; . ./unknown; helper); }",
        "helper()",
        "next",
      ),
      (
        "idle() { (helper() { :; }; \"$callback\"; helper); }",
        "helper()",
        "next",
      ),
      (
        "idle() { (helper() { helper; }; helper); }",
        "helper()",
        "next",
      ),
      (
        "idle() { (helper() { unset -f helper; }; helper; helper); }",
        "helper()",
        "next",
      ),
      (
        "idle() { (helper() { :; }; helper; next() { :; }; next); }",
        "helper()",
        "next",
      ),
      (
        "idle() { (helper() { :; }; helper; next); }",
        "helper()",
        "next",
      ),
    ] {
      assert_rename_refused(source, query, new_name);
    }
  }

  #[test]
  fn rename_preserves_names_and_source_delimiters() {
    for (source, query, new_name, expected) in [
      (
        "x=1; x=2; : $x ${x}",
        "x=1",
        "value",
        "value=1; value=2; : $value ${value}",
      ),
      (
        "x=1; unset x; x=2; : $x",
        "x=1",
        "y",
        "y=1; unset y; y=2; : $y",
      ),
      ("read x; : $x", "x;", "value", "read value; : $value"),
      (
        "export 'x=1'; readonly x; : $x",
        "x=",
        "value",
        "export 'value=1'; readonly value; : $value",
      ),
      ("getopts a x; : $x", "x;", "flag", "getopts a flag; : $flag"),
      (
        "x=0; : $((x+=1)); : $x",
        "x=0",
        "value",
        "value=0; : $((value+=1)); : $value",
      ),
      (
        "f() { :; }; f; 'f'; command -v f; unset -f f",
        "f()",
        "run",
        "run() { :; }; run; 'run'; command -v run; unset -f run",
      ),
      (
        "fun() { :; }; f'u'n; \\fun",
        "fun()",
        "run",
        "run() { :; }; run; run",
      ),
      (
        ": ${x:=1}; : \"$x\"",
        "x:=",
        "value",
        ": ${value:=1}; : \"$value\"",
      ),
      (
        "x=1; : \"${x:-none}\" \"${#x}\"",
        "x=1",
        "value",
        "value=1; : \"${value:-none}\" \"${#value}\"",
      ),
      (
        "x=1; cat <<EOF\n$x ${x}\nEOF\n",
        "x=1",
        "value",
        "value=1; cat <<EOF\n$value ${value}\nEOF\n",
      ),
      (
        "x=1; : `printf '%s' \"$x\"`",
        "x=1",
        "value",
        "value=1; : `printf '%s' \"$value\"`",
      ),
      (
        "x=1; : \"$(printf '%s' \"$x\")\"",
        "x=1",
        "value",
        "value=1; : \"$(printf '%s' \"$value\")\"",
      ),
      (
        "x=1; unset x; : \"$x\"",
        "x=1",
        "value",
        "value=1; unset value; : \"$value\"",
      ),
    ] {
      assert_renamed(source, query, new_name, expected);
    }
  }

  #[test]
  fn rename_respects_callers_environments_and_excluded_effects() {
    for (source, query, new_name, expected) in [
      (
        "x=1; f() { x=2; : $x; }; f; : $x",
        "x=1",
        "y",
        "y=1; f() { y=2; : $y; }; f; : $y",
      ),
      (
        "x=1; (x=2; : $x); : $x",
        "x=1",
        "y",
        "y=1; (x=2; : $x); : $y",
      ),
      (
        "x=1; (: $x; x=2; : $x); : $x",
        "x=1",
        "y",
        "y=1; (: $y; y=2; : $y); : $y",
      ),
      ("x=1; x=2 true; : $x", "x=1", "y", "y=1; x=2 true; : $y"),
      ("x=1; x=2 read x; : $x", "x=1", "y", "y=1; x=2 read x; : $y"),
      ("x=1; x=2 read x; : $x", "x=2", "y", "x=1; y=2 read y; : $x"),
      ("x=1; x=$x true; : $x", "x=1", "y", "y=1; x=$y true; : $y"),
      (
        "x=1; x=2 true; x=3 true; : $x",
        "x=2",
        "y",
        "x=1; y=2 true; x=3 true; : $x",
      ),
      (
        "x=1; f() { : $x; }; (f); : $x",
        "x=1",
        "y",
        "y=1; f() { : $y; }; (f); : $y",
      ),
      (
        "x=1; (unset x; x=2; : $x); : $x",
        "x=1",
        "y",
        "y=1; (unset y; y=2; : $y); : $y",
      ),
      (
        "x=1; other=2 read x; : $x",
        "x=1",
        "y",
        "y=1; other=2 read y; : $y",
      ),
      ("x=1; n=2; : $((x+$n))", "x=1", "y", "y=1; n=2; : $((y+$n))"),
      (
        "x=1; x() { :; }; x; : $x",
        "x=1",
        "y",
        "y=1; x() { :; }; x; : $y",
      ),
      (
        "x=1; trap 'other=2' INT; eval 'x=3'; : $x",
        "x=1",
        "y",
        "y=1; trap 'other=2' INT; eval 'x=3'; : $y",
      ),
      (
        "if condition; then x=1; else x=2; fi; : $x",
        "x=1",
        "y",
        "if condition; then y=1; else y=2; fi; : $y",
      ),
      (
        "for x in a b; do : $x; done",
        "x in",
        "y",
        "for y in a b; do : $y; done",
      ),
      (
        "x=1; read y; : $x",
        "x=1",
        "value",
        "value=1; read y; : $value",
      ),
    ] {
      assert_renamed(source, query, new_name, expected);
    }
  }

  #[test]
  fn rename_preserves_inherited_readonly_bindings_for_attempted_writes() {
    for (source, expected) in [
      (
        "x=1; readonly x; (x=2); : $x",
        "y=1; readonly y; (y=2); : $y",
      ),
      (
        "x=1; readonly x; (read x); : $x",
        "y=1; readonly y; (read y); : $y",
      ),
      (
        "x=1; readonly x; (export 'x=2'); : $x",
        "y=1; readonly y; (export 'y=2'); : $y",
      ),
      (
        "readonly x=1; x=2 read x; : $x",
        "readonly y=1; y=2 read y; : $y",
      ),
      (
        "x=1; if condition; then readonly x; fi; (x=2); : $x",
        "y=1; if condition; then readonly y; fi; (y=2); : $y",
      ),
    ] {
      assert_renamed(source, "x=1", "y", expected);
    }
  }

  #[test]
  fn rename_accepts_unrelated_arithmetic_and_analyzed_expansions() {
    for (source, query, new_name, expected) in [
      (
        "x=1; f() { : $((other+1)); }; : $x",
        "x=1",
        "y",
        "y=1; f() { : $((other+1)); }; : $y",
      ),
      (
        "x=1; f() { : $((1+$((2)))); }; : $x",
        "x=1",
        "y",
        "y=1; f() { : $((1+$((2)))); }; : $y",
      ),
      (
        "run() { :; }; expr=x; f() { : $(($expr)); }; run",
        "run()",
        "launch",
        "launch() { :; }; expr=x; f() { : $(($expr)); }; launch",
      ),
      (
        "x=1; n=2; f() { : $((x+$n)); }; f",
        "x=1",
        "y",
        "y=1; n=2; f() { : $((y+$n)); }; f",
      ),
    ] {
      assert_renamed(source, query, new_name, expected);
    }
  }

  #[test]
  fn rename_preserves_unrelated_generated_names_and_argument_expansions() {
    for (source, query, new_name, expected) in [
      (
        "f() { :; }; cmd=f; \"$cmd\"",
        "cmd=",
        "name",
        "f() { :; }; name=f; \"$name\"",
      ),
      (
        "key=x; x=1; readonly \"$key\"; : $x",
        "key=",
        "name",
        "name=x; x=1; readonly \"$name\"; : $x",
      ),
      (
        "x=body; trap \"$x\" INT; trap ': \"$other\"' EXIT",
        "x=",
        "y",
        "y=body; trap \"$y\" INT; trap ': \"$other\"' EXIT",
      ),
    ] {
      assert_renamed(source, query, new_name, expected);
    }
  }

  #[test]
  fn rename_uses_engine_constraints_to_exclude_unrelated_command_effects() {
    for body in [
      "tool=$(command -v missing); \"$tool\"",
      "read name; \"/bin/$name\"",
      "tool=$(command -v missing); copy=$(command -p printf '%s' \"$tool\"); command \"$copy\"",
      "idle() { \"/bin/$name\"; }",
    ] {
      assert_renamed(
        &format!("x=1; {body}; : $x"),
        "x=1",
        "y",
        &format!("y=1; {body}; : $y"),
      );
    }
  }

  #[test]
  fn rename_ignores_unattempted_effects_on_other_names_and_namespaces() {
    for body in [
      "export other=\"$value\"",
      "readonly other=\"$value\"",
      "export \"other=$value\"",
      "readonly \"other=$value\"",
      "export \"tmp_$target=value\"",
      "readonly \"tmp_$target=value\"",
      "getopts \":$options\" selected \"$@\"",
      "getopts -- \"$options\" selected \"$@\"",
      "getopts a selected $arguments",
      "unset -f -- \"$target\"",
      "unset -f fn\"$suffix\"",
      "read -r other",
      "read -r \"tmp_$suffix\"",
      "unset -v -- \"tmp_$suffix\"",
      "getopts a \"tmp_$suffix\"",
    ] {
      assert_renamed(
        &format!("x=1; idle() {{ {body}; }}; : $x"),
        "x=1",
        "y",
        &format!("y=1; idle() {{ {body}; }}; : $y"),
      );
    }
    for body in [
      "read \"$target\"",
      "getopts \"$options\" \"$target\" \"$@\"",
      "export \"$target\"",
      "readonly \"$target\"",
      "unset -v -- \"$target\"",
    ] {
      assert_renamed(
        &format!("run() {{ :; }}; idle() {{ {body}; }}; run"),
        "run()",
        "launch",
        &format!("launch() {{ :; }}; idle() {{ {body}; }}; launch"),
      );
    }
  }

  #[test]
  fn rename_refuses_incomplete_related_names_and_collisions() {
    for (source, query, new_name) in [
      ("x=1; idle() { export x=\"$value\"; }; : $x", "x=1", "y"),
      (
        "x=1; idle() { export \"$target=value\"; }; : $x",
        "x=1",
        "y",
      ),
      (
        "x=1; idle() { readonly \"$target=value\"; }; : $x",
        "x=1",
        "y",
      ),
      (
        "tmp_x=1; idle() { export \"tmp_$target=value\"; }; : $tmp_x",
        "tmp_x=1",
        "renamed",
      ),
      (
        "x=1; idle() { getopts \"$options\" x \"$@\"; }; : $x",
        "x=1",
        "y",
      ),
      ("x=1; idle() { getopts \"$@\" other; }; : $x", "x=1", "y"),
      (
        "x=1; idle() { getopts \"$options\" other \"$@\"; }; : $x",
        "x=1",
        "y",
      ),
      ("x=1; idle() { getopts $options other; }; : $x", "x=1", "y"),
      (
        "x=1; idle() { unset \"$options\" \"$target\"; }; : $x",
        "x=1",
        "y",
      ),
      (
        "run() { :; }; idle() { unset -f -- \"$target\"; }; run",
        "run()",
        "launch",
      ),
      (
        "run() { :; }; idle() { unset \"$options\" \"$target\"; }; run",
        "run()",
        "launch",
      ),
      ("x=1; expr=x; f() { : $(($expr)); }", "x=1", "y"),
      ("x=1; expr=x; f() { value=$(($expr)); }", "x=1", "y"),
      ("x=1; f() { : $(($(printf x))); }", "x=1", "y"),
      ("x=1; f() { : $((x+$n)); }", "x=1", "y"),
      ("x=1; f() { : `printf '%s' \"$x\"`; }", "x=1", "y"),
      ("x=1; f() { : $x; }", "x=1", "y"),
      ("x=1; f() { read x; }", "x=1", "y"),
      ("x=1; f() { . ./unknown; }", "x=1", "y"),
      ("x=1; (. ./unknown)", "x=1", "y"),
      ("x=1; f() { read \"$target\"; }", "x=1", "y"),
      (
        "tmp_x=1; f() { read \"tmp_$target\"; }",
        "tmp_x=1",
        "renamed",
      ),
      ("x=1; f() { read \"tmp_$target\"; }", "x=1", "tmp_x"),
      ("f() { :; }; g() { command -v f; }", "f()", "run"),
      ("f() { \"$1\" x; }; x=1; f export; f printf", "x=1", "y"),
      (
        "f() { :; }; command_name=f; \"$command_name\"",
        "f()",
        "run",
      ),
      ("x=1; key=x; readonly \"$key\"", "x=1", "y"),
      ("x=1; . \"$unknown\"; x=2; : $x", "x=1", "y"),
      ("read unknown; x=1; read \"$unknown\"; : $x", "x=1", "y"),
      ("x=1; y=2; : $x $y", "x=1", "y"),
      ("x=1; : $y", "x=1", "y"),
      ("x=1; if condition", "x=1", "y"),
      ("f() { :; }; g; f", "f()", "g"),
      ("f() { :; }; command f", "f()", "g"),
    ] {
      let fixture = Fixture::new();
      let path = fixture.write("main.sh", source);
      let mut app = application(&fixture);
      assert!(
        matches!(
          app.rename(
            &path,
            source.find(query).unwrap(),
            new_name,
            &AtomicBool::new(false)
          ),
          Err(Error::RenameRefused(_))
        ),
        "{source}"
      );
    }
  }

  #[test]
  fn rename_collects_shared_sources_across_entries_without_merging_unrelated_names()
   {
    let fixture = Fixture::new();
    let first = fixture.write("a.sh", "x=first; . ./shared.sh; : $x");
    let second = fixture.write("b.sh", "x=second; . ./shared.sh; : $x");
    let shared = fixture.write("shared.sh", "x=shared; : $x");
    let unrelated = fixture.write("unrelated.sh", "x=unrelated; : $x");
    let mut app = application(&fixture);
    app.add_input(&fixture.root);
    let edits = app
      .rename(&first, 0, "value", &AtomicBool::new(false))
      .unwrap();
    let locations: Vec<_> = edits
      .iter()
      .map(|edit| {
        (
          app
            .path(edit.location.snapshot.document)
            .unwrap()
            .to_path_buf(),
          edit.location.bytes.clone(),
        )
      })
      .collect();
    assert_eq!(
      locations,
      [
        (first.clone(), 0..1),
        (first, 27..28),
        (second.clone(), 0..1),
        (second, 28..29),
        (shared.clone(), 0..1),
        (shared, 13..14)
      ]
    );
    assert!(!locations.iter().any(|(path, _)| path == &unrelated));
  }

  #[test]
  fn rename_handles_multiple_complete_function_definitions_and_external_sources()
   {
    let fixture = Fixture::new();
    let external = Fixture::new();
    let library = external.write("library", "f() { :; }; f");
    let main =
      fixture.write("main.sh", &format!(". '{}'; f", library.display()));
    let mut app = application(&fixture);
    app.add_input(&fixture.root);
    let cancel = AtomicBool::new(false);
    let edits = app
      .rename(
        &main,
        std::fs::read_to_string(&main).unwrap().len() - 1,
        "run",
        &cancel,
      )
      .unwrap();
    assert_eq!(edits.len(), 3);
    assert_eq!(
      edits
        .iter()
        .filter(|edit| app.path(edit.location.snapshot.document)
          == Some(library.as_path()))
        .count(),
      2
    );
    let source = "if condition; then f() { :; }; else f() { : 2; }; fi; f";
    app.open(&main, source, &cancel).unwrap();
    assert_eq!(
      app
        .rename(&main, source.len() - 1, "run", &cancel)
        .unwrap()
        .len(),
      3
    );
  }

  #[test]
  fn rename_rechecks_closed_inputs_and_uses_current_open_text() {
    let fixture = Fixture::new();
    let path = fixture.write("main.sh", "x=1; : $x");
    let cancel = AtomicBool::new(false);
    let mut app = application(&fixture);
    app.prepare_rename(&path, 0, &cancel).unwrap();
    fixture.write("main.sh", "x=1; y=2; : $x");
    assert!(matches!(
      app.rename(&path, 0, "y", &cancel),
      Err(Error::Superseded)
    ));
    assert!(matches!(
      app.rename(&path, 0, "y", &cancel),
      Err(Error::RenameRefused(_))
    ));
    app.open(&path, "x=1; : $x", &cancel).unwrap();
    assert_eq!(app.rename(&path, 0, "y", &cancel).unwrap().len(), 2);
    app.change(&path, "x=1; y=2; : $x", &cancel).unwrap();
    assert!(matches!(
      app.rename(&path, 0, "y", &cancel),
      Err(Error::RenameRefused(_))
    ));
  }

  #[tokio::test]
  async fn rename_retries_failed_collection_after_an_input_is_repaired() {
    use shellsmith::interface::application::{RequestScope, Session};

    let fixture = Fixture::new();
    let path = fixture.write("main.sh", "x=1; : $x");
    let broken = fixture.write("broken.sh", "");
    std::fs::write(&broken, [0xff]).unwrap();
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    let session = Session::new(settings).unwrap();
    session.add_input(&fixture.root).unwrap();
    for repaired in [false, true] {
      if repaired {
        fixture.write("broken.sh", "other=2; : $other");
      }
      let document = path.clone();
      let result = session
        .request(RequestScope::Names(path.clone()), move |app, cancelled| {
          app.rename(&document, 0, "value", cancelled)
        })
        .await;
      if repaired {
        assert_eq!(result.unwrap().len(), 2);
      } else {
        assert!(matches!(result, Err(Error::RenameRefused(_))));
      }
    }
  }

  #[test]
  fn rename_rejects_invalid_and_special_names_and_returns_no_edits_for_same_name()
   {
    let fixture = Fixture::new();
    let path = fixture.write("main.sh", "x=1; : $x; f() { :; }; f");
    let cancel = AtomicBool::new(false);
    let mut app = application(&fixture);
    for name in ["", "1x", "a-b", "日本語", "IFS", "HOME", "OPTARG"] {
      assert!(
        matches!(
          app.rename(&path, 0, name, &cancel),
          Err(Error::InvalidName(_))
        ),
        "{name}"
      );
    }
    for name in ["if", "export", "exit"] {
      assert!(
        matches!(
          app.rename(&path, 11, name, &cancel),
          Err(Error::InvalidName(_))
        ),
        "{name}"
      );
    }
    assert_eq!(app.rename(&path, 0, "x", &cancel).unwrap(), []);
    for source in ["PATH=x; : $PATH", ": $1", ": $?", ": $HOME", "printf text"]
    {
      app.open(&path, source, &cancel).unwrap();
      for (offset, _) in source.char_indices() {
        assert_eq!(
          app.prepare_rename(&path, offset, &cancel).unwrap(),
          None,
          "{source}:{offset}"
        );
      }
    }
  }

  #[test]
  fn rename_rejects_optional_reserved_words_as_new_function_names() {
    let fixture = Fixture::new();
    let path = fixture.write("main.sh", "");
    let cancel = AtomicBool::new(false);
    let mut app = application(&fixture);
    for source in [
      "f() { :; }",
      "f() { :; }; f",
      "f() { :; }; 'f'",
      "f() { :; }; \"f\"",
    ] {
      app.open(&path, source, &cancel).unwrap();
      for name in ["function", "namespace", "select", "time"] {
        assert!(
          matches!(
            app.rename(&path, 0, name, &cancel),
            Err(Error::InvalidName(_))
          ),
          "{source} -> {name}"
        );
      }
    }
  }

  #[test]
  fn rename_allows_optional_reserved_variable_names_and_replacing_function_names()
   {
    for (name, variable, function) in [
      ("function", "function=1; : $function", "function() { :; }"),
      (
        "namespace",
        "namespace=1; : $namespace",
        "namespace() { :; }",
      ),
      ("select", "select=1; : $select", "select() { :; }"),
      ("time", "time=1; : $time", "time() { :; }"),
    ] {
      assert_renamed("x=1; : $x", "x=", name, variable);
      assert_renamed(variable, name, "x", "x=1; : $x");
      assert_renamed(function, name, "f", "f() { :; }");
    }
  }

  #[test]
  fn rename_requests_keep_trap_body_symbols_of_other_inputs() {
    let fixture = Fixture::new();
    let trap = fixture.write("a.sh", "trap 'f() { :; }' EXIT\n");
    let main = fixture.write("b.sh", "x=1; : $x\n");
    let mut app = application(&fixture);
    app.add_input(&fixture.root);
    let cancel = AtomicBool::new(false);
    app.refresh(&cancel).unwrap();
    let symbols = |app: &Application| {
      let id = app.document_id(&trap).unwrap();
      let names: Vec<_> = app
        .document_symbols(id)
        .unwrap()
        .map(|entry| entry.details.symbol.name.clone())
        .collect();
      (names, app.workspace_symbols("f").count())
    };
    let expected = (vec![b"f".to_vec()], 1);
    assert_eq!(symbols(&app), expected);
    assert!(app.prepare_rename(&main, 0, &cancel).unwrap().is_some());
    assert_eq!(symbols(&app), expected);
    assert_eq!(app.rename(&main, 0, "y", &cancel).unwrap().len(), 2);
    assert_eq!(symbols(&app), expected);
  }

  #[test]
  fn rename_revalidates_workspace_membership_and_keeps_resolved_paths() {
    let fixture = Fixture::new();
    let main = fixture.write("main.sh", "target=./lib.sh; . \"$target\"");
    let library = fixture.write("lib.sh", "x=1; : $x");
    let mut app = application(&fixture);
    app.add_input(&fixture.root);
    let cancel = AtomicBool::new(false);
    assert_eq!(
      app.rename(&main, 0, "source_file", &cancel).unwrap().len(),
      2
    );
    app.prepare_rename(&library, 0, &cancel).unwrap();
    let added = fixture.write("caller.sh", ". ./lib.sh; : $x");
    assert!(matches!(
      app.rename(&library, 0, "y", &cancel),
      Err(Error::Superseded)
    ));
    let edits = app.rename(&library, 0, "y", &cancel).unwrap();
    assert_eq!(edits.len(), 3);
    assert!(
      edits
        .iter()
        .any(|edit| app.path(edit.location.snapshot.document)
          == Some(added.as_path()))
    );
  }
}
