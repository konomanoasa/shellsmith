mod document {
  use shellsmith::interface::application::engine::{
    Document, DocumentId, Error, Location, Revision, Snapshot,
  };
  use std::sync::atomic::AtomicBool;
  use std::sync::atomic::Ordering;
  use std::thread;
  use std::time::Duration;

  #[test]
  fn locations_belong_to_one_document_revision_and_preserve_utf8_and_bom() {
    let snapshot = Snapshot {
      document: DocumentId(1),
      revision: Revision(2),
    };
    let document = Document::parse(
      snapshot,
      "\u{feff}x=あ\n".as_bytes(),
      &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(document.source(), "\u{feff}x=あ\n");
    assert!(!document.has_syntax_errors());
    let mut location = Location {
      snapshot,
      bytes: 5..8,
    };
    assert_eq!(document.text(&location), Some("あ"));
    location.bytes = 5..6;
    assert_eq!(document.text(&location), None);
    location.bytes = 0..3;
    location.snapshot.revision = Revision(1);
    assert_eq!(document.text(&location), None);
    location.snapshot = Snapshot {
      document: DocumentId(2),
      revision: Revision(2),
    };
    assert_eq!(document.text(&location), None);
  }

  #[test]
  fn syntax_recovery_is_distinct_from_invalid_input_and_cancellation() {
    let snapshot = Snapshot {
      document: DocumentId(1),
      revision: Revision(1),
    };
    let cancelled = AtomicBool::new(false);
    let document = Document::parse(snapshot, b"echo ${x", &cancelled).unwrap();
    assert!(document.has_syntax_errors());
    assert_eq!(
      Document::parse(snapshot, b"x=\xff", &cancelled).unwrap_err(),
      Error::InvalidEncoding { byte: 2 }
    );
    assert_eq!(
      Document::parse(snapshot, b"x=1", &AtomicBool::new(true)).unwrap_err(),
      Error::Cancelled
    );
    assert!(
      !Document::parse(snapshot, b"", &cancelled)
        .unwrap()
        .has_syntax_errors()
    );
  }

  #[test]
  fn cancellation_during_large_document_analysis_returns_no_partial_document() {
    let snapshot = Snapshot {
      document: DocumentId(1),
      revision: Revision(1),
    };
    let source = "x=1; echo \"$x\"\n".repeat(80_000);
    let cancelled = AtomicBool::new(false);
    thread::scope(|scope| {
      scope.spawn(|| {
        thread::sleep(Duration::from_millis(5));
        cancelled.store(true, Ordering::Relaxed);
      });
      assert_eq!(
        Document::parse(snapshot, source.as_bytes(), &cancelled).unwrap_err(),
        Error::Cancelled
      );
    });
  }
}

mod extraction {
  use crate::support;

  use shellsmith::interface::application::engine::{
    Access, OccurrenceKind, SymbolKind, Value, ValueIssue,
  };
  use support::parse;

  #[test]
  fn assignments_and_reads_remain_distinct_without_name_resolution() {
    let document = parse(1, 1, "x=1; echo \"$x\"; x=2");
    let occurrences: Vec<_> = document
      .occurrences()
      .iter()
      .map(|site| {
        (
          site.name.clone(),
          site.kind,
          site.access,
          site.location.bytes.clone(),
        )
      })
      .collect();
    assert_eq!(
      occurrences,
      vec![
        (
          Value::Literal(b"x".to_vec()),
          OccurrenceKind::Assignment,
          Access::Write,
          0..1
        ),
        (
          Value::Literal(b"echo".to_vec()),
          OccurrenceKind::Command,
          Access::Read,
          5..9
        ),
        (
          Value::Literal(b"x".to_vec()),
          OccurrenceKind::VariableExpansion,
          Access::Read,
          12..13
        ),
        (
          Value::Literal(b"x".to_vec()),
          OccurrenceKind::Assignment,
          Access::Write,
          16..17
        ),
      ]
    );
    assert_eq!(document.symbols().len(), 2);
    assert_ne!(document.symbols()[0].id, document.symbols()[1].id);
    let other = parse(2, 1, "x=1");
    let next = parse(1, 2, "x=1");
    assert!(other.symbol(document.symbols()[0].id).is_none());
    assert!(next.occurrence(document.occurrences()[0].id).is_none());
  }

  #[test]
  fn function_loop_declaration_parameter_and_arithmetic_roles_follow_the_cst() {
    let document = parse(
      1,
      2,
      "f() { :; }\nfor i in a; do export x=$i; done\n: ${y:=z} $1 $? $((x=1)) $((y+=x))\n",
    );
    assert!(!document.has_syntax_errors());
    let sites: Vec<_> = document
      .occurrences()
      .iter()
      .filter(|site| site.kind != OccurrenceKind::Command)
      .map(|site| {
        (
          document.text(&site.location).unwrap(),
          site.kind,
          site.access,
        )
      })
      .collect();
    assert_eq!(
      sites,
      vec![
        ("f", OccurrenceKind::FunctionDefinition, Access::Definition),
        ("i", OccurrenceKind::ForVariable, Access::Write),
        ("x", OccurrenceKind::DeclarationAssignment, Access::Write),
        ("i", OccurrenceKind::VariableExpansion, Access::Read),
        (
          "y",
          OccurrenceKind::VariableExpansion,
          Access::ReadConditionalWrite
        ),
        ("1", OccurrenceKind::PositionalParameter, Access::Read),
        ("?", OccurrenceKind::SpecialParameter, Access::Read),
        ("x", OccurrenceKind::Arithmetic, Access::Write),
        ("y", OccurrenceKind::Arithmetic, Access::ReadWrite),
        ("x", OccurrenceKind::Arithmetic, Access::Read),
      ]
    );
    assert_eq!(document.symbols()[0].kind, SymbolKind::Function);
    for kind in [
      "function_definition",
      "brace_group",
      "for_clause",
      "do_group",
    ] {
      assert!(
        document
          .contexts()
          .iter()
          .any(|context| context.kind == kind)
      );
    }
  }

  #[test]
  fn names_decode_static_quotes_and_continuations_without_evaluating_expansions()
   {
    let document = parse(
      1,
      1,
      "fo\\\no() { :; }\nfo\\\no\n\"fo\"'o'\nf\\oo\n$'\\146oo'\n$cmd\nf*\n\"a\\q\"\n",
    );
    assert!(!document.has_syntax_errors());
    let names: Vec<_> = document
      .occurrences()
      .iter()
      .map(|site| site.name.clone())
      .collect();
    assert_eq!(
      names,
      vec![
        Value::Literal(b"foo".to_vec()),
        Value::Literal(b":".to_vec()),
        Value::Literal(b"foo".to_vec()),
        Value::Literal(b"foo".to_vec()),
        Value::Literal(b"foo".to_vec()),
        Value::Literal(b"foo".to_vec()),
        Value::Unknown(ValueIssue::Dynamic),
        Value::Literal(b"cmd".to_vec()),
        Value::Unknown(ValueIssue::PathnameExpansion),
        Value::Literal(b"a\\q".to_vec()),
      ]
    );
    assert_eq!(
      document.text(&document.occurrences()[0].location),
      Some("fo\\\no")
    );
  }

  #[test]
  fn inert_source_and_dynamic_arithmetic_do_not_invent_variable_uses_or_writes()
  {
    let document = parse(
      1,
      1,
      "cat <<'$not_used'\nbody\n$not_used\n: ~${not_used}\necho \"$((x $operator y))\"\n",
    );
    assert!(!document.has_syntax_errors());
    assert!(
      !document
        .occurrences()
        .iter()
        .any(|site| site.name == Value::Literal(b"not_used".to_vec()))
    );
    let arithmetic: Vec<_> = document
      .occurrences()
      .iter()
      .filter(|site| site.kind == OccurrenceKind::Arithmetic)
      .map(|site| site.access)
      .collect();
    assert_eq!(arithmetic, vec![Access::Unknown, Access::Unknown]);
    assert_eq!(document.symbols(), []);
  }

  #[test]
  fn transformed_operators_do_not_guess_reads_or_writes_from_undecoded_source()
  {
    let document = parse(
      1,
      1,
      "echo `: ${target:\\\\\n=value} $((count+\\\\\n=1)) ${known:\\\n=1} $((total=1))`\n",
    );
    assert!(!document.has_syntax_errors());
    let accesses: Vec<_> = document
      .occurrences()
      .iter()
      .filter(|site| site.kind != OccurrenceKind::Command)
      .map(|site| (site.name.clone(), site.access))
      .collect();
    assert_eq!(
      accesses,
      vec![
        (Value::Literal(b"target".to_vec()), Access::Unknown),
        (Value::Literal(b"count".to_vec()), Access::Unknown),
        (
          Value::Literal(b"known".to_vec()),
          Access::ReadConditionalWrite
        ),
        (Value::Literal(b"total".to_vec()), Access::Write),
      ]
    );
    assert_eq!(
      document
        .symbols()
        .iter()
        .map(|symbol| symbol.name.as_slice())
        .collect::<Vec<_>>(),
      vec![b"known".as_slice(), b"total"]
    );
  }

  #[test]
  fn classifications_use_parser_queries_independently_of_symbol_resolution() {
    let document = parse(1, 1, "# hello\nif true; then echo 'text'; fi\n");
    let spans: Vec<_> = document
      .classifications()
      .iter()
      .map(|span| (document.text(&span.location).unwrap(), span.capture))
      .collect();
    for expected in [
      ("# hello", "comment"),
      ("if", "keyword"),
      ("echo", "function.call"),
      ("text", "string"),
    ] {
      assert!(spans.contains(&expected), "{expected:?}: {spans:?}");
    }
    assert_eq!(document.symbols(), []);
  }

  #[test]
  fn damaged_and_transformed_source_keeps_uncertainty_and_recovers_after_repair()
   {
    let damaged = parse(1, 1, "x=1\necho ${unfinished");
    assert!(damaged.has_syntax_errors());
    assert!(
      !damaged
        .symbols()
        .iter()
        .any(|symbol| symbol.name == b"unfinished")
    );
    let repaired = parse(1, 2, "x=1\necho ${unfinished}\n");
    assert!(!repaired.has_syntax_errors());
    assert!(
      repaired
        .occurrences()
        .iter()
        .any(|site| site.name == Value::Literal(b"unfinished".to_vec()))
    );
    let backquote = parse(2, 1, "echo `e\\\\cho $x`\n");
    assert!(
      backquote
        .occurrences()
        .iter()
        .any(|site| site.name == Value::Unknown(ValueIssue::TransformedSource))
    );
  }
}

mod index {
  use crate::support;

  use shellsmith::interface::application::engine::{
    DocumentId, OccurrenceKind, SymbolKind, Workspace,
  };
  use support::parse;

  #[test]
  fn index_preserves_namespaces_multiple_definitions_and_document_identity() {
    let mut workspace = Workspace::default();
    workspace
      .insert(parse(2, 1, "x=1\nx() { :; }\nx=2\nx\n"))
      .unwrap();
    workspace.insert(parse(1, 1, "x=3\nxyz=4\n")).unwrap();
    let definitions: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| {
        (
          symbol.id.snapshot.document,
          symbol.kind,
          workspace
            .occurrence(symbol.definition)
            .unwrap()
            .location
            .bytes
            .clone(),
        )
      })
      .collect();
    assert_eq!(
      definitions,
      vec![
        (DocumentId(1), SymbolKind::Variable, 0..1),
        (DocumentId(2), SymbolKind::Variable, 0..1),
        (DocumentId(2), SymbolKind::Function, 4..5),
        (DocumentId(2), SymbolKind::Variable, 15..16),
      ]
    );
    assert_eq!(workspace.symbols().count(), 5);
    assert_eq!(workspace.symbols_named(b"missing").count(), 0);
    assert_eq!(
      workspace
        .search_symbols(b"xy")
        .map(|symbol| symbol.name.as_slice())
        .collect::<Vec<_>>(),
      vec![b"xyz"]
    );
    assert_eq!(workspace.search_symbols(b"").count(), 5);
    let call = workspace
      .document(DocumentId(2))
      .unwrap()
      .occurrences()
      .last()
      .unwrap();
    assert_eq!(call.kind, OccurrenceKind::Command);
  }

  #[test]
  fn replacing_and_removing_documents_removes_only_their_definitions() {
    let mut workspace = Workspace::default();
    let first = parse(1, 1, "same=1\n");
    let old = first.symbols()[0].id;
    workspace.insert(first).unwrap();
    workspace.insert(parse(2, 1, "same=2\n")).unwrap();
    workspace.insert(parse(1, 2, "new=1\n")).unwrap();
    assert!(workspace.symbol(old).is_none());
    assert_eq!(
      workspace
        .symbols_named(b"same")
        .map(|symbol| symbol.id.snapshot.document)
        .collect::<Vec<_>>(),
      vec![DocumentId(2)]
    );
    let snapshot = workspace.document(DocumentId(2)).unwrap().snapshot();
    workspace.remove(snapshot).unwrap();
    assert_eq!(workspace.symbols_named(b"same").count(), 0);
    assert_eq!(
      workspace
        .symbols()
        .map(|symbol| symbol.name.as_slice())
        .collect::<Vec<_>>(),
      vec![b"new"]
    );
  }
}

mod dependencies {
  use crate::support;

  use shellsmith::interface::application::engine::{
    DependencyState, DocumentId, Error, Snapshot, Value, ValueIssue, Workspace,
  };
  use support::parse;

  #[test]
  fn dot_operands_preserve_static_dynamic_missing_empty_and_nested_occurrences()
  {
    let document = parse(
      1,
      1,
      ". >log './lib.sh'\n\".\" ./lib.sh\nf() { . \"$dir/lib\"; }\n.\n. ''\n. *.sh\n. $'\\0'\necho '. lib.sh'\n",
    );
    assert!(!document.has_syntax_errors());
    let values: Vec<_> = document
      .references()
      .iter()
      .map(|reference| reference.value.clone())
      .collect();
    assert_eq!(
      values,
      vec![
        Value::Literal(b"./lib.sh".to_vec()),
        Value::Literal(b"./lib.sh".to_vec()),
        Value::Unknown(ValueIssue::Dynamic),
        Value::Unknown(ValueIssue::MissingOperand),
        Value::Unknown(ValueIssue::EmptyOperand),
        Value::Unknown(ValueIssue::PathnameExpansion),
        Value::Unknown(ValueIssue::Nul),
      ]
    );
    assert_ne!(document.references()[0].id, document.references()[1].id);
    assert_eq!(
      document.text(document.references()[0].operand.as_ref().unwrap()),
      Some("'./lib.sh'")
    );
    assert!(document.references()[3].operand.is_none());
    let mut owner = Some(document.references()[2].context);
    let mut contexts = Vec::new();
    while let Some(id) = owner {
      let context = document.context(id).unwrap();
      contexts.push(context.kind.as_str());
      owner = context.parent;
    }
    assert!(contexts.contains(&"function_definition"));
  }

  #[test]
  fn repeated_edges_and_cycles_remain_distinct_and_traversal_terminates() {
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
    assert_eq!(
      workspace.reachable(DocumentId(1)),
      vec![DocumentId(1), DocumentId(2)]
    );
    assert_eq!(workspace.dependencies(DocumentId(1)).count(), 2);
    workspace
      .set_reference(first, DependencyState::Failed("permission denied".into()))
      .unwrap();
    assert_eq!(workspace.dependents(DocumentId(2)), vec![second]);
    assert_eq!(
      workspace.reference_state(first),
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
      workspace
        .set_reference(current, DependencyState::Resolved(child_snapshot)),
      Err(Error::StaleSnapshot(child_snapshot))
    );
    assert_eq!(
      workspace.reference_state(current),
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
      workspace.reference_state(id),
      Some(&DependencyState::Unresolved("Dynamic".into()))
    );
    assert_eq!(workspace.reachable(DocumentId(99)), []);
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
    assert_eq!(workspace.reachable(DocumentId(1)), vec![DocumentId(1)]);
    assert_eq!(
      workspace.insert(parse(1, 2, ". self\n")).unwrap().affected,
      vec![DocumentId(1)]
    );
    assert_eq!(workspace.reference_state(reference), None);
    assert_eq!(workspace.dependents(DocumentId(1)), []);
    let current = workspace.document(DocumentId(1)).unwrap().references()[0].id;
    assert_eq!(
      workspace.reference_state(current),
      Some(&DependencyState::Pending)
    );
  }
}

mod updates {
  use crate::support;

  use shellsmith::interface::application::engine::{
    DependencyState, DocumentId, Error, Revision, Snapshot, Workspace,
  };
  use std::sync::atomic::AtomicBool;
  use support::parse;

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
        workspace.reference_state(*id),
        Some(&DependencyState::Invalidated(DocumentId(3)))
      );
    }
    assert_eq!(
      workspace.reference_state(root_reference),
      Some(&DependencyState::Resolved(middle_snapshot))
    );
    assert_eq!(
      workspace.reachable(DocumentId(1)),
      vec![DocumentId(1), DocumentId(2), DocumentId(3)]
    );

    let change = workspace.remove(middle_snapshot).unwrap();
    assert_eq!(change.affected, vec![DocumentId(1), DocumentId(2)]);
    assert_eq!(
      workspace.reference_state(root_reference),
      Some(&DependencyState::Invalidated(DocumentId(2)))
    );
    for id in middle_references {
      assert_eq!(workspace.reference_state(id), None);
    }
    assert_eq!(workspace.dependents(DocumentId(3)), []);
    assert_eq!(workspace.dependencies(DocumentId(1)).count(), 1);
    assert_eq!(workspace.reachable(DocumentId(1)), vec![DocumentId(1)]);

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
    assert_eq!(
      workspace.reachable(DocumentId(1)),
      vec![DocumentId(1), DocumentId(2)]
    );
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
      workspace.update(future, b"\xff", &AtomicBool::new(false)),
      Err(Error::InvalidEncoding { byte: 0 })
    );
    assert_eq!(
      workspace.update(future, b"x=3", &AtomicBool::new(true)),
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
      workspace.reference_state(reference),
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
    assert_eq!(updated.reference_state(old_reference), None);
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
      assert_eq!(updated.reachable(id), fresh.reachable(id));
    }
    assert_eq!(
      updated.search_symbols(b"").collect::<Vec<_>>(),
      fresh.search_symbols(b"").collect::<Vec<_>>()
    );
    assert_eq!(updated.symbols_named(b"old").count(), 0);
    assert_eq!(updated.symbols_named(b"obsolete").count(), 0);
  }
}
