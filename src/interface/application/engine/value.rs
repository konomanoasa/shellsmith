use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Value {
  Literal(Vec<u8>),
  Partial(PartialValue),
  Unknown(ValueIssue),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PartialValue {
  pub choices: Vec<ValueConstraint>,
  pub issue: ValueIssue,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ValueConstraint {
  pub prefix: Vec<u8>,
  pub suffix: Vec<u8>,
  pub minimum: usize,
  pub required: BTreeSet<u8>,
  pub exact: bool,
}

impl ValueConstraint {
  fn literal(bytes: Vec<u8>) -> Self {
    Self {
      minimum: bytes.len(),
      suffix: bytes.clone(),
      required: bytes.iter().copied().collect(),
      prefix: bytes,
      exact: true,
    }
  }

  fn any() -> Self {
    Self {
      prefix: Vec::new(),
      suffix: Vec::new(),
      minimum: 0,
      required: BTreeSet::new(),
      exact: false,
    }
  }

  fn hull_all(choices: impl IntoIterator<Item = Self>) -> Self {
    choices
      .into_iter()
      .reduce(|mut a, b| {
        a.hull(&b);
        a
      })
      .unwrap_or_else(Self::any)
  }

  fn accepts(&self, bytes: &[u8]) -> bool {
    if self.exact {
      return self.prefix == bytes;
    }
    bytes.len() >= self.minimum
      && bytes.starts_with(&self.prefix)
      && bytes.ends_with(&self.suffix)
      && self.required.iter().all(|byte| bytes.contains(byte))
  }

  fn covers(&self, other: &Self) -> bool {
    if other.exact {
      return self.accepts(&other.prefix);
    }
    !self.exact
      && self.minimum <= other.minimum
      && other.prefix.starts_with(&self.prefix)
      && other.suffix.ends_with(&self.suffix)
      && self.required.is_subset(&other.required)
  }

  fn hull(&mut self, other: &Self) {
    if self == other {
      return;
    }
    let prefix = self
      .prefix
      .iter()
      .zip(&other.prefix)
      .take_while(|(a, b)| a == b)
      .count();
    self.prefix.truncate(prefix);
    let suffix = self
      .suffix
      .iter()
      .rev()
      .zip(other.suffix.iter().rev())
      .take_while(|(a, b)| a == b)
      .count();
    self.suffix.drain(..self.suffix.len() - suffix);
    self.minimum = self.minimum.min(other.minimum);
    self.required.retain(|byte| other.required.contains(byte));
    self.exact = false;
  }

  fn concat(&self, other: &Self) -> Self {
    let mut prefix = self.prefix.clone();
    if self.exact {
      prefix.extend_from_slice(&other.prefix);
    }
    let mut suffix = Vec::new();
    if other.exact {
      suffix.extend_from_slice(&self.suffix);
    }
    suffix.extend_from_slice(&other.suffix);
    Self {
      prefix,
      suffix,
      minimum: self.minimum.saturating_add(other.minimum),
      required: self.required.union(&other.required).copied().collect(),
      exact: self.exact && other.exact,
    }
  }

  fn trim_newlines(&mut self) {
    if self.exact {
      while self.prefix.last() == Some(&b'\n') {
        self.prefix.pop();
      }
      *self = Self::literal(std::mem::take(&mut self.prefix));
    } else {
      let previous_length = self.suffix.len();
      while self.suffix.last() == Some(&b'\n') {
        self.suffix.pop();
      }
      if self.suffix.is_empty() {
        while self.prefix.last() == Some(&b'\n') {
          self.prefix.pop();
        }
        self.required.remove(&b'\n');
        self.minimum = self.prefix.len().max(self.required.len());
      } else {
        self.minimum = self
          .minimum
          .saturating_sub(previous_length - self.suffix.len());
        self.prefix.truncate(self.minimum);
        self.required.remove(&b'\n');
      }
      self.required.extend(self.prefix.iter().chain(&self.suffix));
    }
  }
}

impl Value {
  pub(in crate::interface::application::engine) fn partial(
    prefix: Vec<u8>,
    suffix: Vec<u8>,
    minimum: usize,
    issue: ValueIssue,
  ) -> Self {
    let required: BTreeSet<_> = prefix.iter().chain(&suffix).copied().collect();
    let minimum = minimum
      .max(prefix.len())
      .max(suffix.len())
      .max(required.len());
    Self::constrained(
      vec![ValueConstraint {
        prefix,
        suffix,
        minimum,
        required,
        exact: false,
      }],
      issue,
    )
  }

  fn constrained(choices: Vec<ValueConstraint>, issue: ValueIssue) -> Self {
    if issue.causes().any(|cause| {
      matches!(
        cause,
        ValueIssue::Recovery
          | ValueIssue::Arithmetic
          | ValueIssue::Nul
          | ValueIssue::SizeLimit
          | ValueIssue::Unspecified
          | ValueIssue::UnspecifiedEscape
          | ValueIssue::LocaleDependentEscape
          | ValueIssue::TransformedSource
      )
    }) {
      return Self::Unknown(issue);
    }
    // First-byte partitions bound loop growth without merging empty and
    // nonempty values.
    let choices = if choices
      .iter()
      .any(|choice| !choice.exact && choice.prefix.is_empty())
    {
      let empty = ValueConstraint::literal(Vec::new());
      let kept = choices.contains(&empty);
      let hull = ValueConstraint::hull_all(
        choices.into_iter().filter(|choice| *choice != empty),
      );
      if kept && !hull.accepts(&[]) {
        vec![empty, hull]
      } else {
        vec![hull]
      }
    } else {
      let mut groups = BTreeMap::<Option<u8>, ValueConstraint>::new();
      for choice in choices {
        groups
          .entry(choice.prefix.first().copied())
          .and_modify(|old| old.hull(&choice))
          .or_insert(choice);
      }
      groups.into_values().collect::<Vec<_>>()
    };
    if let [choice] = choices.as_slice() {
      if choice.exact {
        return Self::Literal(choice.prefix.clone());
      }
      if choice.minimum == 0
        && choice.prefix.is_empty()
        && choice.suffix.is_empty()
        && choice.required.is_empty()
      {
        return Self::Unknown(issue);
      }
    }
    Self::Partial(PartialValue { choices, issue })
  }

  pub(in crate::interface::application::engine) fn constraints(
    &self,
  ) -> Vec<ValueConstraint> {
    match self {
      Self::Literal(bytes) => vec![ValueConstraint::literal(bytes.clone())],
      Self::Partial(value) => value.choices.clone(),
      Self::Unknown(_) => vec![ValueConstraint::any()],
    }
  }

  fn hull(&self) -> ValueConstraint {
    ValueConstraint::hull_all(self.constraints())
  }

  pub(in crate::interface::application::engine) fn known_size(&self) -> usize {
    match self {
      Self::Literal(bytes) => bytes.len(),
      Self::Partial(value) => value
        .choices
        .iter()
        .map(|choice| choice.minimum)
        .max()
        .unwrap_or(0),
      Self::Unknown(_) => 0,
    }
  }

  pub(in crate::interface::application::engine) fn known_prefix(
    &self,
  ) -> Vec<u8> {
    if let Self::Literal(bytes) = self {
      return bytes.clone();
    }
    self.hull().prefix
  }

  pub(in crate::interface::application::engine) fn issue(
    &self,
  ) -> Option<ValueIssue> {
    match self {
      Self::Literal(_) => None,
      Self::Partial(value) => Some(value.issue),
      Self::Unknown(issue) => Some(*issue),
    }
  }

  pub(in crate::interface::application::engine) fn literal(
    &self,
  ) -> Result<&[u8], ValueIssue> {
    match self {
      Self::Literal(bytes) => Ok(bytes),
      _ => Err(self.issue().expect("unknown value")),
    }
  }

  fn combined_issue(
    &self,
    other: &Self,
    cause: Option<ValueIssue>,
  ) -> ValueIssue {
    self
      .issue()
      .into_iter()
      .chain(other.issue())
      .chain(cause)
      .reduce(ValueIssue::combine)
      .unwrap_or(ValueIssue::Dynamic)
  }

  pub(in crate::interface::application::engine) fn empty(
    &self,
  ) -> Option<bool> {
    if let Self::Literal(bytes) = self {
      return Some(bytes.is_empty());
    }
    (!self.can_equal(b"")).then_some(false)
  }

  pub(in crate::interface::application::engine) fn can_equal(
    &self,
    bytes: &[u8],
  ) -> bool {
    match self {
      Self::Literal(value) => value == bytes,
      Self::Partial(value) => {
        value.choices.iter().any(|choice| choice.accepts(bytes))
      }
      Self::Unknown(_) => true,
    }
  }

  pub(in crate::interface::application::engine) fn may_start_with(
    &self,
    prefix: &[u8],
  ) -> bool {
    if let Self::Literal(bytes) = self {
      return bytes.starts_with(prefix);
    }
    self.constraints().iter().any(|choice| {
      if choice.exact {
        choice.prefix.starts_with(prefix)
      } else {
        choice.prefix.starts_with(prefix) || prefix.starts_with(&choice.prefix)
      }
    })
  }

  pub(in crate::interface::application::engine) fn may_be_name(&self) -> bool {
    if let Self::Literal(bytes) = self {
      return !bytes.is_empty() && !bytes.contains(&b'/');
    }
    self.constraints().iter().any(|choice| {
      !choice.required.contains(&b'/')
        && (!choice.exact || !choice.prefix.is_empty())
    })
  }

  pub(in crate::interface::application::engine) fn intersects(
    &self,
    other: &Self,
  ) -> bool {
    let left = self.constraints();
    let right = other.constraints();
    left.iter().any(|left| {
      right.iter().any(|right| {
        if left.exact {
          return right.accepts(&left.prefix);
        }
        if right.exact {
          return left.accepts(&right.prefix);
        }
        (left.prefix.starts_with(&right.prefix)
          || right.prefix.starts_with(&left.prefix))
          && (left.suffix.ends_with(&right.suffix)
            || right.suffix.ends_with(&left.suffix))
      })
    })
  }

  pub(in crate::interface::application::engine) fn covers(
    &self,
    other: &Self,
  ) -> bool {
    if self == other {
      return true;
    }
    if matches!(self, Self::Literal(_)) {
      return false;
    }
    let issue_covers = other.issue().is_none_or(|right| {
      self.issue().is_some_and(|left| left.combine(right) == left)
    });
    if !issue_covers {
      return false;
    }
    let left = self.constraints();
    other
      .constraints()
      .iter()
      .all(|right| left.iter().any(|left| left.covers(right)))
  }

  pub(in crate::interface::application::engine) fn join(
    &mut self,
    other: &Self,
  ) {
    if self != other {
      self.join_choice(other, None);
    }
  }

  pub(in crate::interface::application::engine) fn join_all(
    values: impl IntoIterator<Item = Self>,
    issue: ValueIssue,
  ) -> Self {
    values
      .into_iter()
      .reduce(|mut left, right| {
        left.join(&right);
        left
      })
      .unwrap_or(Self::Unknown(issue))
  }

  pub(in crate::interface::application::engine) fn join_choice(
    &mut self,
    other: &Self,
    cause: Option<ValueIssue>,
  ) {
    let issue = self.combined_issue(other, cause);
    let mut choices = self.constraints();
    choices.extend(other.constraints());
    *self = Self::constrained(choices, issue);
  }

  pub(in crate::interface::application::engine) fn concat(
    &mut self,
    other: &Self,
    limit: usize,
  ) {
    if let (Self::Literal(left), Self::Literal(right)) = (&mut *self, other) {
      if right.len() > limit.saturating_sub(left.len()) {
        *self = Self::Unknown(ValueIssue::SizeLimit);
      } else {
        left.extend_from_slice(right);
      }
      return;
    }
    if *other == Self::Literal(Vec::new()) {
      return;
    }
    if *self == Self::Literal(Vec::new()) {
      *self = other.clone();
    } else {
      let issue = self.combined_issue(other, None);
      let right = other.hull();
      let mut choices = Vec::new();
      for left in self.constraints() {
        if left.exact && left.prefix.is_empty() {
          choices.extend(other.constraints());
        } else {
          choices.push(left.concat(&right));
        }
      }
      *self = Self::constrained(choices, issue);
    }
    if self.known_size() > limit {
      *self =
        Self::Unknown(self.issue().map_or(ValueIssue::SizeLimit, |issue| {
          issue.combine(ValueIssue::SizeLimit)
        }));
    }
  }

  pub(in crate::interface::application::engine) fn trim_newlines(&mut self) {
    if let Self::Literal(bytes) = self {
      while bytes.last() == Some(&b'\n') {
        bytes.pop();
      }
      return;
    }
    let issue = self.issue().unwrap_or(ValueIssue::Dynamic);
    let mut choices = self.constraints();
    for choice in &mut choices {
      choice.trim_newlines();
    }
    *self = Self::constrained(choices, issue);
  }

  pub(in crate::interface::application::engine) fn remove_prefix(
    &self,
    length: usize,
  ) -> Self {
    let mut choices = self.constraints();
    if choices.iter().any(|choice| choice.prefix.len() < length) {
      return Self::Unknown(self.issue().unwrap_or(ValueIssue::Dynamic));
    }
    for choice in &mut choices {
      let removed = choice.prefix.drain(..length).collect::<Vec<_>>();
      if choice.exact {
        *choice = ValueConstraint::literal(std::mem::take(&mut choice.prefix));
      } else {
        choice.minimum = choice.minimum.saturating_sub(length);
        let retained = choice.suffix.len().min(choice.minimum);
        choice.suffix.drain(..choice.suffix.len() - retained);
        choice.required.retain(|byte| !removed.contains(byte));
        choice
          .required
          .extend(choice.prefix.iter().chain(&choice.suffix));
      }
    }
    Self::constrained(choices, self.issue().unwrap_or(ValueIssue::Dynamic))
  }

  pub(in crate::interface::application::engine) fn before(
    &self,
    delimiter: u8,
  ) -> Self {
    let mut choices = self.constraints();
    for choice in &mut choices {
      if let Some(index) = choice.prefix.iter().position(|b| *b == delimiter) {
        *choice = ValueConstraint::literal(choice.prefix[..index].to_vec());
      } else if !choice.exact {
        choice.suffix.clear();
        choice.minimum = choice.prefix.len();
        choice.required = choice.prefix.iter().copied().collect();
      }
    }
    Self::constrained(choices, self.issue().unwrap_or(ValueIssue::Dynamic))
  }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ValueIssue {
  Dynamic,
  Recovery,
  TransformedSource,
  UnspecifiedEscape,
  LocaleDependentEscape,
  Nul,
  EmptyOperand,
  MissingOperand,
  SizeLimit,
  FieldSplitting,
  PathnameExpansion,
  UnsupportedExpansion,
  Arithmetic,
  Locale,
  Unspecified,
  Multiple(u32),
}

impl ValueIssue {
  const CAUSES: [Self; 15] = [
    Self::Dynamic,
    Self::Recovery,
    Self::TransformedSource,
    Self::UnspecifiedEscape,
    Self::LocaleDependentEscape,
    Self::Nul,
    Self::EmptyOperand,
    Self::MissingOperand,
    Self::SizeLimit,
    Self::FieldSplitting,
    Self::PathnameExpansion,
    Self::UnsupportedExpansion,
    Self::Arithmetic,
    Self::Locale,
    Self::Unspecified,
  ];

  fn mask(self) -> u32 {
    match self {
      Self::Multiple(mask) => mask,
      issue => {
        1 << Self::CAUSES
          .iter()
          .position(|cause| *cause == issue)
          .expect("value cause")
      }
    }
  }

  pub(in crate::interface::application::engine) fn combine(
    self,
    other: Self,
  ) -> Self {
    if self == other {
      self
    } else {
      Self::Multiple(self.mask() | other.mask())
    }
  }

  pub fn causes(self) -> impl Iterator<Item = Self> {
    Self::CAUSES
      .into_iter()
      .enumerate()
      .filter_map(move |(i, cause)| {
        (self.mask() & (1 << i) != 0).then_some(cause)
      })
  }
}

#[cfg(test)]
#[path = "../../../../tests/implementation/value.rs"]
mod tests;
