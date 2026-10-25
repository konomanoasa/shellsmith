use crate::interface::application::engine::{
  Error, Snapshot, SourceDocument, SourceInput, ValueIssue,
};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

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
  /// Deterministic evaluation steps, including condition and byte processing.
  pub work: usize,
  /// Cumulative storage charges for analysis data and memo tables, not process RSS.
  pub storage_bytes: usize,
}

impl AnalysisLimits {
  pub(in crate::interface::application::engine) fn validate(
    self,
  ) -> Result<(), Error> {
    if self.value_bytes == 0 || self.work == 0 || self.storage_bytes == 0 {
      return Err(Error::InvalidAnalysis(
        "Analysis limits must be positive".into(),
      ));
    }
    Ok(())
  }
}

impl Default for AnalysisLimits {
  fn default() -> Self {
    Self {
      value_bytes: 1024 * 1024,
      work: 2_000_000,
      storage_bytes: 64 * 1024 * 1024,
    }
  }
}

#[derive(Clone, Debug)]
pub(super) struct Budget(Rc<Usage>);

#[derive(Debug)]
struct Usage {
  work: Cell<usize>,
  storage: Cell<usize>,
  exhausted: Cell<bool>,
}

impl Default for Budget {
  fn default() -> Self {
    Self::new(AnalysisLimits::default())
  }
}

impl Budget {
  pub(super) fn new(limits: AnalysisLimits) -> Self {
    Self(Rc::new(Usage {
      work: Cell::new(limits.work),
      storage: Cell::new(limits.storage_bytes),
      exhausted: Cell::new(false),
    }))
  }

  fn consume(&self, remaining: &Cell<usize>, amount: usize) {
    if let Some(left) = remaining.get().checked_sub(amount) {
      remaining.set(left);
    } else {
      self.0.exhausted.set(true);
    }
  }

  pub(super) fn work(&self, amount: usize) {
    self.consume(&self.0.work, amount);
  }

  pub(super) fn retain(&self, bytes: usize) {
    self.consume(&self.0.storage, bytes);
  }

  pub(super) fn exhausted(&self) -> bool {
    self.0.exhausted.get()
  }

  pub(super) fn check(&self, cancelled: &AtomicBool) -> Result<(), Error> {
    crate::interface::application::engine::check_cancelled(cancelled)?;
    if self.0.exhausted.get() {
      Err(Error::AnalysisLimit)
    } else {
      Ok(())
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
