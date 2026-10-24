use super::*;

#[tokio::test]
async fn preparation_survives_one_cancellation_and_stops_after_the_last_subscriber()
 {
  let session =
    Session::new(Settings::new(PathBuf::from(env!("CARGO_MANIFEST_DIR"))))
      .unwrap();
  let scope = RequestScope::Document(
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"),
  );
  let first = session.request(scope.clone(), |_, _| Ok(()));
  let second = session.request(scope.clone(), |_, _| Ok(()));
  let shared = lock(&session.state).unwrap().preparations[&scope].clone();
  drop(first);
  assert!(!shared.cancelled.load(Ordering::Acquire));
  second.await.unwrap();
  assert!(shared.cancelled.load(Ordering::Acquire));

  let abandoned = session.request(scope.clone(), |_, _| Ok(()));
  let job = lock(&session.state).unwrap().preparations[&scope].clone();
  assert!(!Arc::ptr_eq(&shared, &job));
  drop(abandoned);
  assert!(job.cancelled.load(Ordering::Acquire));
  session.request(scope.clone(), |_, _| Ok(())).await.unwrap();
}

#[tokio::test]
async fn cancelling_the_last_dependent_request_cancels_workspace_preparations()
{
  let session =
    Session::new(Settings::new(PathBuf::from(env!("CARGO_MANIFEST_DIR"))))
      .unwrap();
  let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
  let names = session.request(RequestScope::Names(path.clone()), |_, _| Ok(()));
  let results = session.request(RequestScope::Results(path), |_, _| Ok(()));
  let (workspace, analysis) = {
    let state = lock(&session.state).unwrap();
    (
      state.preparations[&RequestScope::Workspace].clone(),
      state.preparations[&RequestScope::WorkspaceAnalysis].clone(),
    )
  };
  drop(names);
  assert!(!analysis.cancelled.load(Ordering::Acquire));
  assert!(!workspace.cancelled.load(Ordering::Acquire));
  drop(results);
  assert!(analysis.cancelled.load(Ordering::Acquire));
  assert!(workspace.cancelled.load(Ordering::Acquire));
}

#[tokio::test]
async fn input_changes_cancel_preparation_before_it_runs() {
  let session =
    Session::new(Settings::new(PathBuf::from(env!("CARGO_MANIFEST_DIR"))))
      .unwrap();
  let first = session.request::<()>(RequestScope::Workspace, |_, _| {
    panic!("obsolete operation must not run")
  });
  let second = session.request::<()>(RequestScope::Workspace, |_, _| {
    panic!("obsolete operation must not run")
  });
  let job = lock(&session.state).unwrap().preparations
    [&RequestScope::Workspace]
    .clone();
  session.invalidate().unwrap();
  assert!(job.cancelled.load(Ordering::Acquire));
  let (first, second) = tokio::join!(first, second);
  assert!(matches!(first, Err(Error::Superseded)));
  assert!(matches!(second, Err(Error::Superseded)));
  session
    .request(RequestScope::Workspace, |_, _| Ok(()))
    .await
    .unwrap();
}

use super::super::fixtures::Fixture;

#[tokio::test]
async fn navigation_and_names_wait_for_shared_workspace_results_while_tokens_remain_available()
 {
  use crate::interface::application::engine::SemanticKind;
  let fixture = Fixture::new();
  let source = "a() { :; }\nb() { a; . ./target.sh; }\n";
  let library = fixture.write("lib.sh", source);
  let caller = fixture.write("main.sh", ". ./lib.sh\nb\n");
  let target = fixture.write("target.sh", ":\n");
  let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
  session.add_input(&caller).unwrap();
  session.set_overlay(&library, Some(source.into())).unwrap();
  let held = session
    .analysis_worker
    .clone()
    .acquire_owned()
    .await
    .unwrap();
  session.start_index().unwrap();
  let shared = lock(&session.state).unwrap().preparations
    [&RequestScope::WorkspaceAnalysis]
    .clone();
  let path = library.clone();
  let navigation = tokio::spawn(session.request(
    RequestScope::Results(library.clone()),
    move |app, cancelled| {
      let id = app.document_id(&path).unwrap();
      let definitions = app.definitions(id, 17, cancelled)?;
      assert_eq!(definitions.len(), 1);
      assert_eq!(definitions[0].bytes, 0..1);
      let links = app.document_links(id, cancelled)?;
      assert_eq!(links.len(), 1);
      assert_eq!(links[0].1, target);
      Ok(Arc::as_ptr(app.analyses.as_ref().unwrap()).addr())
    },
  ));
  let path = library.clone();
  let references = tokio::spawn(session.request(
    RequestScope::Names(library.clone()),
    move |app, cancelled| {
      let uses = app.references(&path, 17, false, cancelled)?;
      assert_eq!(
        uses
          .iter()
          .map(|site| (site.bytes.start, site.bytes.end))
          .collect::<Vec<_>>(),
        [(17, 18)]
      );
      Ok(Arc::as_ptr(app.analyses.as_ref().unwrap()).addr())
    },
  ));
  let path = library.clone();
  let tokens = session
    .request(
      RequestScope::Analysis(library.clone()),
      move |app, cancelled| {
        app.classify(app.document_id(&path).unwrap(), cancelled)
      },
    )
    .await
    .unwrap();
  assert!(tokens.iter().any(|span| span.location.bytes == (17..18)
    && span.kind == SemanticKind::Command));
  assert!(!navigation.is_finished());
  assert!(!references.is_finished());
  assert!(Arc::ptr_eq(
    &shared,
    &lock(&session.state).unwrap().preparations
      [&RequestScope::WorkspaceAnalysis]
  ));
  drop(held);
  assert_eq!(
    navigation.await.unwrap().unwrap(),
    references.await.unwrap().unwrap()
  );
}

#[tokio::test]
async fn concurrent_tokens_links_and_definitions_share_analysis_and_refresh_changed_dependencies()
 {
  let fixture = Fixture::new();
  let path = fixture.write(
    "main.sh",
    "trap ':' INT\n. ./lib.sh\n. \"$target\"\n: \"$value\"\n",
  );
  fixture.write("lib.sh", "target=./first.sh\n");
  let first = fixture.write("first.sh", "value=first\n");
  let second = fixture.write("second.sh", "value=second\n");
  let settings = Settings::new(fixture.root.clone());
  let session = Session::new(settings).unwrap();
  session.add_input(&path).unwrap();
  for expected in [first, second] {
    let document = path.clone();
    let tokens = session.request(
      RequestScope::Analysis(path.clone()),
      move |application, cancelled| {
        let id = application.document_id(&document).unwrap();
        let tokens = application.classify(id, cancelled)?;
        let analysis = application
          .analyses
          .as_ref()
          .map(|analyses| Arc::as_ptr(analyses).addr());
        Ok((tokens, analysis))
      },
    );
    let document = path.clone();
    let links = session.request(
      RequestScope::Results(path.clone()),
      move |application, cancelled| {
        let id = application.document_id(&document).unwrap();
        let links = application.document_links(id, cancelled)?;
        let analysis = application
          .analyses
          .as_ref()
          .map(|analyses| Arc::as_ptr(analyses).addr());
        Ok((links, analysis))
      },
    );
    let document = path.clone();
    let definitions = session.request(
      RequestScope::Results(path.clone()),
      move |application, cancelled| {
        let id = application.document_id(&document).unwrap();
        let source = application.workspace().document(id).unwrap().source();
        let offset = source.rfind("value").unwrap();
        let definitions = application.definitions(id, offset, cancelled)?;
        let analysis = application
          .analyses
          .as_ref()
          .map(|analyses| Arc::as_ptr(analyses).addr());
        Ok((definitions, analysis))
      },
    );
    let (tokens, links, definitions) = tokio::join!(tokens, links, definitions);
    let (definitions, definitions_analysis) = definitions.unwrap();
    assert_eq!(definitions.len(), 1);

    let (tokens, _) = tokens.unwrap();
    let (links, links_analysis) = links.unwrap();
    assert_ne!(tokens, []);
    assert!(links_analysis.is_some());
    assert_eq!(links_analysis, definitions_analysis);
    assert_eq!(links.len(), 2);
    assert_eq!(links[1].1, expected);
    let changed = fixture.write("lib.sh", "target=./second.sh\n");
    session.files_changed(&[changed]).unwrap();
  }
}

#[tokio::test]
async fn syntax_requests_share_parsed_documents_and_refresh_the_index_after_edits()
 {
  let fixture = Fixture::new();
  let main = fixture.write("main.sh", "before=1\n. ./missing.sh\n");
  let settings = Settings::new(fixture.root.clone());
  let session = Session::new(settings).unwrap();
  session.add_input(&fixture.root).unwrap();
  for name in ["before", "after"] {
    let path = main.clone();
    let syntax = session
      .request(RequestScope::Document(main.clone()), move |app, _| {
        assert!(app.workspace().analysis().is_none());
        assert_eq!(app.workspace().documents().count(), 1);
        assert_eq!(app.failures(), []);
        let doc = app
          .workspace()
          .document(app.document_id(&path).unwrap())
          .unwrap();
        assert!(
          doc
            .symbols()
            .iter()
            .any(|symbol| symbol.name == name.as_bytes())
        );
        Ok(std::ptr::from_ref(doc).addr())
      })
      .await
      .unwrap();
    session.start_index().unwrap();
    let path = main.clone();
    let index = session
      .request(RequestScope::Workspace, move |app, _| {
        assert!(app.workspace().analysis().is_none());
        let doc = app
          .workspace()
          .document(app.document_id(&path).unwrap())
          .unwrap();
        assert!(
          doc
            .symbols()
            .iter()
            .any(|symbol| symbol.name == name.as_bytes())
        );
        Ok(std::ptr::from_ref(doc).addr())
      })
      .await
      .unwrap();
    assert_eq!(
      syntax, index,
      "document and index must reuse the same parsed document"
    );
    session
      .set_overlay(&main, Some("after=2\n".into()))
      .unwrap();
  }
}

#[tokio::test]
async fn references_and_rename_share_name_collection_without_blocking_document_analysis()
 {
  let fixture = Fixture::new();
  let path = fixture.write("main.sh", "x=1; : $x");
  let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
  let permit = session.index_worker.clone().acquire_owned().await.unwrap();
  let request_path = path.clone();
  let names = tokio::spawn(session.request(
    RequestScope::Names(path.clone()),
    move |app, cancelled| {
      assert_eq!(app.references(&request_path, 0, false, cancelled)?.len(), 1);
      Ok(Arc::as_ptr(app.names.as_ref().unwrap()).addr())
    },
  ));
  let document_path = path.clone();
  let tokens = session
    .request(
      RequestScope::Analysis(path.clone()),
      move |app, cancelled| {
        assert!(app.names.is_none());
        app.classify(app.document_id(&document_path).unwrap(), cancelled)
      },
    )
    .await
    .unwrap();
  assert_ne!(tokens, []);
  drop(permit);
  let first = names.await.unwrap().unwrap();
  let request_path = path.clone();
  let prepare = session
    .request(RequestScope::Names(path.clone()), move |app, cancelled| {
      app.prepare_rename(&request_path, 0, cancelled)?;
      Ok(Arc::as_ptr(app.names.as_ref().unwrap()).addr())
    })
    .await
    .unwrap();
  assert_eq!(first, prepare);
  let second = session
    .request(RequestScope::Names(path.clone()), move |app, cancelled| {
      assert_eq!(app.rename(&path, 0, "x", cancelled)?, []);
      Ok(Arc::as_ptr(app.names.as_ref().unwrap()).addr())
    })
    .await
    .unwrap();
  assert_eq!(first, second);
}

#[tokio::test]
async fn cancelling_one_reference_request_preserves_shared_work_and_updates_supersede_old_results()
 {
  let fixture = Fixture::new();
  let path = fixture.write("main.sh", "x=1; : $x");
  let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
  let scope = RequestScope::Names(path.clone());
  let permit = session.index_worker.clone().acquire_owned().await.unwrap();
  let first_path = path.clone();
  let abandoned = session.request(scope.clone(), move |app, cancelled| {
    app.references(&first_path, 0, false, cancelled)
  });
  let second_path = path.clone();
  let remaining = session.request(scope.clone(), move |app, cancelled| {
    app.references(&second_path, 0, false, cancelled)
  });
  let preparation = lock(&session.state).unwrap().preparations[&scope].clone();
  drop(abandoned);
  assert!(!preparation.cancelled.load(Ordering::Acquire));
  drop(permit);
  assert_eq!(remaining.await.unwrap().len(), 1);
  let obsolete = session.request::<()>(scope.clone(), |_, _| {
    panic!("superseded reference request must not run")
  });
  session.set_overlay(&path, Some("x=2".into())).unwrap();
  assert!(matches!(obsolete.await, Err(Error::Superseded)));
  let current = session
    .request(scope, move |app, cancelled| {
      app.references(&path, 0, false, cancelled)
    })
    .await
    .unwrap();
  assert_eq!(current, []);
}

#[tokio::test]
async fn definition_positions_are_not_reused_after_an_unnotified_document_change()
 {
  let fixture = Fixture::new();
  let path = fixture.write("main.sh", "x=a; f() { :; }; f\n");
  let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
  let definitions = || {
    let path = path.clone();
    session.request(
      RequestScope::Results(path.clone()),
      move |app, cancelled| {
        let id = app.document_id(&path).unwrap();
        let source = app.workspace().document(id).unwrap().source();
        let offset = source.rfind('f').unwrap();
        app.definitions(id, offset, cancelled)
      },
    )
  };
  assert_eq!(definitions().await.unwrap()[0].bytes, 5..6);
  fixture.write("main.sh", "x=é; f() { :; }; f\n");
  assert!(matches!(definitions().await, Err(Error::Superseded)));
  assert_eq!(definitions().await.unwrap()[0].bytes, 6..7);
}

#[tokio::test]
async fn reused_results_keep_the_newer_shared_analysis_and_its_read_failures() {
  let fixture = Fixture::new();
  let main = fixture.write("main.sh", "x=1; : $x\n");
  let other = fixture.write("other.sh", "y=1; : $y\n");
  let broken = fixture.root.join("broken.sh");
  std::fs::write(&broken, b"z=\xff\n").unwrap();
  let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
  session.add_input(&fixture.root).unwrap();
  let failures = |path: PathBuf| {
    session.request(
      RequestScope::Results(path.clone()),
      move |app, cancelled| {
        app.definitions(app.document_id(&path).unwrap(), 7, cancelled)?;
        Ok(app.failures().len())
      },
    )
  };
  assert_eq!(failures(main.clone()).await.unwrap(), 1);
  fixture.write("broken.sh", "z=1\n");
  assert_eq!(failures(other).await.unwrap(), 0);
  assert_eq!(failures(main.clone()).await.unwrap(), 0);
  let tokens = session
    .request(
      RequestScope::Analysis(main.clone()),
      move |app, cancelled| {
        app.classify(app.document_id(&main).unwrap(), cancelled)
      },
    )
    .await;
  assert!(tokens.is_ok(), "{tokens:?}");
}

#[tokio::test]
async fn disk_changes_invalidate_shared_name_collection_and_retry_with_current_inputs()
 {
  let fixture = Fixture::new();
  let path = fixture.write("main.sh", "x=1; : $x");
  let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
  let requested = path.clone();
  session
    .request(RequestScope::Names(path.clone()), move |app, cancelled| {
      app.prepare_rename(&requested, 0, cancelled)
    })
    .await
    .unwrap();
  fixture.write("main.sh", "x=1; y=2; : $x");
  let requested = path.clone();
  let changed = session
    .request(RequestScope::Names(path.clone()), move |app, cancelled| {
      app.rename(&requested, 0, "y", cancelled)
    })
    .await;
  assert!(matches!(changed, Err(Error::Superseded)));
  let conflict = session
    .request(RequestScope::Names(path.clone()), move |app, cancelled| {
      app.rename(&path, 0, "y", cancelled)
    })
    .await;
  assert!(matches!(conflict, Err(Error::RenameRefused(_))));
}

fn token_at(
  spans: &[crate::interface::application::engine::SemanticSpan],
  offset: usize,
) -> &crate::interface::application::engine::SemanticSpan {
  spans
    .iter()
    .find(|span| span.location.bytes.start == offset)
    .unwrap()
}

#[tokio::test]
async fn workspace_analysis_reclassifies_reached_calls_without_delaying_document_results()
 {
  use crate::interface::application::engine::{Modifier, SemanticKind};
  let fixture = Fixture::new();
  let source = "a() { :; }\nb() { a; command -v a; : \"$VALUE\"; }\n";
  let library = fixture.write("lib.sh", source);
  let main = fixture.write("main.sh", ". ./lib.sh\nreadonly VALUE\nb\n");
  let other = fixture.write(
    "other.sh",
    ". ./lib.sh\na() { echo replaced; }\nreadonly VALUE\nb\n",
  );
  let mut settings = Settings::new(fixture.root.clone());
  settings
    .initial
    .environment
    .insert(b"VALUE".to_vec(), b"initial".to_vec());
  let session = Session::new(settings).unwrap();
  session.add_input(&fixture.root).unwrap();
  let mut updates = session.updates().unwrap();
  let permit = session
    .analysis_worker
    .clone()
    .acquire_owned()
    .await
    .unwrap();
  session.start_index().unwrap();
  let classify = || {
    let path = library.clone();
    session.request(
      RequestScope::Analysis(path.clone()),
      move |app, cancelled| {
        app.classify(app.document_id(&path).unwrap(), cancelled)
      },
    )
  };
  let call = source.find("a;").unwrap();
  let operand = source.find("a; :").unwrap();
  let variable = source.find("VALUE").unwrap();
  let initial = classify().await.unwrap();
  assert_eq!(token_at(&initial, call).kind, SemanticKind::Command);
  assert!(!updates.has_changed().unwrap());
  drop(permit);
  updates.changed().await.unwrap();
  let complete = classify().await.unwrap();
  for position in [call, operand] {
    assert_eq!(token_at(&complete, position).kind, SemanticKind::Function);
  }
  let value = token_at(&complete, variable);
  assert!(value.modifiers.contains(&Modifier::Environment));
  assert!(value.modifiers.contains(&Modifier::Readonly));

  session
    .set_overlay(
      &other,
      Some(". ./lib.sh\nunset -f a\nVALUE=changed\nb\n".into()),
    )
    .unwrap();
  let permit = session
    .analysis_worker
    .clone()
    .acquire_owned()
    .await
    .unwrap();
  session.start_index().unwrap();
  let pending = classify().await.unwrap();
  assert_eq!(token_at(&pending, call).kind, SemanticKind::Command);
  drop(permit);
  updates.changed().await.unwrap();
  let conflicting = classify().await.unwrap();
  for position in [call, operand] {
    assert_eq!(token_at(&conflicting, position).kind, SemanticKind::Command);
  }
  let value = token_at(&conflicting, variable);
  assert!(!value.modifiers.contains(&Modifier::Environment));
  assert!(!value.modifiers.contains(&Modifier::Readonly));

  session
    .set_overlay(&other, Some(". ./lib.sh\n".into()))
    .unwrap();
  session.start_index().unwrap();
  updates.changed().await.unwrap();
  assert_eq!(
    token_at(&classify().await.unwrap(), call).kind,
    SemanticKind::Function
  );
  std::fs::remove_file(&main).unwrap();
  session.files_changed(&[main]).unwrap();
  session.start_index().unwrap();
  updates.changed().await.unwrap();
  assert_eq!(
    token_at(&classify().await.unwrap(), call).kind,
    SemanticKind::Command
  );
}

#[tokio::test]
async fn workspace_semantics_reject_unnotified_changes_and_superseded_background_results()
 {
  use crate::interface::application::engine::SemanticKind;
  let fixture = Fixture::new();
  let library = fixture.write("lib.sh", "a() { :; }; b() { a; }");
  let main = fixture.write("main.sh", ". ./lib.sh; b");
  let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
  session.add_input(&fixture.root).unwrap();
  let mut updates = session.updates().unwrap();
  session.start_index().unwrap();
  updates.changed().await.unwrap();
  fixture.write("main.sh", ". ./lib.sh; unset -f a; b");
  let path = library.clone();
  let stale = session
    .request(RequestScope::Analysis(path.clone()), move |app, flag| {
      app.classify(app.document_id(&path).unwrap(), flag)
    })
    .await;
  assert!(matches!(stale, Err(Error::Superseded)));
  let permit = session
    .analysis_worker
    .clone()
    .acquire_owned()
    .await
    .unwrap();
  session.start_index().unwrap();
  let old = lock(&session.state).unwrap().preparations
    [&RequestScope::WorkspaceAnalysis]
    .clone();
  session
    .set_overlay(&main, Some(". ./lib.sh; b".into()))
    .unwrap();
  assert!(old.cancelled.load(Ordering::Relaxed));
  session.start_index().unwrap();
  drop(permit);
  updates.changed().await.unwrap();
  let tokens = session
    .request(RequestScope::Analysis(library.clone()), move |app, flag| {
      app.classify(app.document_id(&library).unwrap(), flag)
    })
    .await
    .unwrap();
  assert_eq!(
    tokens
      .iter()
      .find(|span| span.location.bytes == (18..19))
      .unwrap()
      .kind,
    SemanticKind::Function
  );
}

#[tokio::test]
async fn rename_reanalyzes_unchanged_entries_when_their_source_dependency_is_edited()
 {
  let fixture = Fixture::new();
  let library = fixture.write("lib.sh", "value=1; : \"$value\"");
  let main = fixture.write("main.sh", ". ./lib.sh");
  let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
  session.add_input(&main).unwrap();
  let edits = session
    .request(RequestScope::Names(library.clone()), move |app, flag| {
      app.rename(&library, 0, "renamed", flag)
    })
    .await
    .unwrap();
  assert_eq!(edits.len(), 2);
  assert!(edits.iter().all(|edit| edit.new_text == "renamed"));
}
