use super::environment::{Binding, Environment, Positionals};
use super::values::{ALWAYS, Guard, NEVER, ValueId, Values};
use crate::interface::application::engine::{Error, Resolution};
use std::cell::{RefCell, RefMut};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::interface::application::engine::analysis) enum StateKey {
  Variable(Vec<u8>),
  Function(Vec<u8>),
  Positionals,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum StateValue {
  Binding(Binding),
  Positionals(Positionals),
}

fn resolution_bytes(resolution: &Resolution) -> usize {
  (resolution.candidates.len()
    + resolution.alternatives.len()
    + resolution.issues.len()
    + resolution.origins.len())
    * 32
}

impl StateValue {
  fn storage(&self) -> usize {
    match self {
      Self::Binding(binding) => {
        binding.value.known_size() + resolution_bytes(&binding.resolution)
      }
      Self::Positionals(value) => value
        .fields
        .iter()
        .map(|(value, _)| value.known_size() + 64)
        .sum(),
    }
  }
}

impl StateKey {
  fn read(&self, env: &Environment) -> StateValue {
    match self {
      Self::Variable(name) => {
        StateValue::Binding(env.variables.get(name).clone())
      }
      Self::Function(name) => {
        StateValue::Binding(env.functions.get(name).clone())
      }
      Self::Positionals => StateValue::Positionals(env.positional.clone()),
    }
  }

  fn restore(&self, env: &mut Environment, value: &StateValue) {
    match (self, value) {
      (Self::Variable(name), StateValue::Binding(value)) => {
        env.variables.values.insert(name.clone(), value.clone());
      }
      (Self::Function(name), StateValue::Binding(value)) => {
        env.functions.values.insert(name.clone(), value.clone());
      }
      (Self::Positionals, StateValue::Positionals(value)) => {
        env.positional.clone_from(value);
      }
      _ => unreachable!("state decision kind"),
    }
  }
}

type DecisionId = usize;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Decision {
  Value(StateValue),
  Computed {
    value: ValueId,
    resolution: Resolution,
  },
  Positionals(
    Vec<(
      ValueId,
      crate::interface::application::engine::word::FieldCount,
    )>,
  ),
  Branch {
    guard: Guard,
    left: DecisionId,
    right: DecisionId,
  },
}

type Outcomes = BTreeMap<StateValue, Guard>;

#[derive(Debug, Default)]
struct Decisions {
  nodes: Vec<Decision>,
  index: BTreeMap<Decision, DecisionId>,
  next: usize,
  expressions: Values,
  scalars: HashMap<(DecisionId, Option<usize>), ValueId>,
  outcomes: HashMap<DecisionId, Outcomes>,
}

impl Decisions {
  fn intern(&mut self, decision: Decision) -> DecisionId {
    if let Some(id) = self.index.get(&decision) {
      return *id;
    }
    let bytes = match &decision {
      Decision::Value(value) => value.storage(),
      Decision::Computed { resolution, .. } => resolution_bytes(resolution),
      Decision::Positionals(fields) => {
        fields.len()
          * std::mem::size_of::<(
            ValueId,
            crate::interface::application::engine::word::FieldCount,
          )>()
      }
      Decision::Branch { .. } => 0,
    };
    self.expressions.budget().work(bytes.div_ceil(16) + 1);
    self
      .expressions
      .budget()
      .retain(std::mem::size_of::<Decision>() * 2 + bytes + 64);
    let id = self.nodes.len();
    self.nodes.push(decision.clone());
    self.index.insert(decision, id);
    id
  }

  fn branch(
    &mut self,
    guard: Guard,
    left: DecisionId,
    right: DecisionId,
  ) -> DecisionId {
    if left == right || guard == NEVER {
      left
    } else if guard == ALWAYS {
      right
    } else {
      self.intern(Decision::Branch { guard, left, right })
    }
  }

  fn value(&mut self, env: &Environment, name: &StateKey) -> DecisionId {
    env
      .alternatives
      .values
      .get(name)
      .copied()
      .unwrap_or_else(|| self.intern(Decision::Value(name.read(env))))
  }

  fn computed(&mut self, value: ValueId, resolution: Resolution) -> DecisionId {
    if let Some(value) = self.expressions.constant(value) {
      self.intern(Decision::Value(StateValue::Binding(Binding {
        resolution,
        value,
      })))
    } else {
      self.intern(Decision::Computed { value, resolution })
    }
  }

  fn scalar(
    &mut self,
    root: DecisionId,
    index: Option<usize>,
    cancelled: &AtomicBool,
  ) -> Result<ValueId, Error> {
    let mut pending = vec![(root, false)];
    while let Some((id, visited)) = pending.pop() {
      self.expressions.budget().work(1);
      self.expressions.budget().check(cancelled)?;
      if self.scalars.contains_key(&(id, index)) {
        continue;
      }
      let result = match &self.nodes[id] {
        Decision::Value(StateValue::Binding(binding)) => {
          self.expressions.literal(binding.value.clone())
        }
        Decision::Value(StateValue::Positionals(value)) => {
          self.expressions.literal(std::sync::Arc::new(
            value.parameter(index.expect("positional index")).0,
          ))
        }
        Decision::Positionals(fields) => {
          fields.get(index.expect("positional index")).map_or_else(
            || {
              self.expressions.literal(std::sync::Arc::new(
                crate::interface::application::engine::Value::Literal(
                  Vec::new(),
                ),
              ))
            },
            |(id, _)| *id,
          )
        }
        Decision::Computed { value, .. } => *value,
        Decision::Branch { guard, left, right } => {
          if !visited {
            pending.extend([(id, true), (*right, false), (*left, false)]);
            continue;
          }
          self.expressions.select(
            *guard,
            self.scalars[&(*right, index)],
            self.scalars[&(*left, index)],
          )
        }
      };
      self.expressions.budget().retain(32);
      self.scalars.insert((id, index), result);
    }
    Ok(self.scalars[&(root, index)])
  }

  fn merge(&mut self, output: &mut Outcomes, value: StateValue, guard: Guard) {
    if guard == NEVER {
      return;
    }
    if let Some(previous) = output.get_mut(&value) {
      *previous = self.expressions.conditions.or(*previous, guard);
    } else {
      self
        .expressions
        .budget()
        .retain(std::mem::size_of::<StateValue>() + value.storage() + 64);
      output.insert(value, guard);
    }
  }

  fn possible(
    &mut self,
    root: DecisionId,
    cancelled: &AtomicBool,
  ) -> Result<Outcomes, Error> {
    let mut pending = vec![(root, false)];
    while let Some((id, visited)) = pending.pop() {
      self.expressions.budget().work(1);
      self.expressions.budget().check(cancelled)?;
      if self.outcomes.contains_key(&id) {
        continue;
      }
      let mut result = Outcomes::new();
      match self.nodes[id].clone() {
        Decision::Value(value) => self.merge(&mut result, value, ALWAYS),
        Decision::Computed { value, resolution } => {
          for (value, guard) in self.expressions.candidates(value, cancelled)? {
            let value = self.expressions.projected(value, cancelled)?;
            self.merge(
              &mut result,
              StateValue::Binding(Binding {
                value,
                resolution: resolution.clone(),
              }),
              guard,
            );
          }
        }
        Decision::Positionals(fields) => {
          let ids = fields.iter().map(|(id, _)| *id).collect::<Vec<_>>();
          for (values, guard) in
            self.expressions.combinations(&ids, ALWAYS, cancelled)?
          {
            let fields = values
              .into_iter()
              .zip(&fields)
              .map(|(value, (_, count))| (value.as_ref().clone(), *count))
              .collect();
            self.merge(
              &mut result,
              StateValue::Positionals(Positionals { fields }),
              guard,
            );
          }
        }
        Decision::Branch { guard, left, right } => {
          if !visited {
            pending.extend([(id, true), (right, false), (left, false)]);
            continue;
          }
          let opposite = self.expressions.conditions.not(guard);
          for (child, side) in [(left, opposite), (right, guard)] {
            for (value, guard) in self.outcomes[&child].clone() {
              self.expressions.budget().work(1);
              self.expressions.budget().check(cancelled)?;
              let guard = self.expressions.conditions.and(side, guard);
              self.merge(&mut result, value, guard);
            }
          }
        }
      }
      self.outcomes.insert(id, result);
    }
    Ok(self.outcomes[&root].clone())
  }

  fn summary(
    &mut self,
    root: DecisionId,
    active: Guard,
    cancelled: &AtomicBool,
  ) -> Result<StateValue, Error> {
    if matches!(
      self.nodes[root],
      Decision::Value(StateValue::Positionals(_))
    ) {
      return Ok(match &self.nodes[root] {
        Decision::Value(value) => value.clone(),
        _ => unreachable!(),
      });
    }
    let mut pending = vec![(root, active)];
    let mut seen = BTreeSet::new();
    let mut resolution = Resolution::default();
    let mut positional: Option<Positionals> = None;
    while let Some((id, guard)) = pending.pop() {
      self.expressions.budget().work(1);
      self.expressions.budget().check(cancelled)?;
      if guard == NEVER || !seen.insert((id, guard)) {
        continue;
      }
      match &self.nodes[id] {
        Decision::Value(StateValue::Binding(binding)) => {
          resolution.join(&binding.resolution);
        }
        Decision::Computed {
          resolution: value, ..
        } => resolution.join(value),
        Decision::Positionals(fields) => {
          let fields = fields.clone();
          let mut projected = Vec::new();
          for (id, count) in fields {
            let id = self.expressions.mask(id, guard);
            projected.push((
              self.expressions.projected(id, cancelled)?.as_ref().clone(),
              count,
            ));
          }
          let value = Positionals { fields: projected };
          if let Some(previous) = &mut positional {
            previous.join(&value);
          } else {
            positional = Some(value);
          }
        }
        Decision::Value(StateValue::Positionals(value)) => {
          if let Some(previous) = &mut positional {
            previous.join(value);
          } else {
            positional = Some(value.clone());
          }
        }
        Decision::Branch {
          guard: condition,
          left,
          right,
        } => {
          let (condition, left, right) = (*condition, *left, *right);
          let opposite = self.expressions.conditions.not(condition);
          let low = self.expressions.conditions.and(guard, opposite);
          let high = self.expressions.conditions.and(guard, condition);
          pending.extend([(left, low), (right, high)]);
        }
      }
    }
    if let Some(value) = positional {
      return Ok(StateValue::Positionals(value));
    }
    let scalar = self.scalar(root, None, cancelled)?;
    let scalar = self.expressions.mask(scalar, active);
    let value = self.expressions.projected(scalar, cancelled)?;
    self.expressions.budget().check(cancelled)?;
    Ok(StateValue::Binding(Binding { resolution, value }))
  }
}

#[derive(Clone, Debug)]
pub(in crate::interface::application::engine::analysis) struct Alternatives {
  values: imbl::OrdMap<StateKey, DecisionId>,
  decisions: Rc<RefCell<Decisions>>,
  active: Guard,
}

impl Default for Alternatives {
  fn default() -> Self {
    Self {
      values: imbl::OrdMap::new(),
      decisions: Rc::default(),
      active: ALWAYS,
    }
  }
}

impl PartialEq for Alternatives {
  fn eq(&self, other: &Self) -> bool {
    (self.values.is_empty() || Rc::ptr_eq(&self.decisions, &other.decisions))
      && self.values == other.values
      && self.active == other.active
  }
}
impl Eq for Alternatives {}

impl Alternatives {
  pub fn set_budget(&mut self, budget: super::super::request::Budget) {
    self.decisions.borrow_mut().expressions.set_budget(budget);
  }
  pub fn expressions(&self) -> RefMut<'_, Values> {
    RefMut::map(self.decisions.borrow_mut(), |nodes| &mut nodes.expressions)
  }
  pub fn scalar(
    &self,
    env: &Environment,
    name: &[u8],
    cancelled: &AtomicBool,
  ) -> Result<ValueId, Error> {
    let mut nodes = self.decisions.borrow_mut();
    let root = nodes.value(env, &StateKey::Variable(name.to_vec()));
    let value = nodes.scalar(root, None, cancelled)?;
    Ok(nodes.expressions.mask(value, self.active))
  }
  pub fn store(&mut self, name: &[u8], value: ValueId, resolution: Resolution) {
    let mut nodes = self.decisions.borrow_mut();
    let value = nodes.computed(value, resolution);
    let key = StateKey::Variable(name.to_vec());
    if matches!(nodes.nodes[value], Decision::Value(_)) {
      self.values.remove(&key);
    } else {
      self.values.insert(key, value);
    }
  }
  pub fn positional(
    &self,
    env: &Environment,
    index: usize,
    cancelled: &AtomicBool,
  ) -> Result<Option<ValueId>, Error> {
    if env
      .positional
      .fields
      .iter()
      .take(index + 1)
      .any(|(_, count)| count.boundary().is_err())
    {
      return Ok(None);
    }
    let mut nodes = self.decisions.borrow_mut();
    let root = nodes.value(env, &StateKey::Positionals);
    let value = nodes.scalar(root, Some(index), cancelled)?;
    Ok(Some(nodes.expressions.mask(value, self.active)))
  }

  pub fn store_positionals(
    &mut self,
    fields: Vec<(
      ValueId,
      crate::interface::application::engine::word::FieldCount,
    )>,
  ) {
    if fields
      .iter()
      .all(|(id, _)| self.expressions().constant(*id).is_some())
    {
      self.forget_positionals();
      return;
    }
    let id = self
      .decisions
      .borrow_mut()
      .intern(Decision::Positionals(fields));
    self.values.insert(StateKey::Positionals, id);
  }

  pub fn output(&self, env: &Environment) -> ValueId {
    let output = env.io.output.as_ref().expect("captured output");
    let mut values = self.expressions();
    let value = output
      .expression
      .unwrap_or_else(|| values.literal(std::sync::Arc::new(output.value())));
    values.mask(value, self.active)
  }

  pub fn is_empty(&self) -> bool {
    self.values.is_empty()
  }

  pub fn forget_names(
    &mut self,
    functions: bool,
    changed: impl Fn(&[u8]) -> bool,
  ) {
    let keys: Vec<_> = self
      .values
      .keys()
      .filter(|key| match key {
        StateKey::Variable(name) => !functions && changed(name),
        StateKey::Function(name) => functions && changed(name),
        StateKey::Positionals => false,
      })
      .cloned()
      .collect();
    for key in keys {
      self.values.remove(&key);
    }
  }

  pub fn forget_function(&mut self, name: &[u8]) {
    self.values.remove(&StateKey::Function(name.to_vec()));
  }

  pub fn forget_positionals(&mut self) {
    self.values.remove(&StateKey::Positionals);
  }

  pub fn restore_positionals(&mut self, before: &Self) {
    self.restore_key(StateKey::Positionals, before);
  }

  pub fn forget(&mut self, name: &[u8]) {
    self.values.remove(&StateKey::Variable(name.to_vec()));
  }

  pub fn restore(&mut self, name: &[u8], before: &Self) {
    self.restore_key(StateKey::Variable(name.to_vec()), before);
  }

  fn restore_key(&mut self, key: StateKey, before: &Self) {
    if let Some(value) = before.values.get(&key) {
      self.values.insert(key, *value);
    } else {
      self.values.remove(&key);
    }
  }

  pub fn retain_common(&mut self, other: &Self) {
    if self == other {
      return;
    }
    if !Rc::ptr_eq(&self.decisions, &other.decisions) {
      self.values.clear();
      self.active = ALWAYS;
      return;
    }
    self.active = self
      .decisions
      .borrow_mut()
      .expressions
      .conditions
      .or(self.active, other.active);
    let removed: Vec<_> = self
      .values
      .iter()
      .filter(|(key, value)| other.values.get(*key) != Some(*value))
      .map(|(key, _)| key.clone())
      .collect();
    for key in removed {
      self.values.remove(&key);
    }
  }

  pub fn joined(
    left: &Environment,
    right: &Environment,
  ) -> (Self, Option<ValueId>) {
    if left.alternatives == right.alternatives
      && left.variables == right.variables
      && left.functions == right.functions
      && left.positional == right.positional
      && left.io.output == right.io.output
    {
      return (
        left.alternatives.clone(),
        left.io.output.as_ref().and_then(|output| output.expression),
      );
    }
    let names: BTreeSet<_> = left
      .variables
      .differences(&right.variables)
      .map(|(name, _, _)| StateKey::Variable(name.clone()))
      .chain(
        left
          .functions
          .differences(&right.functions)
          .map(|(name, _, _)| StateKey::Function(name.clone())),
      )
      .chain(std::iter::once(StateKey::Positionals))
      .chain(left.alternatives.values.keys().cloned())
      .chain(right.alternatives.values.keys().cloned())
      .collect();
    let decisions = left.alternatives.decisions.clone();
    let mut nodes = decisions.borrow_mut();
    let (a, b) = (left.alternatives.active, right.alternatives.active);
    let overlap = nodes.expressions.conditions.and(a, b);
    let guard = if overlap == NEVER {
      b
    } else {
      let choice = nodes.next;
      nodes.next += 1;
      let choice = nodes.expressions.conditions.variable(choice);
      let outside = nodes.expressions.conditions.not(a);
      let selected = nodes.expressions.conditions.or(outside, choice);
      nodes.expressions.conditions.and(b, selected)
    };
    let active = nodes.expressions.conditions.or(a, b);
    let mut values = imbl::OrdMap::new();
    for name in names {
      nodes.expressions.budget().work(1);
      if nodes.expressions.budget().exhausted() {
        break;
      }
      let a = nodes.value(left, &name);
      let b = nodes.value(right, &name);
      let value = nodes.branch(guard, a, b);
      if !matches!(nodes.nodes[value], Decision::Value(_)) {
        values.insert(name, value);
      }
    }
    let output =
      if let (Some(left), Some(right)) = (&left.io.output, &right.io.output) {
        if left.expression.is_some() || right.expression.is_some() {
          let a = left.expression.unwrap_or_else(|| {
            nodes.expressions.literal(std::sync::Arc::new(left.value()))
          });
          let b = right.expression.unwrap_or_else(|| {
            nodes
              .expressions
              .literal(std::sync::Arc::new(right.value()))
          });
          Some(nodes.expressions.select(guard, b, a))
        } else {
          None
        }
      } else {
        None
      };
    drop(nodes);
    (
      Self {
        values,
        decisions,
        active,
      },
      output,
    )
  }

  pub fn depends(&self, names: &BTreeSet<StateKey>) -> bool {
    names.iter().any(|name| self.values.contains_key(name))
  }

  pub fn partitions(
    env: &Environment,
    names: &BTreeSet<StateKey>,
    cancelled: &AtomicBool,
  ) -> Result<Vec<Environment>, Error> {
    let decisions = env.alternatives.decisions.clone();
    let mut nodes = decisions.borrow_mut();
    let names: Vec<_> = names
      .iter()
      .filter_map(|name| {
        env
          .alternatives
          .values
          .get(name)
          .map(|id| (name.clone(), *id))
      })
      .collect();
    let mut inputs = BTreeMap::from([(Vec::new(), env.alternatives.active)]);
    for (_, id) in &names {
      let possible = nodes.possible(*id, cancelled)?;
      let mut next = BTreeMap::new();
      for (input, active) in inputs {
        for (value, condition) in &possible {
          nodes.expressions.budget().work(1);
          nodes.expressions.budget().check(cancelled)?;
          let guard = nodes.expressions.conditions.and(active, *condition);
          if guard == NEVER {
            continue;
          }
          let mut key = input.clone();
          key.push(value.clone());
          nodes
            .expressions
            .budget()
            .retain(key.len() * std::mem::size_of::<StateValue>() + 64);
          next.insert(key, guard);
        }
      }
      inputs = next;
    }
    let mut result = Vec::new();
    for (input, active) in inputs {
      nodes.expressions.budget().work(1);
      nodes.expressions.budget().check(cancelled)?;
      let mut output = env.clone();
      output.alternatives.active = active;
      for ((name, _), value) in names.iter().zip(input) {
        name.restore(&mut output, &value);
        output.alternatives.values.remove(name);
      }
      for (name, id) in output.alternatives.values.clone() {
        let value = nodes.summary(id, active, cancelled)?;
        name.restore(&mut output, &value);
      }
      if let Some(captured) = &mut output.io.output
        && let Some(id) = captured.expression
      {
        let id = nodes.expressions.mask(id, active);
        let value = nodes.expressions.projected(id, cancelled)?;
        let trace = captured.trace();
        *captured = super::environment::Output::default();
        captured.append(value.as_ref().clone(), trace, usize::MAX);
        captured.expression = Some(id);
      }
      result.push(output);
    }
    Ok(result)
  }
}

#[cfg(test)]
#[path = "../../../../../../tests/implementation/alternatives.rs"]
mod tests;
