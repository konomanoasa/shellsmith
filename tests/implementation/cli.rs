use super::Replacement;
use crate::interface::fixtures::Fixture;

#[test]
fn a_replacement_refuses_changes_made_after_formatting() {
  let fixture = Fixture::new();
  let path = fixture.write("edited.sh", "echo $new_value\n");
  let replacement = Replacement {
    original: "echo $value\n".into(),
    formatted: "echo ${value}\n".into(),
  };
  let error = replacement.write(&path).unwrap_err();
  assert_eq!(error.to_string(), "File changed during formatting");
  assert_eq!(std::fs::read_to_string(path).unwrap(), "echo $new_value\n");
  assert_eq!(std::fs::read_dir(&fixture.root).unwrap().count(), 1);
}
