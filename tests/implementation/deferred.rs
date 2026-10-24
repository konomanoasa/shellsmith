use super::*;
use std::sync::Arc;

#[test]
fn invariant_history_keeps_one_change_per_name_and_the_first_registration() {
  use crate::interface::application::engine::{
    DocumentId, ExecutionContextId, Revision, Snapshot,
  };
  let snapshot = Snapshot {
    document: DocumentId(1),
    revision: Revision(1),
  };
  let frame = Frame {
    snapshot,
    context: ExecutionContextId(0),
    loops: 0,
    dynamic_loops: false,
    returnable: false,
    functions: 0,
  };
  let registration = Location {
    snapshot,
    bytes: 0..4,
  };
  let mut env = Environment::new(&InitialEnvironment {
    values: [
      (b"x".to_vec(), b"1".to_vec()),
      (b"kept".to_vec(), b"same".to_vec()),
    ]
    .into(),
    ..InitialEnvironment::empty()
  });
  let mut inputs = InvariantInputs::default();
  inputs.capture(snapshot, registration.clone(), frame, frame.context, &env);
  let original = env.variables.get(b"x").clone();
  let mut changed = original.clone();
  changed.value = Arc::new(Value::Literal(b"2".to_vec()));
  env.variables.values.insert(b"x".to_vec(), changed);
  inputs.observe(frame.context, &env);
  env.variables.values.insert(b"x".to_vec(), original);
  inputs.observe(frame.context, &env);
  inputs.capture(snapshot, registration.clone(), frame, frame.context, &env);
  assert_eq!(inputs.pending.len(), 1);
  assert_eq!(
    inputs.observed[&frame.context]
      .changes
      .variables
      .values
      .len(),
    1
  );
  let input = inputs.input(&inputs.pending[&(snapshot, frame.context)]);
  assert_eq!(
    input.variables.get(b"x").value.as_ref(),
    &Value::Unknown(ValueIssue::Dynamic)
  );
  assert_eq!(
    input.variables.get(b"kept").value.as_ref(),
    &Value::Literal(b"same".to_vec())
  );
  let later = Snapshot {
    document: DocumentId(2),
    ..snapshot
  };
  inputs.capture(later, registration, frame, frame.context, &env);
  let input = inputs.input(&inputs.pending[&(later, frame.context)]);
  assert_eq!(
    input.variables.get(b"x").value.as_ref(),
    &Value::Literal(b"1".to_vec())
  );
}
