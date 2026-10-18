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
  for expected in [first, second] {
    let document = path.clone();
    let tokens = session.request(
      RequestScope::Analysis(path.clone()),
      move |application, cancelled| {
        let id = application.document_id(&document).unwrap();
        let tokens = application.classify(id, cancelled)?;
        let analysis =
          std::ptr::from_ref(application.workspace().analysis().unwrap())
            .addr();
        Ok((tokens, analysis))
      },
    );
    let document = path.clone();
    let links = session.request(
      RequestScope::Analysis(path.clone()),
      move |application, cancelled| {
        let id = application.document_id(&document).unwrap();
        let links = application.document_links(id, cancelled)?;
        let analysis =
          std::ptr::from_ref(application.workspace().analysis().unwrap())
            .addr();
        Ok((links, analysis))
      },
    );
    let document = path.clone();
    let definitions = session.request(
      RequestScope::Analysis(path.clone()),
      move |application, cancelled| {
        let id = application.document_id(&document).unwrap();
        let source = application.workspace().document(id).unwrap().source();
        let offset = source.rfind("value").unwrap();
        let definitions = application.definitions(id, offset, cancelled)?;
        let analysis =
          std::ptr::from_ref(application.workspace().analysis().unwrap())
            .addr();
        Ok((definitions, analysis))
      },
    );
    let (tokens, links, definitions) = tokio::join!(tokens, links, definitions);
    let (definitions, definitions_analysis) = definitions.unwrap();
    assert_eq!(definitions.len(), 1);

    let (tokens, tokens_analysis) = tokens.unwrap();
    let (links, links_analysis) = links.unwrap();
    assert_ne!(tokens, []);
    assert_eq!(tokens_analysis, links_analysis);
    assert_eq!(tokens_analysis, definitions_analysis);
    assert_eq!(links.len(), 2);
    assert_eq!(links[1].1, expected);
    fixture.write("lib.sh", "target=./second.sh\n");
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
async fn rename_requests_share_name_collection_without_blocking_document_analysis()
 {
  let fixture = Fixture::new();
  let path = fixture.write("main.sh", "x=1; : $x");
  let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
  let permit = session.index_worker.clone().acquire_owned().await.unwrap();
  let request_path = path.clone();
  let names = tokio::spawn(session.request(
    RequestScope::Names(path.clone()),
    move |app, cancelled| {
      app.prepare_rename(&request_path, 0, cancelled)?;
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
