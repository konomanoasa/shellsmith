use super::*;

#[test]
fn positional_joins_keep_cardinality_causes_separate_from_element_causes() {
  let mut left = Environment::new(&InitialEnvironment::empty());
  left.positional = Positionals {
    fields: vec![(Value::Unknown(ValueIssue::SizeLimit), FieldCount::One)],
  };
  let right = Environment::new(&InitialEnvironment::empty());
  left.join(&right);
  assert_eq!(left.parameter(b"#").0, Value::Unknown(ValueIssue::Dynamic));
  assert_eq!(
    left
      .parameter(b"1")
      .0
      .issue()
      .unwrap()
      .causes()
      .collect::<Vec<_>>(),
    [ValueIssue::Dynamic, ValueIssue::SizeLimit]
  );

  let mut right = right;
  right.positional = Positionals::unknown(ValueIssue::Arithmetic);
  left.join(&right);
  assert_eq!(
    left
      .parameter(b"#")
      .0
      .issue()
      .unwrap()
      .causes()
      .collect::<Vec<_>>(),
    [ValueIssue::Dynamic, ValueIssue::Arithmetic]
  );
  assert_eq!(
    left
      .parameter(b"1")
      .0
      .issue()
      .unwrap()
      .causes()
      .collect::<Vec<_>>(),
    [
      ValueIssue::Dynamic,
      ValueIssue::SizeLimit,
      ValueIssue::Arithmetic
    ]
  );
}

#[test]
fn patterned_fallbacks_survive_joins_and_allow_explicit_overwrites() {
  let absent = Binding::new(initial(InitialBinding::Absent));
  let explicit = Binding {
    resolution: alternative(Alternative::External),
    value: Arc::new(Value::Literal(b"value".to_vec())),
  };
  let mut left = Names {
    values: OrdMap::new(),
    fallback: absent.clone(),
    patterns: BTreeSet::new(),
    uncertain: None,
  };
  left.restrict(
    Value::partial(b"tmp_".to_vec(), Vec::new(), 4, ValueIssue::Dynamic),
    ResolutionIssue::DynamicName,
    ValueIssue::Dynamic,
  );
  let right = Names {
    values: OrdMap::from_iter([(b"tmp_x".to_vec(), explicit.clone())]),
    fallback: absent.clone(),
    patterns: BTreeSet::new(),
    uncertain: None,
  };
  for (mut joined, other) in [(left.clone(), &right), (right.clone(), &left)] {
    joined.join(other);
    assert_eq!(joined.get(b"unrelated"), &absent);
    for name in [b"tmp_x".as_slice(), b"tmp_y"] {
      assert_eq!(
        joined.get(name).value.as_ref(),
        &Value::Unknown(ValueIssue::Dynamic)
      );
      assert!(
        joined
          .get(name)
          .resolution
          .issues
          .contains(&ResolutionIssue::DynamicName)
      );
    }
    let before = joined.clone();
    joined.join(other);
    assert_eq!(joined, before);
    joined.values.insert(b"tmp_y".to_vec(), explicit.clone());
    assert_eq!(joined.get(b"tmp_y"), &explicit);
    assert_eq!(
      joined.get(b"tmp_z").value.as_ref(),
      &Value::Unknown(ValueIssue::Dynamic)
    );
  }
}

#[test]
fn joining_an_explicit_unset_name_keeps_it_separate_from_unknown_fallback() {
  let left = Names {
    patterns: BTreeSet::new(),
    uncertain: None,
    values: OrdMap::new(),
    fallback: Binding::new(initial(InitialBinding::Absent)),
  };
  let right = Names {
    patterns: BTreeSet::new(),
    uncertain: None,
    values: OrdMap::from_iter([(
      b"known".to_vec(),
      Binding::new(initial(InitialBinding::Absent)),
    )]),
    fallback: Binding::new(initial(InitialBinding::Unknown)),
  };
  for (mut joined, other) in [(left.clone(), &right), (right.clone(), &left)] {
    joined.join(other);
    assert_eq!(
      joined.get(b"known").resolution,
      alternative(Alternative::Unset)
    );
    assert_eq!(
      joined.get(b"known").value.as_ref(),
      &Value::Literal(Vec::new())
    );
    assert_eq!(
      joined.get(b"other").resolution,
      initial(InitialBinding::Unknown)
    );
    let before = joined.clone();
    joined.join(other);
    assert_eq!(joined, before);
  }
}

#[test]
fn joining_updated_names_preserves_the_original_snapshot_and_unaffected_values()
{
  let literal = |value: &[u8]| Binding {
    resolution: alternative(Alternative::External),
    value: Arc::new(Value::Literal(value.to_vec())),
  };
  let original = Names {
    patterns: BTreeSet::new(),
    uncertain: None,
    values: OrdMap::from_iter([
      (b"changed".to_vec(), literal(b"first")),
      (b"kept".to_vec(), literal(b"stable")),
    ]),
    fallback: Binding::new(initial(InitialBinding::Absent)),
  };
  let mut updated = original.clone();
  updated
    .values
    .insert(b"changed".to_vec(), literal(b"second"));
  updated.join(&original);
  assert_eq!(original.get(b"changed"), &literal(b"first"));
  assert_eq!(updated.get(b"kept"), &literal(b"stable"));
  let changed = &updated.get(b"changed").value;
  assert!(changed.can_equal(b"first"));
  assert!(changed.can_equal(b"second"));
  assert!(!changed.can_equal(b"stable"));
  let before = updated.clone();
  updated.join(&original);
  assert_eq!(updated, before);
}

#[test]
fn word_and_output_preserve_constraints_across_boundaries() {
  let mut source =
    Value::partial(b"/bin/".to_vec(), Vec::new(), 5, ValueIssue::Dynamic);
  source.join(&Value::Literal(Vec::new()));
  let mut word = crate::interface::application::engine::word::Word::value(
    source.clone(),
    true,
  );
  assert_eq!(word.scalar(), source);
  word.extend(
    crate::interface::application::engine::word::Word::value(
      Value::Literal(b"/tail".to_vec()),
      true,
    ),
    usize::MAX,
  );
  assert!(!word.scalar().may_be_name());
  let mut output = Output::default();
  output.append(source, usize::MAX);
  output.append(Value::Literal(b"/tail\n".to_vec()), usize::MAX);
  let mut result = output.value();
  result.trim_newlines();
  assert!(result.can_equal(b"/tail"));
  assert!(result.can_equal(b"/bin/tool/tail"));
  assert!(!result.can_equal(b"read"));
  let mut bounded =
    crate::interface::application::engine::word::Word::default();
  bounded.extend(
    crate::interface::application::engine::word::Word::value(
      Value::partial(b"/bin/".to_vec(), Vec::new(), 5, ValueIssue::Dynamic),
      true,
    ),
    4,
  );
  assert_eq!(
    bounded
      .scalar()
      .issue()
      .unwrap()
      .causes()
      .collect::<Vec<_>>(),
    [ValueIssue::Dynamic, ValueIssue::SizeLimit]
  );
}
