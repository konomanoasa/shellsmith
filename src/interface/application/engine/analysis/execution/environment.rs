use super::alternatives::Alternatives;
use imbl::{OrdMap, ordmap::DiffItem};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::interface::application::engine::{
  Alternative, InitialBinding, InitialEnvironment, Resolution, ResolutionIssue,
  SymbolId, Value, ValueIssue,
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

  pub fn taint(&mut self, issue: ResolutionIssue) {
    self
      .resolution
      .alternatives
      .extend([Alternative::External, Alternative::Unset]);
    self.resolution.issues.insert(issue);
    self.value = Arc::new(Value::Unknown(ValueIssue::Dynamic));
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

  pub fn taint(&mut self, issue: ResolutionIssue) {
    self.patterns.clear();
    self.uncertain = None;
    for (name, mut value) in self.values.clone() {
      value.taint(issue);
      self.values.insert(name, value);
    }
    self.fallback.taint(issue);
  }

  pub fn restrict(&mut self, pattern: Value, issue: ResolutionIssue) {
    let mut fallback = self
      .uncertain
      .take()
      .unwrap_or_else(|| self.fallback.clone());
    fallback.taint(issue);
    self.uncertain = Some(fallback);
    self.patterns.insert(pattern);
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::interface::application::engine::analysis) struct Environment {
  pub alternatives: Alternatives,
  pub variables: Names,
  pub functions: Names,
  pub order: BTreeSet<crate::interface::application::engine::EffectTarget>,
  pub readonly: OrdMap<Vec<u8>, bool>,
  pub unknown_attributes: bool,
  pub errexit: Option<bool>,
  pub nounset: Option<bool>,
  pub noglob: Option<bool>,
  pub cwd: Value,
  pub positional: Option<Vec<Value>>,
  pub special: BTreeMap<Vec<u8>, Value>,
  pub substitution_status: Option<Value>,
  pub option_cursor: Option<(Vec<Value>, usize)>,
  pub options_reset: bool,
  pub io: Io,
  pub directory_variables_modified: bool,
}

impl Environment {
  pub(in crate::interface::application::engine::analysis) fn invalidate_pattern(
    &mut self,
    pattern: Value,
    functions: bool,
    issue: ResolutionIssue,
  ) {
    if !functions && pattern.can_equal(b"OPTIND") {
      self.variable_changed(b"OPTIND", None);
    }
    let names = if functions {
      &mut self.functions
    } else {
      &mut self.variables
    };
    for (name, mut binding) in names.values.clone() {
      if pattern.can_equal(&name)
        && (functions || self.readonly.get(&name) != Some(&true))
      {
        binding.taint(issue);
        names.values.insert(name.clone(), binding);
        if functions {
          self.alternatives.forget_function(&name);
        } else {
          self.alternatives.forget(&name);
        }
      }
    }
    names.restrict(pattern, issue);
  }

  pub fn variable_changed(&mut self, name: &[u8], value: Option<&Value>) {
    if name == b"OPTIND" {
      self.option_cursor = None;
      self.options_reset = value == Some(&Value::Literal(b"1".to_vec()));
    }
  }

  pub fn locale(&self, category: &[u8]) -> Option<&[u8]> {
    for name in [b"LC_ALL".as_slice(), category, b"LANG"] {
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
      positional: input
        .positional
        .as_ref()
        .map(|args| args.iter().cloned().map(Value::Literal).collect()),
      special: input
        .special
        .iter()
        .map(|(k, v)| (k.clone(), Value::Literal(v.clone())))
        .collect(),
      directory_variables_modified: false,
      substitution_status: None,
      option_cursor: None,
      options_reset: input
        .values
        .get(b"OPTIND".as_slice())
        .or_else(|| input.environment.get(b"OPTIND".as_slice()))
        .is_some_and(|value| value == b"1"),
      io: Io::default(),
      order: BTreeSet::new(),
      readonly: OrdMap::new(),
      unknown_attributes: false,
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
    self.alternatives.clear();
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
    match (&mut self.positional, &other.positional) {
      (Some(left), Some(right)) if left.len() == right.len() => {
        for (a, b) in left.iter_mut().zip(right) {
          a.join(b);
        }
      }
      _ => self.positional = None,
    }
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
    self.unknown_attributes |= other.unknown_attributes;
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

  pub fn readonly(&self, name: &[u8]) -> Option<bool> {
    self
      .readonly
      .get(name)
      .copied()
      .or_else(|| self.unknown_attributes.then_some(false))
  }

  pub fn readonly_attribute(
    &self,
    name: &[u8],
  ) -> crate::interface::application::engine::AttributeState {
    use crate::interface::application::engine::AttributeState;
    match self.readonly.get(name) {
      Some(true) => AttributeState::Known(true),
      Some(false) => AttributeState::Conditional,
      None if self.unknown_attributes => AttributeState::Unknown,
      None if self.variables.get(name).resolution.issues.is_empty() => {
        AttributeState::Known(false)
      }
      None => AttributeState::Unknown,
    }
  }

  pub fn join_branches(&mut self, other: &Self) {
    let alternatives = Alternatives::joined(self, other);
    self.join(other);
    self.alternatives = alternatives;
  }

  pub fn uncertain_variable(&self, name: &[u8]) -> bool {
    self
      .order
      .contains(&crate::interface::application::engine::EffectTarget::Variables)
      || self.order.contains(
        &crate::interface::application::engine::EffectTarget::Variable(
          name.to_vec(),
        ),
      )
      || self.order.iter().any(|target| matches!(target, crate::interface::application::engine::EffectTarget::VariablePattern(pattern) if pattern.can_equal(name)))
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
  pub BTreeMap<Transfer, Environment>,
);

impl Flow {
  pub fn next(environment: Environment) -> Self {
    Self(BTreeMap::from([(Transfer::Next, environment)]))
  }

  pub fn add(&mut self, transfer: Transfer, environment: Environment) {
    self
      .0
      .entry(transfer)
      .and_modify(|old| old.join_branches(&environment))
      .or_insert(environment);
  }

  pub fn join(&mut self, other: Self) {
    for (transfer, environment) in other.0 {
      self.add(transfer, environment);
    }
  }
}

#[cfg(test)]
#[path = "../../../../../../tests/implementation/state.rs"]
mod tests;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::interface::application::engine::analysis) struct Io {
  pub input: Input,
  pub input_order_unknown: bool,
  saved_inputs: imbl::Vector<(Input, bool)>,
  pub output: Option<Output>,
  pub stdout_redirected: bool,
  pub stderr_captured: bool,
}

type Input = Option<(Arc<Vec<u8>>, usize)>;

impl Io {
  fn join_input(&mut self, other: &Self) {
    let same_stream = match (&self.input, &other.input) {
      (Some((left, _)), Some((right, _))) => Arc::ptr_eq(left, right),
      (None, None) => true,
      _ => false,
    };
    for (index, (input, unordered)) in self.saved_inputs.iter_mut().enumerate()
    {
      let next = other.saved_inputs.get(index);
      if next.map(|(input, _)| input) != Some(input)
        || (!same_stream
          && input.as_ref().is_some_and(|(saved, _)| {
            [self.input.as_ref(), other.input.as_ref()]
              .into_iter()
              .flatten()
              .any(|(active, _)| Arc::ptr_eq(active, saved))
          }))
      {
        *input = None;
      }
      *unordered |= next.is_some_and(|(_, unordered)| *unordered);
    }
    if self.input != other.input {
      self.input = None;
    }
    self.input_order_unknown |= other.input_order_unknown;
  }

  pub fn forget_inputs(&mut self) {
    self.input = None;
    for (input, _) in &mut self.saved_inputs {
      *input = None;
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
      self.forget_inputs();
      return;
    }
    let Some((stream, _)) = &self.input else {
      return;
    };
    let next = offset.map(|offset| (Arc::clone(stream), offset));
    for (saved, _) in &mut self.saved_inputs {
      if saved
        .as_ref()
        .is_some_and(|(old, _)| Arc::ptr_eq(old, stream))
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
