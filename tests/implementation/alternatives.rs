use super::super::environment::alternative;
use super::*;
use crate::interface::application::engine::Value;
use crate::interface::application::engine::{Alternative, InitialEnvironment};
use std::sync::Arc;

#[test]
fn identical_decisions_share_nodes_and_equal_branches_collapse() {
  let mut decisions = Decisions::default();
  let first = decisions.intern(Decision::Value(StateValue::Binding(Binding {
    resolution: alternative(Alternative::External),
    value: Arc::new(Value::Literal(b"first".to_vec())),
  })));
  let again = decisions.intern(Decision::Value(StateValue::Binding(Binding {
    resolution: alternative(Alternative::External),
    value: Arc::new(Value::Literal(b"first".to_vec())),
  })));
  assert_eq!(first, again);
  let second =
    decisions.intern(Decision::Value(StateValue::Binding(Binding {
      resolution: alternative(Alternative::External),
      value: Arc::new(Value::Literal(b"second".to_vec())),
    })));
  let guard = decisions.expressions.conditions.variable(0);
  assert_eq!(decisions.branch(guard, first, first), first);
  let branch = decisions.branch(guard, first, second);
  assert_eq!(decisions.branch(guard, first, second), branch);
  assert_eq!(decisions.nodes.len(), 3);
}

#[test]
fn partitions_preserve_unread_correlations_and_merge_equal_inputs() {
  let assign = |env: &mut Environment, name: &[u8], value: &[u8]| {
    env.assign(
      name,
      alternative(Alternative::External),
      Value::Literal(value.to_vec()),
    );
  };
  let mut env = Environment::new(&InitialEnvironment::empty());
  let mut other = env.clone();
  assign(&mut env, b"a", b"1");
  assign(&mut env, b"d", b"x");
  assign(&mut other, b"a", b"2");
  assign(&mut other, b"d", b"y");
  env.join(&other);
  let mut replaced = env.clone();
  assign(&mut replaced, b"d", b"z");
  env.join(&replaced);
  let choices = env.alternatives.decisions.borrow().next;
  let read = BTreeSet::from([StateKey::Variable(b"a".to_vec())]);
  let cancelled = AtomicBool::new(false);
  let inputs = Alternatives::partitions(&env, &read, &cancelled).unwrap();
  assert_eq!(inputs.len(), 2);
  for (input, expected, allowed, excluded) in [
    (&inputs[0], b"1", b"x", b"y"),
    (&inputs[1], b"2", b"y", b"x"),
  ] {
    assert!(!input.alternatives.depends(&read));
    assert_eq!(
      input.variables.get(b"a").value.as_ref(),
      &Value::Literal(expected.to_vec())
    );
    let value = &input.variables.get(b"d").value;
    assert!(value.can_equal(allowed));
    assert!(value.can_equal(b"z"));
    assert!(!value.can_equal(excluded));
  }
  let mut joined = inputs[0].clone();
  joined.join(&inputs[1]);
  assert_eq!(joined.alternatives.decisions.borrow().next, choices);
  let both = BTreeSet::from([
    StateKey::Variable(b"a".to_vec()),
    StateKey::Variable(b"d".to_vec()),
  ]);
  let pairs = Alternatives::partitions(&joined, &both, &cancelled)
    .unwrap()
    .into_iter()
    .map(|env| {
      (
        env.variables.get(b"a").value.clone(),
        env.variables.get(b"d").value.clone(),
      )
    })
    .collect::<BTreeSet<_>>();
  let expected =
    [(b"1", b"x"), (b"1", b"z"), (b"2", b"y"), (b"2", b"z")].map(|(a, b)| {
      (
        Arc::new(Value::Literal(a.to_vec())),
        Arc::new(Value::Literal(b.to_vec())),
      )
    });
  assert_eq!(pairs, expected.into());
}

#[test]
fn widening_keeps_shared_decisions_without_creating_choices_and_forgets_replaced_names()
 {
  let mut left = Environment::new(&InitialEnvironment::empty());
  let mut right = left.clone();
  for name in [b"a", b"b"] {
    left.assign(
      name,
      alternative(Alternative::External),
      Value::Literal(b"1".to_vec()),
    );
    right.assign(
      name,
      alternative(Alternative::External),
      Value::Literal(b"2".to_vec()),
    );
  }
  left.join(&right);
  let original = left.alternatives.clone();
  let count = original.decisions.borrow().nodes.len();
  let choices = original.decisions.borrow().next;
  let mut repeated = left.clone();
  repeated.assign(
    b"unrelated",
    alternative(Alternative::External),
    Value::Literal(b"3".to_vec()),
  );
  left.widen(&repeated);
  assert_eq!(left.alternatives, original);
  let stable = left.clone();
  left.widen(&repeated);
  assert_eq!(left, stable);

  repeated.assign(
    b"a",
    alternative(Alternative::External),
    Value::Literal(b"3".to_vec()),
  );
  left.widen(&repeated);
  assert!(
    !left
      .alternatives
      .values
      .contains_key(&StateKey::Variable(b"a".to_vec()))
  );
  assert_eq!(
    left
      .alternatives
      .values
      .get(&StateKey::Variable(b"b".to_vec())),
    original.values.get(&StateKey::Variable(b"b".to_vec()))
  );
  assert_eq!(original.decisions.borrow().nodes.len(), count);
  assert_eq!(original.decisions.borrow().next, choices);

  let unrelated = Environment::new(&InitialEnvironment::empty());
  left.widen(&unrelated);
  assert!(left.alternatives.is_empty());
}

#[test]
fn shared_string_queries_and_pattern_transfers_match_enumerated_values() {
  use crate::interface::application::engine::ValueIssue;
  use crate::interface::application::engine::word::Pattern;
  let flag = AtomicBool::new(false);
  for a in [b"".as_slice(), b"a", b"ab", b":a"] {
    for b in [b"".as_slice(), b"a", b"b", b"a:"] {
      for c in [b"".as_slice(), b"a", b"b", b":b"] {
        for d in [b"".as_slice(), b"a", b"ab", b":"] {
          let mut values = Values::default();
          let first = values.conditions.variable(0);
          let second = values.conditions.variable(1);
          let [a_id, b_id, c_id, d_id] = [a, b, c, d].map(|bytes| {
            values.literal(Arc::new(Value::Literal(bytes.to_vec())))
          });
          let left = values.select(first, a_id, b_id);
          let right = values.select(second, c_id, d_id);
          let root = values.concat(left, right, &flag).unwrap().unwrap();
          let concrete = [(a, c), (a, d), (b, c), (b, d)]
            .map(|(left, right)| Value::Literal([left, right].concat()));
          assert_eq!(
            values.projected(root, &flag).unwrap().as_ref(),
            &Value::join_all(concrete.clone(), ValueIssue::Dynamic)
          );
          for pattern in [b"a*".as_slice(), b"*:*", b"[ab]?"] {
            let pattern = Pattern::compile(
              pattern,
              &vec![false; pattern.len()],
              false,
              true,
            )
            .unwrap();
            for suffix in [false, true] {
              for longest in [false, true] {
                let result = values
                  .remove(root, &pattern, suffix, longest, false, &flag)
                  .unwrap();
                let expected = concrete
                  .iter()
                  .map(|value| {
                    pattern.remove(value, suffix, longest, false, &flag)
                  })
                  .collect::<BTreeSet<_>>();
                let actual = values
                  .candidates(result, &flag)
                  .unwrap()
                  .keys()
                  .map(|id| values.constant(*id).unwrap().as_ref().clone())
                  .collect::<BTreeSet<_>>();
                assert_eq!(
                  actual, expected,
                  "{a:?}/{b:?}/{c:?}/{d:?}: suffix={suffix}, longest={longest}"
                );
              }
            }
          }
        }
      }
    }
  }
}
