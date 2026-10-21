use super::*;
use crate::interface::application::engine::AnalysisRequest;

struct NoInputs;

impl engine::AnalysisInputs for NoInputs {
  fn source(
    &mut self,
    input: &engine::SourceInput,
  ) -> Result<engine::SourceDocument, Error> {
    panic!("Unexpected source input: {input:?}")
  }

  fn evaluate(
    &mut self,
    query: &engine::InputQuery,
  ) -> Result<engine::InputResult, Error> {
    panic!("Unexpected query: {query:?}")
  }
}

#[test]
fn projected_symbol_details_require_agreement_on_ranges_and_containers() {
  let cancel = AtomicBool::new(false);
  let snapshot = Snapshot {
    document: DocumentId(1),
    revision: Revision(1),
  };
  let mut workspace = Workspace::default();
  workspace
    .update(snapshot, b"trap 'f() { x=1; }' EXIT", &cancel)
    .unwrap();
  let registration = workspace
    .document(snapshot.document)
    .unwrap()
    .occurrences()
    .iter()
    .find(|site| site.name == Value::Literal(b"trap".to_vec()))
    .unwrap()
    .id;
  let mut derived = DerivedDocuments::default();
  for (body, parts) in [
    (
      b"f() { x=1; }".as_slice(),
      vec![OriginPart::new(0..12, Some(6..18))],
    ),
    (
      b"f() { :; }; x=1".as_slice(),
      vec![
        OriginPart::new(0..1, Some(6..7)),
        OriginPart::new(1..12, None),
        OriginPart::new(12..15, Some(12..15)),
      ],
    ),
  ] {
    derived
      .body(
        &mut workspace,
        registration,
        Ok(Some((
          DerivedOrigin {
            argument: Location {
              snapshot,
              bytes: 5..19,
            },
            parts,
          },
          Value::Literal(body.to_vec()),
        ))),
        4096,
        &cancel,
      )
      .unwrap();
  }
  workspace
    .indexed
    .insert(snapshot.document, (4096, Arc::new(derived)));
  assert_eq!(workspace.projected_symbols(snapshot.document).count(), 2);
  let details: Vec<_> = workspace
    .document_symbol_details(snapshot.document)
    .into_iter()
    .map(|entry| {
      (
        entry.symbol.name.as_slice(),
        entry.location.bytes,
        entry.range.map(|range| range.bytes),
        entry.container.map(|parent| parent.name.as_slice()),
      )
    })
    .collect();
  assert_eq!(
    details,
    [
      (b"f".as_slice(), 6..7, None, None),
      (b"x".as_slice(), 12..13, Some(12..15), None),
    ]
  );
}

#[test]
fn symbol_hierarchy_preserves_known_names_without_inventing_generated_ranges() {
  use crate::interface::application::{Application, Settings};

  let cancel = AtomicBool::new(false);
  let snapshot = Snapshot {
    document: DocumentId(1),
    revision: Revision(1),
  };
  let source = b"outer() { trap 'inner() { x=1; }' EXIT; }";
  let mut app = Application::new(Settings::new(std::path::PathBuf::from(
    env!("CARGO_MANIFEST_DIR"),
  )))
  .unwrap();
  app.workspace.update(snapshot, source, &cancel).unwrap();
  let registration = app
    .workspace
    .document(snapshot.document)
    .unwrap()
    .occurrences()
    .iter()
    .find(|site| site.name == Value::Literal(b"trap".to_vec()))
    .unwrap()
    .id;
  let mut derived = DerivedDocuments::default();
  derived
    .body(
      &mut app.workspace,
      registration,
      Ok(Some((
        DerivedOrigin {
          argument: Location {
            snapshot,
            bytes: 15..33,
          },
          parts: vec![
            OriginPart::new(0..5, Some(16..21)),
            OriginPart::new(5..10, None),
            OriginPart::new(10..16, Some(26..32)),
          ],
        },
        Value::Literal(b"inner() { x=1; }".to_vec()),
      ))),
      4096,
      &cancel,
    )
    .unwrap();
  app
    .workspace
    .indexed
    .insert(snapshot.document, (4096, Arc::new(derived)));
  let entries: Vec<_> = app
    .document_symbols(snapshot.document)
    .unwrap()
    .map(|entry| {
      (
        entry.details.symbol.name.as_slice(),
        entry.parent,
        entry.details.location.bytes,
        entry.details.range.map(|range| range.bytes),
        entry.details.container.map(|parent| parent.name.as_slice()),
      )
    })
    .collect();
  assert_eq!(
    entries,
    [
      (b"outer".as_slice(), None, 0..5, Some(0..source.len()), None),
      (b"inner", Some(0), 16..21, None, Some(b"outer".as_slice())),
      (
        b"x",
        Some(0),
        26..27,
        Some(26..29),
        Some(b"inner".as_slice())
      ),
    ]
  );
  assert!(app.workspace.analysis().is_none());
}

#[test]
fn syntax_index_and_analysis_reuse_body_snapshots_and_invalidate_them_together()
{
  let cancel = AtomicBool::new(false);
  let snapshot = Snapshot {
    document: DocumentId(1),
    revision: Revision(1),
  };
  let mut workspace = Workspace::default();
  workspace
    .update(snapshot, b"trap 'inner() { :; }' EXIT", &cancel)
    .unwrap();
  workspace
    .index_document(
      snapshot.document,
      engine::AnalysisLimits::default(),
      &cancel,
    )
    .unwrap();
  assert_eq!(workspace.documents().count(), 2);
  let body = workspace.derived.values().next().unwrap().document.clone();
  let indexed = workspace.indexed[&snapshot.document].1.clone();
  workspace
    .index_document(
      snapshot.document,
      engine::AnalysisLimits::default(),
      &cancel,
    )
    .unwrap();
  assert!(Arc::ptr_eq(
    &indexed,
    &workspace.indexed[&snapshot.document].1
  ));
  let result = workspace
    .analyze(AnalysisRequest::new(snapshot), &mut NoInputs, &cancel)
    .unwrap();
  assert_eq!(result.contexts().len(), 2);
  assert!(matches!(
    result.contexts()[1].kind,
    engine::ExecutionKind::Deferred(_)
  ));
  assert_eq!(workspace.documents().count(), 2);
  assert!(Arc::ptr_eq(
    &body,
    &workspace.documents[&body.snapshot().document]
  ));
  workspace.publish_analysis(result).unwrap();
  assert_eq!(workspace.projected_symbols(snapshot.document).count(), 1);
  workspace.clear_analysis();
  assert_eq!(workspace.projected_symbols(snapshot.document).count(), 1);
  assert_eq!(
    workspace.index_document(
      snapshot.document,
      engine::AnalysisLimits::default(),
      &AtomicBool::new(true)
    ),
    Err(Error::Cancelled)
  );
  let next = Snapshot {
    revision: Revision(2),
    ..snapshot
  };
  workspace
    .update(next, b"trap 'exit 1' EXIT", &cancel)
    .unwrap();
  assert_eq!(
    workspace
      .analyze(AnalysisRequest::new(next), &mut NoInputs, &cancel)
      .unwrap()
      .contexts()
      .len(),
    1
  );
  workspace
    .update(
      Snapshot {
        revision: Revision(3),
        ..snapshot
      },
      b"trap - EXIT",
      &cancel,
    )
    .unwrap();
  workspace
    .index_document(
      snapshot.document,
      engine::AnalysisLimits::default(),
      &cancel,
    )
    .unwrap();
  assert!(workspace.document(body.snapshot().document).is_none());
  assert_eq!(workspace.projected_symbols(snapshot.document).count(), 0);
  assert!(workspace.programs.is_empty());
}

fn origin(parts: Vec<OriginPart>) -> DerivedOrigin {
  let snapshot = Snapshot {
    document: DocumentId(1),
    revision: Revision(1),
  };
  DerivedOrigin {
    argument: Location {
      snapshot,
      bytes: 0..40,
    },
    parts,
  }
}

#[test]
fn source_projection_preserves_disjoint_ranges_and_rejects_ambiguous_boundaries()
 {
  let origin = origin(vec![
    OriginPart::new(0..2, Some(10..12)),
    OriginPart::new(2..4, Some(20..22)),
    OriginPart::new(4..6, Some(30..34)),
    OriginPart::new(6..7, None),
  ]);
  for (range, expected) in [
    (0..4, Some(vec![(10, 12), (20, 22)])),
    (3..6, Some(vec![(21, 22), (30, 34)])),
    (0..0, Some(vec![(10, 10)])),
    (1..1, Some(vec![(11, 11)])),
    (2..2, None),
    (4..5, None),
    (5..5, None),
    (6..7, None),
    (0..8, None),
  ] {
    let actual = origin.project(range.clone()).map(|locations| {
      locations
        .into_iter()
        .map(|location| {
          assert_eq!(location.snapshot, origin.argument.snapshot);
          (location.bytes.start, location.bytes.end)
        })
        .collect::<Vec<_>>()
    });
    assert_eq!(actual, expected, "{range:?}");
  }
}

#[test]
fn nested_origin_composition_keeps_known_positions_without_inventing_partial_mappings()
 {
  let outer = origin(vec![
    OriginPart::new(0..2, Some(10..12)),
    OriginPart::new(2..4, Some(20..22)),
    OriginPart::new(4..6, Some(30..34)),
  ]);
  let composed = outer.compose(&[
    OriginPart::new(0..4, Some(1..5)),
    OriginPart::new(4..5, Some(0..4)),
    OriginPart::new(5..6, None),
  ]);
  assert_eq!(
    composed,
    [
      OriginPart::new(0..1, Some(11..12)),
      OriginPart::new(1..3, Some(20..22)),
      OriginPart::new(3..4, None),
      OriginPart {
        bytes: 4..5,
        source: OriginMapping::Whole(vec![10..12, 20..22])
      },
      OriginPart::new(5..6, None),
    ]
  );
}

#[test]
fn derived_programs_share_text_but_keep_snapshot_identity_and_release_unused_entries()
 {
  let cancelled = AtomicBool::new(false);
  let snapshot = Snapshot {
    document: DocumentId(1),
    revision: Revision(1),
  };
  let document =
    Document::parse(snapshot, b"read x\nread x\n", &cancelled).unwrap();
  let mut workspace = Workspace::default();
  workspace.insert(document).unwrap();
  let mut derived = Vec::new();
  for bytes in [5..6, 12..13] {
    let (key, origin) = DerivedKey::new(
      DerivedOrigin {
        argument: Location {
          snapshot,
          bytes: bytes.clone(),
        },
        parts: vec![
          OriginPart::new(0..1, Some(bytes)),
          OriginPart::new(1..2, None),
        ],
      },
      b"x=",
      &workspace.derived,
    );
    workspace
      .add_derived(key.clone(), origin, &cancelled)
      .unwrap();
    derived.push(workspace.derived_keys[&key]);
  }
  let first = workspace.document(derived[0].document).unwrap();
  let second = workspace.document(derived[1].document).unwrap();
  assert!(Arc::ptr_eq(&first.source, &second.source));
  assert_ne!(first.symbols()[0].id, second.symbols()[0].id);
  for (doc, expected) in [(first, 5..6), (second, 12..13)] {
    let site = doc.occurrence(doc.symbols()[0].definition).unwrap();
    assert_eq!(
      workspace.project_locations(&site.location),
      Some(vec![Location {
        snapshot,
        bytes: expected
      }])
    );
  }
  assert_eq!(workspace.programs.len(), 1);
  workspace.discard(derived[0].document);
  assert!(workspace.document(derived[0].document).is_none());
  assert!(workspace.document(derived[1].document).is_some());
  assert_eq!(workspace.programs.len(), 1);
  workspace.remove(snapshot).unwrap();
  assert!(workspace.document(derived[1].document).is_none());
  assert_eq!(workspace.derived_keys.len(), 0);
  assert_eq!(workspace.programs.len(), 0);
}
