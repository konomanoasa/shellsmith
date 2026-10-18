use crate::interface::application::engine::{
  self, Error, ReferenceId, Snapshot, SourceDocument, SourceInput, ValueIssue,
};
use std::collections::BTreeMap;

/// Supplies immutable document snapshots and stable query results for one analysis.
pub trait AnalysisInputs {
  fn source(
    &mut self,
    input: &engine::SourceInput,
  ) -> Result<SourceDocument, Error>;
  fn evaluate(
    &mut self,
    query: &engine::InputQuery,
  ) -> Result<engine::InputResult, Error>;
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum InputQuery {
  Home(Vec<u8>),
  Encoding(Vec<u8>),
  Directory {
    cwd: Vec<u8>,
    path: Vec<u8>,
    physical: bool,
  },
  Glob {
    cwd: Vec<u8>,
    pattern: Vec<u8>,
    quoted: Vec<bool>,
    utf8: bool,
    portable_collation: bool,
  },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputResult {
  Encoding(CharacterEncoding),
  Path(Vec<u8>),
  Paths(Vec<Vec<u8>>),
  Missing,
  Unknown(ValueIssue),
}

pub type EvaluationInputs = BTreeMap<InputQuery, InputResult>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CharacterEncoding {
  Ascii,
  Utf8,
  Other,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InitialBinding {
  #[default]
  Unknown,
  Absent,
  External,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InitialEnvironment {
  pub environment: BTreeMap<Vec<u8>, Vec<u8>>,
  pub values: BTreeMap<Vec<u8>, Vec<u8>>,
  pub cwd: Option<Vec<u8>>,
  pub positional: Option<Vec<Vec<u8>>>,
  pub user_homes: BTreeMap<Vec<u8>, Vec<u8>>,
  pub special: BTreeMap<Vec<u8>, Vec<u8>>,
  pub variables: BTreeMap<Vec<u8>, InitialBinding>,
  pub functions: BTreeMap<Vec<u8>, InitialBinding>,
  pub other_variables: InitialBinding,
  pub other_functions: InitialBinding,
  pub errexit: Option<bool>,
  pub nounset: Option<bool>,
  pub noglob: Option<bool>,
}

impl InitialEnvironment {
  #[must_use]
  pub fn empty() -> Self {
    Self {
      positional: Some(Vec::new()),
      special: BTreeMap::from([(b"?".to_vec(), b"0".to_vec())]),
      other_variables: InitialBinding::Absent,
      other_functions: InitialBinding::Absent,
      errexit: Some(false),
      nounset: Some(false),
      noglob: Some(false),
      ..Self::default()
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnalysisLimits {
  pub value_bytes: usize,
}

impl Default for AnalysisLimits {
  fn default() -> Self {
    Self {
      value_bytes: 1024 * 1024,
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalysisRequest {
  pub entry: Snapshot,
  pub initial: InitialEnvironment,
  pub limits: AnalysisLimits,
  pub source_inputs: BTreeMap<ReferenceId, SourceInput>,
  pub inputs: engine::EvaluationInputs,
}

impl AnalysisRequest {
  #[must_use]
  pub fn new(entry: Snapshot) -> Self {
    Self {
      entry,
      initial: InitialEnvironment::default(),
      limits: AnalysisLimits::default(),
      source_inputs: BTreeMap::new(),
      inputs: BTreeMap::new(),
    }
  }
}

pub(in crate::interface::application::engine) fn validate(
  request: &AnalysisRequest,
) -> Result<(), Error> {
  if request.limits.value_bytes == 0 {
    return Err(Error::InvalidAnalysis("Value size must be positive".into()));
  }
  Ok(())
}
