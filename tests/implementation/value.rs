use super::*;

#[test]
fn partial_value_joins_retain_failure_causes_and_invalid_values_have_no_bounds()
{
  let partial =
    Value::partial(b"prefix".to_vec(), Vec::new(), 6, ValueIssue::Dynamic);
  let unknown = Value::Unknown(ValueIssue::UnsupportedExpansion);
  for (mut joined, other) in
    [(partial.clone(), &unknown), (unknown.clone(), &partial)]
  {
    joined.join(other);
    assert_eq!(
      joined.issue().unwrap().causes().collect::<Vec<_>>(),
      [ValueIssue::Dynamic, ValueIssue::UnsupportedExpansion]
    );
  }
  assert_eq!(
    Value::partial(b"prefix".to_vec(), Vec::new(), 6, ValueIssue::Arithmetic),
    Value::Unknown(ValueIssue::Arithmetic)
  );
}

#[test]
fn value_constraints_compose_soundly_and_joins_stabilize() {
  let literal = |bytes: &[u8]| Value::Literal(bytes.to_vec());
  let mut path =
    Value::partial(b"/".to_vec(), b"\n".to_vec(), 2, ValueIssue::Dynamic);
  path.join(&literal(b""));
  assert!(path.can_equal(b""));
  assert!(path.can_equal(b"/usr/bin/tool\n"));
  assert!(!path.can_equal(b"read\n"));
  path.trim_newlines();
  assert!(path.can_equal(b"/usr/bin/tool"));
  assert!(!path.can_equal(b"read"));
  assert_eq!(path.empty(), None);
  let mut command = path.clone();
  command.concat(&literal(b"unset"), usize::MAX);
  assert!(command.can_equal(b"unset"));
  assert!(command.can_equal(b"/usr/bin/toolunset"));
  assert!(!command.can_equal(b"read"));
  let mut joined = literal(b"/bin/a");
  joined.join(&literal(b"/bin/b"));
  assert_eq!(joined.known_prefix(), b"/bin/");
  assert!(!joined.can_equal(b"/bin/"));
  assert!(!joined.may_be_name());
  let mut suffixed =
    Value::partial(Vec::new(), b"x".to_vec(), 1, ValueIssue::Dynamic);
  suffixed.join(&literal(b""));
  assert!(suffixed.can_equal(b""));
  assert!(suffixed.can_equal(b"ax"));
  assert!(!suffixed.can_equal(b"eval"));
  let values = [
    path,
    command,
    joined,
    suffixed,
    literal(b""),
    literal(b"name"),
    Value::Unknown(ValueIssue::Dynamic),
  ];
  for left in &values {
    let mut same = left.clone();
    same.join(left);
    assert_eq!(&same, left);
    for right in &values {
      let mut a = left.clone();
      a.join(right);
      let mut b = right.clone();
      b.join(left);
      assert_eq!(a, b);
      assert!(a.covers(left));
      assert!(a.covers(right));
      let mut again = a.clone();
      again.join(left);
      again.join(right);
      assert_eq!(a, again);
    }
  }
  let mut repeated = literal(b"/bin/a");
  let mut next = repeated.clone();
  next.concat(&literal(b"x"), usize::MAX);
  repeated.join(&next);
  let stable = repeated.clone();
  for _ in 0..100 {
    let mut next = repeated.clone();
    next.concat(&literal(b"x"), usize::MAX);
    repeated.join(&next);
  }
  assert_eq!(repeated, stable);
  assert!(!repeated.may_be_name());
}

#[test]
fn constraint_operations_never_exclude_concrete_outputs() {
  let words: &[&[u8]] = &[
    b"",
    b"a",
    b"a\n",
    b"a\na\n",
    b"/",
    b"/\n",
    b"ab",
    b"a/b",
    b"\n\n",
    b"z=\n",
    b"tmp_a=value",
    b"tmp_b=other",
    b"tmp_a",
  ];
  let mut choices = Vec::new();
  for left in words {
    for right in words {
      let mut joined = Value::Literal(left.to_vec());
      joined.join(&Value::Literal(right.to_vec()));
      choices.push((joined.clone(), [*left, *right]));
      for bytes in [left, right] {
        let name = joined.before(b'=');
        let end = bytes
          .iter()
          .position(|byte| *byte == b'=')
          .unwrap_or(bytes.len());
        assert!(name.can_equal(&bytes[..end]));
        let mut trimmed = joined.clone();
        trimmed.trim_newlines();
        let end = bytes
          .iter()
          .rposition(|byte| *byte != b'\n')
          .map_or(0, |last| last + 1);
        let expected = &bytes[..end];
        assert!(
          trimmed.can_equal(expected),
          "{joined:?} -> {trimmed:?}: {expected:?}"
        );
        for suffix in words {
          let mut appended = joined.clone();
          appended.concat(&Value::Literal(suffix.to_vec()), usize::MAX);
          assert!(appended.can_equal(&[*bytes, *suffix].concat()));
        }
      }
      let prefix = joined.known_prefix();
      for length in 0..=prefix.len() {
        let removed = joined.remove_prefix(length);
        assert!(removed.can_equal(&left[length..]));
        assert!(removed.can_equal(&right[length..]));
      }
    }
  }
  for (left, left_words) in &choices {
    for (right, right_words) in &choices {
      let mut appended = left.clone();
      appended.concat(right, usize::MAX);
      let mut trimmed = appended.clone();
      trimmed.trim_newlines();
      for a in left_words {
        for b in right_words {
          let expected = [*a, *b].concat();
          assert!(
            appended.can_equal(&expected),
            "{left:?} + {right:?} -> {appended:?}: {expected:?}"
          );
          let end = expected
            .iter()
            .rposition(|byte| *byte != b'\n')
            .map_or(0, |last| last + 1);
          assert!(
            trimmed.can_equal(&expected[..end]),
            "{appended:?} -> {trimmed:?}: {expected:?}"
          );
        }
      }
    }
  }
}
