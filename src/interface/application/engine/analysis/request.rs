use crate::interface::application::engine::{
  Error, Snapshot, SourceDocument, SourceInput, ValueIssue,
};
use std::collections::BTreeMap;

/// Supplies immutable snapshots and stable results for each complete query during an analysis.
/// Providers own input acquisition and caching, including unresolved and failed lookups.
pub trait AnalysisInputs {
  fn source(&mut self, input: &SourceInput) -> Result<SourceDocument, Error>;
  fn evaluate(&mut self, query: &InputQuery) -> Result<InputResult, Error>;
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
  pub fn variable_binding(&self, name: &[u8]) -> InitialBinding {
    self.variables.get(name).copied().unwrap_or_else(|| {
      if self.values.contains_key(name) || self.environment.contains_key(name) {
        InitialBinding::External
      } else {
        self.other_variables
      }
    })
  }

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

impl AnalysisLimits {
  pub(in crate::interface::application::engine) fn validate(
    self,
  ) -> Result<(), Error> {
    if self.value_bytes == 0 {
      return Err(Error::InvalidAnalysis("Value size must be positive".into()));
    }
    Ok(())
  }
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
}

impl AnalysisRequest {
  #[must_use]
  pub fn new(entry: Snapshot) -> Self {
    Self {
      entry,
      initial: InitialEnvironment::default(),
      limits: AnalysisLimits::default(),
    }
  }
}
