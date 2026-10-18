use super::super::environment::alternative;
use super::*;
use crate::interface::application::engine::Alternative;
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
