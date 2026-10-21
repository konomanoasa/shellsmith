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
  assert_eq!(decisions.branch(0, first, first), first);
  let branch = decisions.branch(0, first, second);
  assert_eq!(decisions.branch(0, first, second), branch);
  assert_eq!(decisions.nodes.len(), 3);
}

#[test]
fn restricting_nested_choices_preserves_unselected_branches_and_shares_results()
{
  let mut decisions = Decisions::default();
  let first = decisions.intern(Decision::Value(StateValue::Binding(Binding {
    resolution: alternative(Alternative::External),
    value: Arc::new(Value::Literal(b"first".to_vec())),
  })));
  let second =
    decisions.intern(Decision::Value(StateValue::Binding(Binding {
      resolution: alternative(Alternative::External),
      value: Arc::new(Value::Literal(b"second".to_vec())),
    })));
  let third = decisions.intern(Decision::Value(StateValue::Binding(Binding {
    resolution: alternative(Alternative::External),
    value: Arc::new(Value::Literal(b"third".to_vec())),
  })));
  let inner = decisions.branch(0, first, second);
  let outer = decisions.branch(1, inner, third);
  for (right, selected) in [(false, first), (true, second)] {
    let expected = decisions.branch(1, selected, third);
    let count = decisions.nodes.len();
    let mut cache = HashMap::new();
    assert_eq!(decisions.restrict(outer, 0, right, &mut cache), expected);
    assert_eq!(decisions.restrict(inner, 0, right, &mut cache), selected);
    assert_eq!(decisions.nodes.len(), count);
  }
  assert_eq!(
    decisions.restrict(inner, 1, true, &mut HashMap::new()),
    inner
  );
  assert_eq!(
    decisions.restrict(outer, 1, true, &mut HashMap::new()),
    third
  );
  assert_eq!(
    decisions.restrict(outer, 1, false, &mut HashMap::new()),
    inner
  );
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
