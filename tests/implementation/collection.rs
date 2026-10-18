use super::super::fixtures::Fixture;
use super::*;

#[tokio::test]
async fn workspace_and_rename_requests_share_the_completed_collection() {
  use super::super::{Application, RequestScope, Session, Settings};

  fn snapshot(app: &Application) -> Arc<Root> {
    app
      .collection
      .state
      .lock()
      .unwrap()
      .roots
      .values()
      .next()
      .unwrap()
      .data
      .clone()
      .unwrap()
  }

  let fixture = Fixture::new();
  let path = fixture.write("main.sh", "x=1; : $x");
  let session = Session::new(Settings::new(fixture.root.clone())).unwrap();
  session.add_input(&fixture.root).unwrap();
  session.set_watching(true).unwrap();
  let workspace =
    session.request(RequestScope::Workspace, |app, _| Ok(snapshot(app)));
  let names = session.request(RequestScope::Names(path.clone()), |app, _| {
    Ok(snapshot(app))
  });
  let (workspace, names) = tokio::join!(workspace, names);
  let workspace = workspace.unwrap();
  assert!(Arc::ptr_eq(&workspace, &names.unwrap()));
  let renamed = session
    .request(RequestScope::Names(path.clone()), move |app, cancel| {
      assert_eq!(app.rename(&path, 0, "value", cancel)?.len(), 2);
      Ok(snapshot(app))
    })
    .await
    .unwrap();
  assert!(Arc::ptr_eq(&workspace, &renamed));
}

#[test]
fn collection_reuses_completed_scans_and_refreshes_only_notified_subtrees() {
  let fixture = Fixture::new();
  let first = fixture.write("first/main.sh", "first=1\n");
  let second = fixture.write("second/main.sh", "second=1\n");
  let collection = Collection::default();
  collection.watching(true);
  let roots = BTreeSet::from([fixture.root.clone()]);
  let cancel = AtomicBool::new(false);
  assert_eq!(
    collection.collect(&roots, false, &cancel).unwrap(),
    (BTreeSet::from([first.clone(), second.clone()]), vec![])
  );
  let initial = collection.state.lock().unwrap().roots[&fixture.root]
    .data
    .clone()
    .unwrap();
  collection.collect(&roots, true, &cancel).unwrap();
  assert!(Arc::ptr_eq(
    &initial,
    collection.state.lock().unwrap().roots[&fixture.root]
      .data
      .as_ref()
      .unwrap()
  ));
  let later = fixture.write("second/later.sh", "later=1\n");
  fixture.write("first/main.sh", "#!/bin/bash\nfirst=2\n");
  collection.changed(&[first]);
  assert_eq!(
    collection.collect(&roots, false, &cancel).unwrap(),
    (BTreeSet::from([second.clone()]), vec![])
  );
  collection.changed(&[fixture.root.join("second")]);
  assert_eq!(
    collection.collect(&roots, false, &cancel).unwrap(),
    (BTreeSet::from([second, later]), vec![])
  );
}

#[test]
fn partial_collection_keeps_parent_ignore_rules_and_removes_deleted_entries() {
  let fixture = Fixture::new();
  fixture.write(".gitignore", "*.sh\n!keep.sh\nexcluded/\n");
  let keep = fixture.write("sub/keep.sh", "keep=1\n");
  let rules = fixture.write("sub/.gitignore", "!included.sh\n");
  let included = fixture.write("sub/included.sh", "included=1\n");
  let ignored = fixture.write("sub/ignored.sh", "ignored=1\n");
  let collection = Collection::default();
  let roots = BTreeSet::from([fixture.root.clone()]);
  let cancel = AtomicBool::new(false);
  collection.collect(&roots, false, &cancel).unwrap();
  fixture.write("sub/.gitignore", "!ignored.sh\n");
  collection.changed(&[rules]);
  assert_eq!(
    collection.collect(&roots, false, &cancel).unwrap(),
    (BTreeSet::from([keep.clone(), ignored]), vec![])
  );
  std::fs::remove_file(&keep).unwrap();
  collection.changed(&[keep]);
  let added = fixture.write("new/keep.sh", "new=1\n");
  fixture.write("new/no.sh", "wrong=1\n");
  fixture.write("excluded/keep.sh", "wrong=1\n");
  collection
    .changed(&[fixture.root.join("new"), fixture.root.join("excluded")]);
  let (paths, failures) = collection.collect(&roots, false, &cancel).unwrap();
  assert_eq!(
    paths,
    BTreeSet::from([fixture.root.join("sub/ignored.sh"), added])
  );
  assert_eq!(failures, []);
  assert!(!paths.contains(&included));
}

#[test]
fn unmonitored_verification_rescans_and_cancelled_collection_keeps_previous_results()
 {
  let fixture = Fixture::new();
  let first = fixture.write("first.sh", "first=1\n");
  let collection = Collection::default();
  let roots = BTreeSet::from([fixture.root.clone()]);
  let cancel = AtomicBool::new(false);
  collection.collect(&roots, false, &cancel).unwrap();
  let previous = collection.state.lock().unwrap().roots[&fixture.root]
    .data
    .clone()
    .unwrap();
  let second = fixture.write("second.sh", "second=1\n");
  assert!(
    collection
      .collect(&roots, true, &AtomicBool::new(true))
      .is_err()
  );
  assert!(Arc::ptr_eq(
    &previous,
    collection.state.lock().unwrap().roots[&fixture.root]
      .data
      .as_ref()
      .unwrap()
  ));
  assert_eq!(
    collection.collect(&roots, true, &cancel).unwrap(),
    (BTreeSet::from([first, second]), vec![])
  );
}

#[cfg(unix)]
#[test]
fn linked_directory_changes_update_the_collected_alias_and_retargeting_discards_old_paths()
 {
  let fixture = Fixture::new();
  let external = Fixture::new();
  let original = external.write("original/main.sh", "original=1\n");
  let replacement = external.write("replacement/main.sh", "replacement=1\n");
  let alias = fixture.root.join("library");
  std::os::unix::fs::symlink(original.parent().unwrap(), &alias).unwrap();
  let collection = Collection::default();
  let roots = BTreeSet::from([fixture.root.clone()]);
  let cancel = AtomicBool::new(false);
  assert_eq!(
    collection.collect(&roots, false, &cancel).unwrap(),
    (BTreeSet::from([original.clone()]), vec![])
  );
  let added = external.write("original/added.sh", "added=1\n");
  collection.changed(std::slice::from_ref(&added));
  assert_eq!(
    collection.collect(&roots, false, &cancel).unwrap(),
    (BTreeSet::from([original, added]), vec![])
  );
  std::fs::remove_file(&alias).unwrap();
  std::os::unix::fs::symlink(replacement.parent().unwrap(), &alias).unwrap();
  collection.changed(&[alias]);
  assert_eq!(
    collection.collect(&roots, false, &cancel).unwrap(),
    (BTreeSet::from([replacement]), vec![])
  );
}

#[cfg(unix)]
#[test]
fn physical_target_changes_reconsider_unselected_links_and_deleted_parent_directories()
 {
  let fixture = Fixture::new();
  let external = Fixture::new();
  let target = external.write("files/runner", "text\n");
  let library = external.write("libraries/one/lib.sh", "library=1\n");
  std::os::unix::fs::symlink(&target, fixture.root.join("runner")).unwrap();
  std::os::unix::fs::symlink(
    library.parent().unwrap(),
    fixture.root.join("library"),
  )
  .unwrap();
  let collection = Collection::default();
  let roots = BTreeSet::from([fixture.root.clone()]);
  let cancel = AtomicBool::new(false);
  assert_eq!(
    collection.collect(&roots, false, &cancel).unwrap(),
    (BTreeSet::from([library.clone()]), vec![])
  );
  external.write("files/runner", "#!/bin/sh\nrunner=1\n");
  collection.changed(std::slice::from_ref(&target));
  assert_eq!(
    collection.collect(&roots, false, &cancel).unwrap(),
    (BTreeSet::from([library, target]), vec![])
  );
  let removed = external.root.join("libraries");
  std::fs::remove_dir_all(&removed).unwrap();
  collection.changed(&[removed]);
  let (paths, failures) = collection.collect(&roots, false, &cancel).unwrap();
  assert_eq!(paths, BTreeSet::from([external.root.join("files/runner")]));
  assert_eq!(failures.len(), 1);
  assert_eq!(failures[0].path, fixture.root.join("library"));
}
