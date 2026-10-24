use super::alternatives::Alternatives;
use crate::interface::application::engine::word::FieldCount;
use imbl::{OrdMap, ordmap::DiffItem};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::interface::application::engine::{
  Alternative, AttributeState, EffectTarget, InitialBinding,
  InitialEnvironment, Resolution, ResolutionIssue, SymbolId, SymbolKind, Value,
  ValueIssue,
};

pub(super) fn merge_value(into: &mut Arc<Value>, other: &Arc<Value>) {
  if into != other {
    Arc::make_mut(into).join(other);
  }
}

pub(in crate::interface::application::engine::analysis) fn definition(
  id: SymbolId,
) -> Resolution {
  Resolution {
    candidates: BTreeSet::from([id]),
    ..Resolution::default()
  }
}

pub(in crate::interface::application::engine::analysis) fn alternative(
  value: Alternative,
) -> Resolution {
  Resolution {
    alternatives: BTreeSet::from([value]),
    ..Resolution::default()
  }
}

fn initial(binding: InitialBinding) -> Resolution {
  match binding {
    InitialBinding::Absent => alternative(Alternative::Unset),
    InitialBinding::External => alternative(Alternative::External),
    InitialBinding::Unknown => Resolution {
      alternatives: BTreeSet::from([Alternative::External, Alternative::Unset]),
      issues: BTreeSet::from([ResolutionIssue::InitialState]),
      ..Resolution::default()
    },
  }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::interface::application::engine::analysis) struct Binding {
  pub resolution: Resolution,
  pub value: Arc<Value>,
}

impl Binding {
  fn covers(&self, other: &Self) -> bool {
    self.value.covers(&other.value)
      && self
        .resolution
        .candidates
        .is_superset(&other.resolution.candidates)
      && self
        .resolution
        .alternatives
        .is_superset(&other.resolution.alternatives)
      && self.resolution.issues.is_superset(&other.resolution.issues)
  }

  pub fn new(resolution: Resolution) -> Self {
    let value = if resolution == alternative(Alternative::Unset) {
      Value::Literal(Vec::new())
    } else {
      Value::Unknown(ValueIssue::Dynamic)
    };
    Self {
      resolution,
      value: Arc::new(value),
    }
  }

  pub fn join(&mut self, other: &Self) {
    self.resolution.join(&other.resolution);
    merge_value(&mut self.value, &other.value);
  }

  pub fn taint(&mut self, issue: ResolutionIssue, value_issue: ValueIssue) {
    self
      .resolution
      .alternatives
      .extend([Alternative::External, Alternative::Unset]);
    self.resolution.issues.insert(issue);
    self.value = Arc::new(Value::Unknown(
      self
        .value
        .issue()
        .map_or(value_issue, |previous| previous.combine(value_issue)),
    ));
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::interface::application::engine::analysis) struct Names {
  pub values: OrdMap<Vec<u8>, Binding>,
  pub fallback: Binding,
  pub patterns: BTreeSet<Value>,
  pub uncertain: Option<Binding>,
}

impl Names {
  pub fn differences<'a>(
    &'a self,
    other: &'a Self,
  ) -> impl Iterator<Item = (&'a Vec<u8>, &'a Binding, &'a Binding)> {
    self.values.diff(&other.values).map(|change| match change {
      DiffItem::Add(name, right) => (name, self.get(name), right),
      DiffItem::Remove(name, left) => (name, left, other.get(name)),
      DiffItem::Update {
        old: (name, left),
        new: (_, right),
      } => (name, left, right),
    })
  }

  pub fn get(&self, name: &[u8]) -> &Binding {
    self.values.get(name).unwrap_or_else(|| {
      if self.patterns.iter().any(|pattern| pattern.can_equal(name)) {
        self.uncertain.as_ref().unwrap_or(&self.fallback)
      } else {
        &self.fallback
      }
    })
  }

  pub fn join(&mut self, other: &Self) {
    let changes: Vec<_> = self
      .differences(other)
      .filter_map(|(name, left, right)| {
        if self.values.contains_key(name) && left.covers(right) {
          return None;
        }
        let mut value = left.clone();
        value.join(right);
        Some((name.clone(), value))
      })
      .collect();
    for (name, value) in changes {
      self.values.insert(name, value);
    }
    if self.uncertain.is_some() || other.uncertain.is_some() {
      let mut uncertain = self
        .uncertain
        .clone()
        .unwrap_or_else(|| self.fallback.clone());
      uncertain.join(other.uncertain.as_ref().unwrap_or(&other.fallback));
      self.uncertain = Some(uncertain);
      self.patterns.extend(other.patterns.iter().cloned());
    }
    self.fallback.join(&other.fallback);
  }

  /// No pattern means all names; explicit `kept` bindings survive.
  fn invalidate(
    &mut self,
    pattern: Option<Value>,
    kept: impl Fn(&[u8]) -> bool,
    issue: ResolutionIssue,
    value_issue: ValueIssue,
  ) {
    for (name, mut binding) in self.values.clone() {
      if pattern
        .as_ref()
        .is_none_or(|pattern| pattern.can_equal(&name))
        && !kept(&name)
      {
        binding.taint(issue, value_issue);
        self.values.insert(name, binding);
      }
    }
    if let Some(pattern) = pattern {
      self.restrict(pattern, issue, value_issue);
    } else {
      self.patterns.clear();
      self.uncertain = None;
      self.fallback.taint(issue, value_issue);
    }
  }

  pub fn restrict(
    &mut self,
    pattern: Value,
    issue: ResolutionIssue,
    value_issue: ValueIssue,
  ) {
    let mut fallback = self
      .uncertain
      .take()
      .unwrap_or_else(|| self.fallback.clone());
    fallback.taint(issue, value_issue);
    self.uncertain = Some(fallback);
    self.patterns.insert(pattern);
  }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::interface::application::engine::analysis) struct Positionals {
  pub fields: Vec<(Value, FieldCount)>,
}

impl Positionals {
  pub fn unknown(issue: ValueIssue) -> Self {
    Self {
      fields: vec![(
        Value::Unknown(issue),
        FieldCount::Unknown { minimum: 0, issue },
      )],
    }
  }

  pub fn minimum(&self) -> usize {
    self
      .fields
      .iter()
      .map(|(_, count)| count.minimum())
      .fold(0, usize::saturating_add)
  }

  pub fn parameter(&self, index: usize) -> (Value, Option<bool>) {
    let mut minimum = 0usize;
    let mut uncertain = None;
    let mut result: Option<Value> = None;
    for (value, count) in &self.fields {
      if minimum > index {
        break;
      }
      if uncertain.is_none() && count.boundary().is_ok() && minimum == index {
        return (value.clone(), Some(false));
      }
      uncertain = uncertain
        .into_iter()
        .chain(count.boundary().err())
        .reduce(ValueIssue::combine);
      if uncertain.is_some() {
        if let Some(result) = &mut result {
          if result != value {
            result.join_choice(value, uncertain);
          }
        } else {
          result = Some(value.clone());
        }
      }
      minimum = minimum.saturating_add(count.minimum());
    }
    let Some(mut result) = result else {
      return (Value::Literal(Vec::new()), Some(true));
    };
    if index < self.minimum() {
      (result, Some(false))
    } else {
      let empty = Value::Literal(Vec::new());
      if result != empty {
        result.join_choice(&empty, uncertain);
      }
      (result, None)
    }
  }

  pub fn present(&self, index: usize) -> Result<Value, ValueIssue> {
    let (value, absent) = self.parameter(index);
    if absent == Some(false) {
      Ok(value)
    } else {
      Err(value.issue().unwrap_or(ValueIssue::Dynamic))
    }
  }

  pub fn count(&self) -> Value {
    self
      .fields
      .iter()
      .filter_map(|(_, count)| count.boundary().err())
      .reduce(ValueIssue::combine)
      .map_or_else(
        || Value::Literal(self.fields.len().to_string().into_bytes()),
        Value::Unknown,
      )
  }

  pub fn issue(&self) -> Option<ValueIssue> {
    self
      .fields
      .iter()
      .flat_map(|(value, count)| {
        value.issue().into_iter().chain(count.boundary().err())
      })
      .reduce(ValueIssue::combine)
  }

  fn join(&mut self, other: &Self) {
    if self == other {
      return;
    }
    if self.fields.len() == other.fields.len()
      && self
        .fields
        .iter()
        .zip(&other.fields)
        .all(|((_, left), (_, right))| {
          left.boundary().is_ok() == right.boundary().is_ok()
        })
    {
      for ((left, count), (right, other_count)) in
        self.fields.iter_mut().zip(&other.fields)
      {
        left.join(right);
        if let Err(issue) = count.boundary() {
          *count = FieldCount::Unknown {
            minimum: count.minimum().min(other_count.minimum()),
            issue: issue.combine(other_count.boundary().unwrap_err()),
          };
        }
      }
      return;
    }
    let prefix = self
      .fields
      .iter()
      .zip(&other.fields)
      .take_while(|(left, right)| left == right && left.1.boundary().is_ok())
      .count();
    let suffix = self.fields[prefix..]
      .iter()
      .rev()
      .zip(other.fields[prefix..].iter().rev())
      .take_while(|(left, right)| left == right && left.1.boundary().is_ok())
      .count();
    let mut fields = self.fields[..prefix].to_vec();
    let left = &self.fields[prefix..self.fields.len() - suffix];
    let right = &other.fields[prefix..other.fields.len() - suffix];
    let known = left
      .iter()
      .zip(right)
      .take_while(|((_, left), (_, right))| {
        left.boundary().is_ok() && right.boundary().is_ok()
      })
      .count();
    for ((left, _), (right, _)) in left[..known].iter().zip(&right[..known]) {
      let mut value = left.clone();
      value.join(right);
      fields.push((value, FieldCount::One));
    }
    let left = &left[known..];
    let right = &right[known..];
    if !left.is_empty() || !right.is_empty() {
      let minimum = |fields: &[(Value, FieldCount)]| {
        fields
          .iter()
          .map(|(_, count)| count.minimum())
          .fold(0, usize::saturating_add)
      };
      let issue = left
        .iter()
        .chain(right)
        .filter_map(|(_, count)| count.boundary().err())
        .reduce(ValueIssue::combine)
        .unwrap_or(ValueIssue::Dynamic);
      let value = left
        .iter()
        .chain(right)
        .map(|(value, _)| value.clone())
        .reduce(|mut left, right| {
          left.join(&right);
          left
        })
        .unwrap_or(Value::Unknown(issue));
      fields.push((
        value,
        FieldCount::Unknown {
          minimum: minimum(left).min(minimum(right)),
          issue,
        },
      ));
    }
    fields.extend_from_slice(&self.fields[self.fields.len() - suffix..]);
    self.fields = fields;
  }

  pub fn shift(&mut self, mut amount: usize) -> bool {
    if amount > self.minimum() {
      return false;
    }
    let mut removed = 0;
    let mut uncertain: Option<(Value, ValueIssue)> = None;
    while amount > 0 {
      let (value, count) = &mut self.fields[removed];
      if let Err(issue) = count.boundary() {
        if let Some((previous, cause)) = &mut uncertain {
          previous.join(value);
          *cause = cause.combine(issue);
        } else if amount <= count.minimum() {
          *count = FieldCount::Unknown {
            minimum: count.minimum() - amount,
            issue,
          };
          break;
        } else {
          uncertain = Some((value.clone(), issue));
        }
      } else if let Some((previous, _)) = &mut uncertain {
        previous.join(value);
      }
      let consumed = count.minimum().min(amount);
      amount -= consumed;
      if consumed < count.minimum() {
        if let FieldCount::Unknown { minimum, .. } = count {
          *minimum -= consumed;
        }
        break;
      }
      removed += 1;
    }
    self.fields.drain(..removed);
    if let Some((value, issue)) = uncertain {
      self
        .fields
        .insert(0, (value, FieldCount::Unknown { minimum: 0, issue }));
    }
    true
  }

  pub fn invalidate(&mut self, issue: ValueIssue) {
    let count = self
      .count()
      .issue()
      .map_or(issue, |count| count.combine(issue));
    let value = self.issue().map_or(count, |value| value.combine(count));
    self.fields = vec![(
      Value::Unknown(value),
      FieldCount::Unknown {
        minimum: 0,
        issue: count,
      },
    )];
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::interface::application::engine::analysis) struct Environment {
  pub alternatives: Alternatives,
  pub variables: Names,
  pub functions: Names,
  pub order: BTreeSet<EffectTarget>,
  pub readonly: OrdMap<Vec<u8>, bool>,
  /// Names matching these values may have become readonly untracked.
  pub unknown_attributes: BTreeSet<Value>,
  pub errexit: Option<bool>,
  pub nounset: Option<bool>,
  pub noglob: Option<bool>,
  pub cwd: Value,
  pub positional: Positionals,
  pub special: BTreeMap<Vec<u8>, Value>,
  pub substitution_status: Option<Value>,
  /// Errors in this command's expansions, assignments and redirections.
  pub failure: CommandFailure,
  pub option_cursor: Option<(Positionals, usize)>,
  pub options_reset: bool,
  pub io: Io,
  pub directory_variables_modified: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::interface::application::engine::analysis) enum CommandFailure {
  #[default]
  None,
  Definite,
  Possible(ValueIssue),
}

impl CommandFailure {
  pub fn uncertain(&mut self, issue: ValueIssue) {
    *self = match *self {
      Self::None => Self::Possible(issue),
      Self::Possible(previous) => Self::Possible(previous.combine(issue)),
      Self::Definite => Self::Definite,
    };
  }

  pub fn then(&mut self, other: Self) {
    match other {
      Self::Definite => *self = Self::Definite,
      Self::Possible(issue) => self.uncertain(issue),
      Self::None => {}
    }
  }

  fn join(&mut self, other: Self) {
    if *self == other {
      return;
    }
    *self = Self::Possible(match (*self, other) {
      (Self::Possible(left), Self::Possible(right)) => left.combine(right),
      (Self::Possible(issue), _) | (_, Self::Possible(issue)) => issue,
      _ => ValueIssue::Dynamic,
    });
  }
}

impl Environment {
  /// Preserves definite readonly bindings and unchanged branch choices.
  pub(in crate::interface::application::engine::analysis) fn invalidate(
    &mut self,
    target: EffectTarget,
    issue: ResolutionIssue,
    value_issue: ValueIssue,
  ) {
    match target {
      EffectTarget::Variable(name) => {
        if self.taint_variable(&name, issue, value_issue) {
          self.variable_changed(&name, None);
        }
      }
      EffectTarget::VariablePattern(pattern) => {
        self.invalidate_names(false, Some(pattern), issue, value_issue);
      }
      EffectTarget::Variables => {
        self.invalidate_names(false, None, issue, value_issue);
      }
      EffectTarget::Function(name) => {
        self.alternatives.forget_function(&name);
        let mut binding = self.functions.get(&name).clone();
        binding.taint(issue, value_issue);
        self.functions.values.insert(name, binding);
      }
      EffectTarget::FunctionPattern(pattern) => {
        self.invalidate_names(true, Some(pattern), issue, value_issue);
      }
      EffectTarget::Functions => {
        self.invalidate_names(true, None, issue, value_issue);
      }
      EffectTarget::Attribute(name) => {
        if !self.readonly.contains_key(&name) {
          self.readonly.insert(name, false);
        }
      }
      EffectTarget::AttributePattern(pattern) => {
        self.unknown_attributes.insert(pattern);
      }
      EffectTarget::Attributes => {
        self.unknown_attributes.insert(Value::Unknown(value_issue));
      }
      EffectTarget::Directory => {
        self.cwd = Value::Unknown(value_issue);
        for name in [b"PWD".as_slice(), b"OLDPWD"] {
          self.taint_variable(name, issue, value_issue);
        }
      }
      EffectTarget::Positionals => {
        self.positional.invalidate(value_issue);
        self.alternatives.forget_positionals();
      }
      EffectTarget::Options => {
        self.errexit = None;
        self.nounset = None;
        self.noglob = None;
        self
          .special
          .insert(b"-".to_vec(), Value::Unknown(value_issue));
      }
      EffectTarget::SpecialParameters => {
        for name in [b"?".as_slice(), b"-", b"!"] {
          self
            .special
            .insert(name.to_vec(), Value::Unknown(value_issue));
        }
      }
    }
  }

  fn invalidate_names(
    &mut self,
    functions: bool,
    pattern: Option<Value>,
    issue: ResolutionIssue,
    value_issue: ValueIssue,
  ) {
    let matches = |name: &[u8]| {
      pattern
        .as_ref()
        .is_none_or(|pattern| pattern.can_equal(name))
    };
    if !functions {
      self.variables_changed(pattern.as_ref());
      for (name, definite) in &self.readonly {
        if *definite
          && matches(name)
          && !self.variables.values.contains_key(name)
        {
          let binding = self.variables.get(name).clone();
          self.variables.values.insert(name.clone(), binding);
        }
      }
    }
    let readonly = &self.readonly;
    let kept = |name: &[u8]| !functions && readonly.get(name) == Some(&true);
    self
      .alternatives
      .forget_names(functions, |name| matches(name) && !kept(name));
    let names = if functions {
      &mut self.functions
    } else {
      &mut self.variables
    };
    names.invalidate(pattern, kept, issue, value_issue);
  }

  /// Definite readonly bindings survive; returns whether the value may change.
  fn taint_variable(
    &mut self,
    name: &[u8],
    issue: ResolutionIssue,
    value_issue: ValueIssue,
  ) -> bool {
    if self.readonly(name) == Some(true) {
      return false;
    }
    self.alternatives.forget(name);
    let mut binding = self.variables.get(name).clone();
    binding.taint(issue, value_issue);
    self.variables.values.insert(name.to_vec(), binding);
    true
  }

  /// Retains the previous binding wherever the variable may be readonly.
  pub(in crate::interface::application::engine::analysis) fn assign(
    &mut self,
    name: &[u8],
    definition: Resolution,
    value: Value,
  ) {
    let readonly = self.readonly(name);
    if readonly == Some(true) {
      return;
    }
    self.variable_changed(name, readonly.is_none().then_some(&value));
    self.alternatives.forget(name);
    let mut binding = Binding {
      resolution: definition,
      value: Arc::new(value),
    };
    if readonly.is_some() {
      binding.join(self.variables.get(name));
    }
    self.variables.values.insert(name.to_vec(), binding);
  }

  pub fn variable_changed(&mut self, name: &[u8], value: Option<&Value>) {
    match name {
      b"OPTIND" => {
        self.option_cursor = None;
        self.options_reset = value == Some(&Value::Literal(b"1".to_vec()));
      }
      b"PWD" | b"OLDPWD" => self.directory_variables_modified = true,
      _ => {}
    }
  }

  fn variables_changed(&mut self, pattern: Option<&Value>) {
    for name in [b"OPTIND".as_slice(), b"PWD", b"OLDPWD"] {
      if pattern.is_none_or(|pattern| pattern.can_equal(name))
        && self.readonly(name) != Some(true)
      {
        self.variable_changed(name, None);
      }
    }
  }

  pub fn locale(&self, category: &[u8]) -> Option<&[u8]> {
    for name in [b"LC_ALL".as_slice(), category, b"LANG"] {
      if self.uncertain_variable(name) {
        return None;
      }
      match self.variables.get(name).value.as_ref() {
        Value::Literal(bytes) if !bytes.is_empty() => return Some(bytes),
        Value::Literal(_) => {}
        Value::Unknown(_) | Value::Partial(_) => return None,
      }
    }
    Some(b"C")
  }

  pub fn portable_collation(&self) -> bool {
    matches!(self.locale(b"LC_COLLATE"), Some(b"C" | b"POSIX"))
      && matches!(self.locale(b"LC_CTYPE"), Some(b"C" | b"POSIX"))
  }

  pub fn new(input: &InitialEnvironment) -> Self {
    Self {
      alternatives: Alternatives::default(),
      variables: Names {
        patterns: BTreeSet::new(),
        uncertain: None,
        values: input
          .environment
          .iter()
          .map(|(name, value)| {
            (
              name.clone(),
              Binding {
                resolution: alternative(Alternative::Environment),
                value: Arc::new(if value.contains(&0) {
                  Value::Unknown(ValueIssue::Nul)
                } else {
                  Value::Literal(value.clone())
                }),
              },
            )
          })
          .chain(input.values.iter().map(|(name, bytes)| {
            (
              name.clone(),
              Binding {
                resolution: alternative(Alternative::External),
                value: Arc::new(Value::Literal(bytes.clone())),
              },
            )
          }))
          .chain(input.variables.iter().map(|(name, binding)| {
            (name.clone(), Binding::new(initial(*binding)))
          }))
          .collect(),
        fallback: Binding::new(initial(input.other_variables)),
      },
      functions: Names {
        patterns: BTreeSet::new(),
        uncertain: None,
        values: input
          .functions
          .iter()
          .map(|(name, binding)| {
            (name.clone(), Binding::new(initial(*binding)))
          })
          .collect(),
        fallback: Binding::new(initial(input.other_functions)),
      },
      positional: input.positional.as_ref().map_or_else(
        || Positionals::unknown(ValueIssue::Dynamic),
        |args| Positionals {
          fields: args
            .iter()
            .cloned()
            .map(|value| (Value::Literal(value), FieldCount::One))
            .collect(),
        },
      ),
      special: input
        .special
        .iter()
        .map(|(k, v)| (k.clone(), Value::Literal(v.clone())))
        .collect(),
      directory_variables_modified: false,
      substitution_status: None,
      failure: CommandFailure::None,
      option_cursor: None,
      options_reset: input
        .values
        .get(b"OPTIND".as_slice())
        .or_else(|| input.environment.get(b"OPTIND".as_slice()))
        .is_some_and(|value| value == b"1"),
      io: Io::default(),
      order: BTreeSet::new(),
      readonly: OrdMap::new(),
      unknown_attributes: BTreeSet::new(),
      errexit: input.errexit,
      nounset: input.nounset,
      noglob: input.noglob,
      cwd: input
        .cwd
        .clone()
        .map_or(Value::Unknown(ValueIssue::Dynamic), Value::Literal),
    }
  }

  pub fn join(&mut self, other: &Self) {
    let alternatives = Alternatives::joined(self, other);
    self.join_values(other);
    self.alternatives = alternatives;
  }

  /// Converges repeated execution without introducing a new path choice.
  pub fn widen(&mut self, other: &Self) {
    self.alternatives.retain_common(&other.alternatives);
    self.join_values(other);
  }

  fn join_values(&mut self, other: &Self) {
    self.failure.join(other.failure);
    if self.option_cursor != other.option_cursor {
      self.option_cursor = None;
    }
    self.options_reset &= other.options_reset;
    self.io.join_input(&other.io);
    if self.io.stdout_redirected != other.io.stdout_redirected
      || self.io.stderr_captured != other.io.stderr_captured
    {
      if let Some(output) = &mut self.io.output {
        output.fail(ValueIssue::Dynamic);
      }
      self.io.stdout_redirected = false;
      self.io.stderr_captured = true;
    }
    match (&mut self.io.output, &other.io.output) {
      (Some(left), Some(right)) => left.join(right),
      (Some(left), None) => left.fail(ValueIssue::Dynamic),
      (None, Some(right)) => {
        let mut output = right.clone();
        output.fail(ValueIssue::Dynamic);
        self.io.output = Some(output);
      }
      _ => {}
    }
    match (&mut self.substitution_status, &other.substitution_status) {
      (Some(left), Some(right)) => left.join(right),
      (left, right) if *left != *right => {
        *left = Some(Value::Unknown(ValueIssue::Dynamic));
      }
      _ => {}
    }
    if self.errexit != other.errexit {
      self.errexit = None;
    }
    if self.nounset != other.nounset {
      self.nounset = None;
    }
    if self.noglob != other.noglob {
      self.noglob = None;
    }
    self.positional.join(&other.positional);
    for (name, value) in &mut self.special {
      value.join(
        other
          .special
          .get(name)
          .unwrap_or(&Value::Unknown(ValueIssue::Dynamic)),
      );
    }
    self.directory_variables_modified |= other.directory_variables_modified;
    self.cwd.join(&other.cwd);
    self.variables.join(&other.variables);
    self.functions.join(&other.functions);
    self.order.extend(other.order.iter().cloned());
    self
      .unknown_attributes
      .extend(other.unknown_attributes.iter().cloned());
    let names: BTreeSet<_> = self
      .readonly
      .keys()
      .chain(other.readonly.keys())
      .cloned()
      .collect();
    for name in names {
      let definite = self.readonly.get(&name) == Some(&true)
        && other.readonly.get(&name) == Some(&true);
      self.readonly.insert(name, definite);
    }
  }

  pub fn status(&self) -> Value {
    self.parameter(b"?").0
  }

  pub fn set_status(&mut self, status: Value) {
    self.special.insert(b"?".to_vec(), status);
  }

  pub fn complete_command(&mut self) {
    match self.failure {
      CommandFailure::None => {}
      CommandFailure::Definite => {
        self.set_status(Value::Literal(b"1".to_vec()));
      }
      CommandFailure::Possible(issue) => {
        self.set_status(Value::Unknown(
          self
            .status()
            .issue()
            .map_or(issue, |status| status.combine(issue)),
        ));
      }
    }
  }

  pub fn require_parameter(&mut self, name: &[u8]) {
    if self.nounset == Some(false) || matches!(name, b"@" | b"*") {
      return;
    }
    let (value, absent) = self.parameter(name);
    match (self.nounset, absent) {
      (Some(true), Some(true)) => self.failure = CommandFailure::Definite,
      (_, Some(false)) => {}
      _ => self
        .failure
        .uncertain(value.issue().unwrap_or(ValueIssue::Dynamic)),
    }
  }

  pub fn readonly(&self, name: &[u8]) -> Option<bool> {
    self
      .readonly
      .get(name)
      .copied()
      .or_else(|| self.unknown_attribute(name).then_some(false))
  }

  fn unknown_attribute(&self, name: &[u8]) -> bool {
    self
      .unknown_attributes
      .iter()
      .any(|pattern| pattern.can_equal(name))
  }

  pub fn readonly_attribute(&self, name: &[u8]) -> AttributeState {
    match self.readonly.get(name) {
      Some(true) => AttributeState::Known(true),
      Some(false) => AttributeState::Conditional,
      None if self.unknown_attribute(name) => AttributeState::Unknown,
      None if self.variables.get(name).resolution.issues.is_empty() => {
        AttributeState::Known(false)
      }
      None => AttributeState::Unknown,
    }
  }

  pub fn uncertain_variable(&self, name: &[u8]) -> bool {
    self
      .order
      .iter()
      .any(|target| target.covers(SymbolKind::Variable, name))
  }

  pub(in crate::interface::application::engine::analysis) fn parameter(
    &self,
    name: &[u8],
  ) -> (Value, Option<bool>) {
    if name == b"#" {
      return (self.positional.count(), Some(false));
    }
    if name != b"0" && name.iter().all(u8::is_ascii_digit) && !name.is_empty() {
      let index = std::str::from_utf8(name)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .and_then(|n| n.checked_sub(1));
      return self.positional.parameter(index.unwrap_or(usize::MAX));
    }
    if matches!(name, b"?" | b"-" | b"$" | b"!" | b"0") {
      return (
        self
          .special
          .get(name)
          .cloned()
          .unwrap_or(Value::Unknown(ValueIssue::Dynamic)),
        if name == b"!" { None } else { Some(false) },
      );
    }
    if matches!(name, b"@" | b"*") {
      return (Value::Unknown(ValueIssue::Unspecified), None);
    }
    if self.uncertain_variable(name) {
      return (Value::Unknown(ValueIssue::Dynamic), None);
    }
    let binding = self.variables.get(name);
    let resolution = &binding.resolution;
    let absent = if !resolution.issues.is_empty() {
      None
    } else if resolution.candidates.is_empty()
      && resolution.alternatives == [Alternative::Unset].into()
    {
      Some(true)
    } else if !resolution.alternatives.contains(&Alternative::Unset) {
      Some(false)
    } else {
      None
    };
    (binding.value.as_ref().clone(), absent)
  }

  pub(in crate::interface::application::engine::analysis) fn ifs(
    &self,
  ) -> Value {
    let default = Value::Literal(b" \t\n".to_vec());
    match self.parameter(b"IFS") {
      (_, Some(true)) => default,
      (value, Some(false)) => value,
      (mut value, None) => {
        value.join(&default);
        value
      }
    }
  }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::interface::application::engine::analysis) struct Output {
  chunks: imbl::Vector<Vec<u8>>,
  bytes: usize,
  constraint: Option<Value>,
}

impl Output {
  pub fn fail(&mut self, issue: ValueIssue) {
    self.constraint = Some(Value::Unknown(
      self
        .constraint
        .as_ref()
        .and_then(Value::issue)
        .map_or(issue, |old| old.combine(issue)),
    ));
    self.chunks.clear();
    self.bytes = 0;
  }
  pub fn append(&mut self, value: Value, limit: usize) {
    if let Some(constraint) = &mut self.constraint {
      constraint.concat(&value, limit);
    } else if let Value::Literal(bytes) = value {
      if bytes.len() > limit.saturating_sub(self.bytes) {
        self.fail(ValueIssue::SizeLimit);
      } else if !bytes.is_empty() {
        self.bytes += bytes.len();
        self.chunks.push_back(bytes);
      }
    } else {
      let mut constraint = self.value();
      constraint.concat(&value, limit);
      self.chunks.clear();
      self.bytes = 0;
      self.constraint = Some(constraint);
    }
  }
  pub fn value(&self) -> Value {
    self.constraint.clone().unwrap_or_else(|| {
      Value::Literal(self.chunks.iter().flatten().copied().collect())
    })
  }
  fn join(&mut self, other: &Self) {
    if self != other {
      let mut value = self.value();
      value.join(&other.value());
      *self = Self::default();
      self.append(value, usize::MAX);
    }
  }
}

impl Environment {
  pub fn emit(&mut self, value: Value, limit: usize) {
    if !self.io.stdout_redirected
      && let Some(output) = &mut self.io.output
    {
      output.append(value, limit);
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::interface::application::engine::analysis) enum Transfer {
  Next,
  Break(u32),
  Continue(u32),
  Return,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::interface::application::engine::analysis) struct Flow(
  BTreeMap<Transfer, Environment>,
);

impl Flow {
  pub fn next(environment: Environment) -> Self {
    Self::single(Transfer::Next, environment)
  }

  pub fn single(transfer: Transfer, environment: Environment) -> Self {
    Self(BTreeMap::from([(transfer, environment)]))
  }

  pub fn take(&mut self, transfer: Transfer) -> Option<Environment> {
    self.0.remove(&transfer)
  }

  pub fn get(&self, transfer: Transfer) -> Option<&Environment> {
    self.0.get(&transfer)
  }

  pub fn get_mut(&mut self, transfer: Transfer) -> Option<&mut Environment> {
    self.0.get_mut(&transfer)
  }

  pub fn environments(&self) -> impl Iterator<Item = &Environment> {
    self.0.values()
  }

  pub fn environments_mut(&mut self) -> impl Iterator<Item = &mut Environment> {
    self.0.values_mut()
  }

  pub fn merged(self) -> Option<Environment> {
    self.0.into_values().reduce(|mut left, right| {
      left.join(&right);
      left
    })
  }

  /// Continues after a function or `.` boundary, where `return` ends.
  pub fn returned(mut self) -> Self {
    if let Some(environment) = self.take(Transfer::Return) {
      self.add(Transfer::Next, environment);
    }
    self
  }

  pub fn add(&mut self, transfer: Transfer, environment: Environment) {
    self
      .0
      .entry(transfer)
      .and_modify(|old| old.join(&environment))
      .or_insert(environment);
  }

  pub fn join(&mut self, other: Self) {
    for (transfer, environment) in other.0 {
      self.add(transfer, environment);
    }
  }
}

impl IntoIterator for Flow {
  type Item = (Transfer, Environment);
  type IntoIter = std::collections::btree_map::IntoIter<Transfer, Environment>;

  fn into_iter(self) -> Self::IntoIter {
    self.0.into_iter()
  }
}

#[cfg(test)]
#[path = "../../../../../../tests/implementation/state.rs"]
mod tests;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::interface::application::engine::analysis) struct Io {
  pub input: Input,
  pub input_order_unknown: bool,
  saved_inputs: imbl::Vector<(Input, bool)>,
  pub output: Option<Output>,
  pub stdout_redirected: bool,
  pub stderr_captured: bool,
}

type Input = Result<(Arc<Vec<u8>>, usize), ValueIssue>;

impl Default for Io {
  fn default() -> Self {
    Self {
      input: Err(ValueIssue::Dynamic),
      input_order_unknown: false,
      saved_inputs: imbl::Vector::new(),
      output: None,
      stdout_redirected: false,
      stderr_captured: false,
    }
  }
}

fn input_issue(left: &Input, right: &Input) -> ValueIssue {
  left
    .as_ref()
    .err()
    .into_iter()
    .chain(right.as_ref().err())
    .copied()
    .reduce(ValueIssue::combine)
    .unwrap_or(ValueIssue::Dynamic)
}

impl Io {
  fn join_input(&mut self, other: &Self) {
    let same_stream = match (&self.input, &other.input) {
      (Ok((left, _)), Ok((right, _))) => Arc::ptr_eq(left, right),
      (Err(_), Err(_)) => true,
      _ => false,
    };
    for (index, (input, unordered)) in self.saved_inputs.iter_mut().enumerate()
    {
      let next = other.saved_inputs.get(index);
      if next.map(|(input, _)| input) != Some(input)
        || (!same_stream
          && input.as_ref().is_ok_and(|(saved, _)| {
            [self.input.as_ref(), other.input.as_ref()]
              .into_iter()
              .flatten()
              .any(|(active, _)| Arc::ptr_eq(active, saved))
          }))
      {
        *input =
          Err(next.map_or(ValueIssue::Dynamic, |(other, _)| {
            input_issue(input, other)
          }));
      }
      *unordered |= next.is_some_and(|(_, unordered)| *unordered);
    }
    if self.input != other.input {
      self.input = Err(input_issue(&self.input, &other.input));
    }
    self.input_order_unknown |= other.input_order_unknown;
  }

  pub fn forget_inputs(&mut self, issue: ValueIssue) {
    self.input = Err(issue);
    for (input, _) in &mut self.saved_inputs {
      *input = Err(issue);
    }
  }

  pub fn save_input(&mut self) -> usize {
    let slot = self.saved_inputs.len();
    self
      .saved_inputs
      .push_back((self.input.clone(), self.input_order_unknown));
    slot
  }

  pub fn restore_input(&mut self, slot: usize) {
    (self.input, self.input_order_unknown) = self.saved_inputs[slot].clone();
    self.saved_inputs.truncate(slot);
  }

  pub fn inherit_input(&mut self, other: &Self, slot: usize) {
    self.saved_inputs.clone_from(&other.saved_inputs);
    self.restore_input(slot);
  }

  pub fn consume_input(&mut self, offset: Option<usize>) {
    if self.input_order_unknown {
      self.forget_inputs(ValueIssue::Unspecified);
      return;
    }
    let Ok((stream, _)) = &self.input else {
      return;
    };
    let next = offset
      .map(|offset| (Arc::clone(stream), offset))
      .ok_or(ValueIssue::Dynamic);
    for (saved, _) in &mut self.saved_inputs {
      if saved
        .as_ref()
        .is_ok_and(|(old, _)| Arc::ptr_eq(old, stream))
      {
        saved.clone_from(&next);
      }
    }
    self.input = next;
  }

  pub fn connect_input(&mut self, input: Input) {
    self.input = input;
    self.input_order_unknown = false;
  }
}
