use super::*;

fn parse(id: u64, revision: u64, source: &str) -> Document {
  Document::parse(
    Snapshot {
      document: DocumentId(id),
      revision: crate::interface::application::engine::Revision(revision),
    },
    source.as_bytes(),
    &AtomicBool::new(false),
  )
  .unwrap()
}

#[test]
fn repeated_edges_and_cycles_keep_reverse_dependencies_current() {
  let root = parse(1, 1, ". a\n. a\n");
  let child = parse(2, 1, ". root\n");
  let root_snapshot = root.snapshot();
  let child_snapshot = child.snapshot();
  let first = root.references()[0].id;
  let second = root.references()[1].id;
  let back = child.references()[0].id;
  let mut workspace = Workspace::default();
  workspace.insert(root).unwrap();
  workspace.insert(child).unwrap();
  for id in [first, second] {
    workspace
      .set_reference(id, DependencyState::Resolved(child_snapshot))
      .unwrap();
  }
  workspace
    .set_reference(back, DependencyState::Resolved(root_snapshot))
    .unwrap();
  assert_eq!(workspace.dependents(DocumentId(2)), vec![first, second]);
  assert_eq!(workspace.dependencies(DocumentId(1)).count(), 2);
  workspace
    .set_reference(first, DependencyState::Failed("permission denied".into()))
    .unwrap();
  assert_eq!(workspace.dependents(DocumentId(2)), vec![second]);
  assert_eq!(
    workspace.dependencies.get(&first),
    Some(&DependencyState::Failed("permission denied".into()))
  );
}

#[test]
fn stale_source_and_target_results_cannot_change_dependency_state() {
  let root = parse(1, 1, ". lib\n");
  let old = root.references()[0].id;
  let child = parse(2, 1, "x=1");
  let child_snapshot = child.snapshot();
  let mut workspace = Workspace::default();
  workspace.insert(root).unwrap();
  workspace.insert(child).unwrap();
  workspace.insert(parse(1, 2, ". lib\n")).unwrap();
  assert_eq!(
    workspace.set_reference(old, DependencyState::Resolved(child_snapshot)),
    Err(Error::StaleSnapshot(old.snapshot))
  );
  let current = workspace.document(DocumentId(1)).unwrap().references()[0].id;
  workspace.insert(parse(2, 2, "x=2")).unwrap();
  assert_eq!(
    workspace.set_reference(current, DependencyState::Resolved(child_snapshot)),
    Err(Error::StaleSnapshot(child_snapshot))
  );
  assert_eq!(
    workspace.dependencies.get(&current),
    Some(&DependencyState::Pending)
  );
  let absent = Snapshot {
    document: DocumentId(3),
    ..child_snapshot
  };
  assert_eq!(
    workspace.set_reference(current, DependencyState::Resolved(absent)),
    Err(Error::UnknownDocument(DocumentId(3)))
  );
  assert_eq!(workspace.dependents(DocumentId(2)), []);
}

#[test]
fn unresolved_operands_are_not_pending_path_searches() {
  let document = parse(1, 1, ". \"$file\"\n");
  let id = document.references()[0].id;
  let mut workspace = Workspace::default();
  workspace.insert(document).unwrap();
  assert_eq!(
    workspace.dependencies.get(&id),
    Some(&DependencyState::Unresolved("Dynamic".into()))
  );
}

#[test]
fn updating_a_self_reference_discards_its_old_edge_without_stale_invalidation()
{
  let root = parse(1, 1, ". self\n");
  let snapshot = root.snapshot();
  let reference = root.references()[0].id;
  let mut workspace = Workspace::default();
  workspace.insert(root).unwrap();
  workspace
    .set_reference(reference, DependencyState::Resolved(snapshot))
    .unwrap();
  assert_eq!(
    workspace.insert(parse(1, 2, ". self\n")).unwrap().affected,
    vec![DocumentId(1)]
  );
  assert_eq!(workspace.dependencies.get(&reference), None);
  assert_eq!(workspace.dependents(DocumentId(1)), []);
  let current = workspace.document(DocumentId(1)).unwrap().references()[0].id;
  assert_eq!(
    workspace.dependencies.get(&current),
    Some(&DependencyState::Pending)
  );
}

mod updates {
  use super::*;

  use crate::interface::application::engine::{
    DependencyState, DocumentId, Error, Revision, Snapshot, Workspace,
  };
  use std::sync::atomic::AtomicBool;

  #[test]
  fn target_changes_invalidate_each_reader_and_report_transitive_dependents() {
    let root = parse(1, 1, ". middle\n");
    let middle = parse(2, 1, ". leaf\n. leaf\n");
    let leaf = parse(3, 1, "leaf=1\n");
    let root_reference = root.references()[0].id;
    let middle_references: Vec<_> = middle
      .references()
      .iter()
      .map(|reference| reference.id)
      .collect();
    let middle_snapshot = middle.snapshot();
    let leaf_snapshot = leaf.snapshot();
    let mut workspace = Workspace::default();
    for document in [root, middle, leaf] {
      workspace.insert(document).unwrap();
    }
    workspace
      .set_reference(root_reference, DependencyState::Resolved(middle_snapshot))
      .unwrap();
    for id in &middle_references {
      workspace
        .set_reference(*id, DependencyState::Resolved(leaf_snapshot))
        .unwrap();
    }

    let change = workspace.insert(parse(3, 2, "leaf=2\n")).unwrap();
    assert_eq!(
      change.affected,
      vec![DocumentId(1), DocumentId(2), DocumentId(3)]
    );
    for id in &middle_references {
      assert_eq!(
        workspace.dependencies.get(id),
        Some(&DependencyState::Invalidated(DocumentId(3)))
      );
    }
    assert_eq!(
      workspace.dependencies.get(&root_reference),
      Some(&DependencyState::Resolved(middle_snapshot))
    );

    let change = workspace.remove(middle_snapshot).unwrap();
    assert_eq!(change.affected, vec![DocumentId(1), DocumentId(2)]);
    assert_eq!(
      workspace.dependencies.get(&root_reference),
      Some(&DependencyState::Invalidated(DocumentId(2)))
    );
    for id in middle_references {
      assert_eq!(workspace.dependencies.get(&id), None);
    }
    assert_eq!(workspace.dependents(DocumentId(3)), []);
    assert_eq!(workspace.dependencies(DocumentId(1)).count(), 1);

    assert_eq!(
      workspace.insert(parse(2, 1, "old=1")),
      Err(Error::StaleSnapshot(middle_snapshot))
    );
    let restored = parse(2, 2, "restored=1");
    let restored_snapshot = restored.snapshot();
    assert_eq!(
      workspace.insert(restored).unwrap().affected,
      vec![DocumentId(1), DocumentId(2)]
    );
    workspace
      .set_reference(
        root_reference,
        DependencyState::Resolved(restored_snapshot),
      )
      .unwrap();
  }

  #[test]
  fn failed_cancelled_stale_updates_and_stale_deletes_leave_committed_state_intact()
   {
    let root = parse(1, 2, "x=1\n. lib\n");
    let snapshot = root.snapshot();
    let reference = root.references()[0].id;
    let symbol = root.symbols()[0].id;
    let child = parse(2, 1, "y=2");
    let child_snapshot = child.snapshot();
    let mut workspace = Workspace::default();
    workspace.insert(root).unwrap();
    workspace.insert(child).unwrap();
    workspace
      .set_reference(reference, DependencyState::Resolved(child_snapshot))
      .unwrap();
    let future = Snapshot {
      revision: Revision(3),
      ..snapshot
    };
    assert_eq!(
      Document::parse(future, b"\xff", &AtomicBool::new(false))
        .and_then(|doc| workspace.insert(doc)),
      Err(Error::InvalidEncoding { byte: 0 })
    );
    assert_eq!(
      Document::parse(future, b"x=3", &AtomicBool::new(true))
        .and_then(|doc| workspace.insert(doc)),
      Err(Error::Cancelled)
    );
    let stale = Snapshot {
      revision: Revision(1),
      ..snapshot
    };
    assert_eq!(
      workspace.insert(parse(1, 1, "obsolete=1")),
      Err(Error::StaleSnapshot(stale))
    );
    assert_eq!(workspace.remove(stale), Err(Error::StaleSnapshot(stale)));
    assert_eq!(
      workspace.document(DocumentId(1)).unwrap().source(),
      "x=1\n. lib\n"
    );
    assert_eq!(workspace.symbol(symbol).unwrap().name, b"x");
    assert_eq!(
      workspace.dependencies.get(&reference),
      Some(&DependencyState::Resolved(child_snapshot))
    );
    assert_eq!(workspace.dependents(DocumentId(2)), vec![reference]);
  }

  #[test]
  fn updated_workspace_matches_fresh_registration_including_indexes_and_edges()
  {
    let mut updated = Workspace::default();
    updated.insert(parse(1, 1, "old=1\n. lib\n")).unwrap();
    updated.insert(parse(2, 1, "obsolete() { :; }\n")).unwrap();
    let old_reference =
      updated.document(DocumentId(1)).unwrap().references()[0].id;
    let old_target = updated.document(DocumentId(2)).unwrap().snapshot();
    updated
      .set_reference(old_reference, DependencyState::Resolved(old_target))
      .unwrap();
    updated.remove(old_target).unwrap();

    let mut fresh = Workspace::default();
    for workspace in [&mut updated, &mut fresh] {
      workspace
        .insert(parse(1, 3, "now=1\n. lib\n. \"$dynamic\"\n"))
        .unwrap();
      workspace
        .insert(parse(2, 2, "lib() { echo $now; }\n"))
        .unwrap();
      let reference =
        workspace.document(DocumentId(1)).unwrap().references()[0].id;
      let target = workspace.document(DocumentId(2)).unwrap().snapshot();
      workspace
        .set_reference(reference, DependencyState::Resolved(target))
        .unwrap();
    }
    assert_eq!(updated.dependencies.get(&old_reference), None);
    for id in [DocumentId(1), DocumentId(2)] {
      let actual = updated.document(id).unwrap();
      let expected = fresh.document(id).unwrap();
      assert_eq!(actual.contexts(), expected.contexts());
      assert_eq!(actual.occurrences(), expected.occurrences());
      assert_eq!(actual.symbols(), expected.symbols());
      assert_eq!(actual.classifications(), expected.classifications());
      assert_eq!(
        updated.dependencies(id).collect::<Vec<_>>(),
        fresh.dependencies(id).collect::<Vec<_>>()
      );
      assert_eq!(updated.dependents(id), fresh.dependents(id));
    }
    assert_eq!(
      updated.search_symbols(b"").collect::<Vec<_>>(),
      fresh.search_symbols(b"").collect::<Vec<_>>()
    );
    assert_eq!(
      updated
        .symbols()
        .filter(|symbol| symbol.name == b"old")
        .count(),
      0
    );
    assert_eq!(
      updated
        .symbols()
        .filter(|symbol| symbol.name == b"obsolete")
        .count(),
      0
    );
  }
}
