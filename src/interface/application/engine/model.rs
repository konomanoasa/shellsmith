use super::Value;
use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocumentId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Revision(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Snapshot {
  pub document: DocumentId,
  pub revision: Revision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Location {
  pub snapshot: Snapshot,
  pub bytes: Range<usize>,
}

impl Location {
  pub(in crate::interface::application) fn contains(
    &self,
    inner: &Self,
  ) -> bool {
    self.snapshot == inner.snapshot
      && self.bytes.start <= inner.bytes.start
      && inner.bytes.end <= self.bytes.end
  }
}

macro_rules! entity_id {
  ($($name:ident),+) => {$ (
    #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
    pub struct $name {
      pub snapshot: Snapshot,
      pub(in crate::interface::application::engine) index: usize,
    }
  )+};
}

entity_id!(SymbolId, OccurrenceId, ContextId, ReferenceId);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum UseSite {
  Syntax(OccurrenceId),
  Argument {
    snapshot: Snapshot,
    start: usize,
    end: usize,
  },
}

impl UseSite {
  #[must_use]
  pub fn argument(location: &Location) -> Self {
    Self::Argument {
      snapshot: location.snapshot,
      start: location.bytes.start,
      end: location.bytes.end,
    }
  }

  #[must_use]
  pub const fn syntax(self) -> Option<OccurrenceId> {
    match self {
      Self::Syntax(id) => Some(id),
      Self::Argument { .. } => None,
    }
  }

  #[must_use]
  pub const fn snapshot(self) -> Snapshot {
    match self {
      Self::Syntax(id) => id.snapshot,
      Self::Argument { snapshot, .. } => snapshot,
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SymbolKind {
  Function,
  Variable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OccurrenceKind {
  FunctionDefinition,
  Command,
  Assignment,
  DeclarationAssignment,
  ForVariable,
  VariableExpansion,
  PositionalParameter,
  SpecialParameter,
  Arithmetic,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Access {
  Definition,
  Read,
  NameReference,
  Write,
  ReadWrite,
  ReadConditionalWrite,
  Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Occurrence {
  pub id: OccurrenceId,
  pub location: Location,
  pub context: ContextId,
  pub kind: OccurrenceKind,
  pub access: Access,
  pub name: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Symbol {
  pub id: SymbolId,
  pub kind: SymbolKind,
  pub name: Vec<u8>,
  pub definition: OccurrenceId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Context {
  pub id: ContextId,
  pub parent: Option<ContextId>,
  pub kind: String,
  pub field: Option<String>,
  pub location: Location,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Classification {
  pub location: Location,
  pub capture: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceReference {
  pub id: ReferenceId,
  pub command: Location,
  pub operand: Option<Location>,
  pub context: ContextId,
  pub value: Value,
  pub annotation: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DependencyState {
  Pending,
  Resolved(Snapshot),
  Unresolved(String),
  Failed(String),
  Invalidated(DocumentId),
}

impl DependencyState {
  pub(in crate::interface::application::engine) const fn target(
    &self,
  ) -> Option<DocumentId> {
    match self {
      Self::Resolved(snapshot) => Some(snapshot.document),
      Self::Invalidated(document) => Some(*document),
      _ => None,
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Change {
  pub affected: Vec<DocumentId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
  InvalidEncoding { byte: usize },
  SourceTooLarge,
  Cancelled,
  Parser(String),
  StaleSnapshot(Snapshot),
  UnknownDocument(DocumentId),
  UnknownReference(ReferenceId),
  InvalidAnalysis(String),
  InvalidPosition,
  Input(String),
  StaleAnalysis,
}

impl fmt::Display for Error {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::InvalidPosition => formatter.write_str("Invalid source position"),
      Self::InvalidEncoding { byte } => {
        write!(formatter, "Invalid UTF-8 at byte {byte}")
      }
      Self::SourceTooLarge => formatter.write_str("Source exceeds u32 bytes"),
      Self::Cancelled => formatter.write_str("Analysis cancelled"),
      Self::Parser(message)
      | Self::InvalidAnalysis(message)
      | Self::Input(message) => formatter.write_str(message),
      Self::StaleSnapshot(snapshot) => {
        write!(formatter, "Stale document snapshot: {snapshot:?}")
      }
      Self::UnknownDocument(id) => {
        write!(formatter, "Unknown document: {id:?}")
      }
      Self::UnknownReference(id) => {
        write!(formatter, "Unknown reference: {id:?}")
      }
      Self::StaleAnalysis => {
        formatter.write_str("Analysis inputs have changed")
      }
    }
  }
}

impl std::error::Error for Error {}

pub(in crate::interface::application) fn check_cancelled(
  cancelled: &AtomicBool,
) -> Result<(), Error> {
  if cancelled.load(Ordering::Relaxed) {
    Err(Error::Cancelled)
  } else {
    Ok(())
  }
}

#[must_use]
pub fn valid_name(name: &[u8]) -> bool {
  name
    .first()
    .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
    && name
      .iter()
      .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}
