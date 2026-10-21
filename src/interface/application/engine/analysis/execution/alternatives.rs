use super::environment::{Binding, Environment, Positionals};
use super::values::{ValueId, Values};
use crate::interface::application::engine::{
  Error, Resolution, check_cancelled,
};
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
  Branch {
    choice: usize,
    left: DecisionId,
    right: DecisionId,
  },
}

#[derive(Debug, Default)]
struct Decisions {
  nodes: Vec<Decision>,
  index: BTreeMap<Decision, DecisionId>,
  next: usize,
  expressions: Values,
  scalars: HashMap<DecisionId, ValueId>,
}

impl Decisions {
  fn intern(&mut self, decision: Decision) -> DecisionId {
    if let Some(id) = self.index.get(&decision) {
      return *id;
    }
    let id = self.nodes.len();
    self.nodes.push(decision.clone());
    self.index.insert(decision, id);
    id
  }

  fn branch(
    &mut self,
    choice: usize,
    left: DecisionId,
    right: DecisionId,
  ) -> DecisionId {
    if left == right {
      left
    } else {
      self.intern(Decision::Branch {
        choice,
        left,
        right,
      })
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
    if self.expressions.choice(value).is_none() {
      self.intern(Decision::Value(StateValue::Binding(Binding {
        value: self.expressions.projected(value).clone(),
        resolution,
      })))
    } else {
      self.intern(Decision::Computed { value, resolution })
    }
  }

  fn scalar(
    &mut self,
    root: DecisionId,
    cancelled: &AtomicBool,
  ) -> Result<ValueId, Error> {
    let mut pending = vec![(root, false)];
    while let Some((id, visited)) = pending.pop() {
      check_cancelled(cancelled)?;
      if self.scalars.contains_key(&id) {
        continue;
      }
      let result = match &self.nodes[id] {
        Decision::Value(StateValue::Binding(binding)) => {
          self.expressions.literal(binding.value.clone())
        }
        Decision::Value(StateValue::Positionals(_)) => unreachable!(),
        Decision::Computed { value, .. } => *value,
        Decision::Branch {
          choice,
          left,
          right,
        } => {
          if !visited {
            pending.extend([(id, true), (*right, false), (*left, false)]);
            continue;
          }
          self.expressions.branch(
            *choice,
            self.scalars[left],
            self.scalars[right],
          )
        }
      };
      self.scalars.insert(id, result);
    }
    Ok(self.scalars[&root])
  }

  fn choice(&self, id: DecisionId) -> Option<usize> {
    match self.nodes[id] {
      Decision::Branch { choice, .. } => Some(choice),
      Decision::Computed { value, .. } => self.expressions.choice(value),
      Decision::Value(_) => None,
    }
  }

  // Preserve correlations even for unread variables.
  // Indexed nodes avoid recursive traversal and destruction of deep choices.
  fn restrict(
    &mut self,
    value: DecisionId,
    choice: usize,
    right_side: bool,
    cache: &mut HashMap<DecisionId, DecisionId>,
  ) -> DecisionId {
    let mut pending = vec![(value, false)];
    while let Some((id, visited)) = pending.pop() {
      if cache.contains_key(&id) {
        continue;
      }
      let result = match self.nodes[id] {
        Decision::Branch {
          choice: current,
          left,
          right,
        } if current == choice => {
          if right_side {
            right
          } else {
            left
          }
        }
        Decision::Branch {
          choice: current,
          left,
          right,
        } if current > choice => {
          if !visited {
            pending.push((id, true));
            pending.push((right, false));
            pending.push((left, false));
            continue;
          }
          self.branch(current, cache[&left], cache[&right])
        }
        Decision::Computed {
          value,
          ref resolution,
        } => {
          let resolution = resolution.clone();
          let value = self.expressions.restrict(value, choice, right_side);
          self.computed(value, resolution)
        }
        _ => id,
      };
      cache.insert(id, result);
    }
    cache[&value]
  }
}

#[derive(Clone, Debug, Default)]
pub(in crate::interface::application::engine::analysis) struct Alternatives {
  values: imbl::OrdMap<StateKey, DecisionId>,
  decisions: Rc<RefCell<Decisions>>,
}

impl PartialEq for Alternatives {
  fn eq(&self, other: &Self) -> bool {
    (self.values.is_empty() || Rc::ptr_eq(&self.decisions, &other.decisions))
      && self.values == other.values
  }
}

impl Eq for Alternatives {}

impl Alternatives {
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
    let value = nodes.value(env, &StateKey::Variable(name.to_vec()));
    nodes.scalar(value, cancelled)
  }

  pub fn store(&mut self, name: &[u8], value: ValueId, resolution: Resolution) {
    let mut nodes = self.decisions.borrow_mut();
    let value = nodes.computed(value, resolution);
    if !matches!(nodes.nodes[value], Decision::Value(_)) {
      self.values.insert(StateKey::Variable(name.to_vec()), value);
    }
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
    self.forget_positionals();
    if let Some(value) = before.values.get(&StateKey::Positionals) {
      self.values.insert(StateKey::Positionals, *value);
    }
  }

  pub fn forget(&mut self, name: &[u8]) {
    self.values.remove(&StateKey::Variable(name.to_vec()));
  }

  pub fn restore(&mut self, name: &[u8], before: &Self) {
    if let Some(value) = before.values.get(&StateKey::Variable(name.to_vec())) {
      self
        .values
        .insert(StateKey::Variable(name.to_vec()), *value);
    } else {
      self.forget(name);
    }
  }

  pub fn clear(&mut self) {
    self.values.clear();
  }

  pub fn retain_common(&mut self, other: &Self) {
    if self == other {
      return;
    }
    if !Rc::ptr_eq(&self.decisions, &other.decisions) || other.is_empty() {
      self.clear();
      return;
    }
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

  pub fn joined(left: &Environment, right: &Environment) -> Self {
    if left.alternatives == right.alternatives
      && left.variables == right.variables
      && left.functions == right.functions
      && left.positional == right.positional
    {
      return left.alternatives.clone();
    }
    // Other names have equal bindings and no choices on both sides.
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
    let choice = nodes.next;
    nodes.next += 1;
    let mut values = imbl::OrdMap::new();
    for name in names {
      let a = nodes.value(left, &name);
      let b = nodes.value(right, &name);
      let value = nodes.branch(choice, a, b);
      if !matches!(nodes.nodes[value], Decision::Value(_)) {
        values.insert(name.clone(), value);
      }
    }
    drop(nodes);
    Self { values, decisions }
  }

  pub fn select(
    &self,
    env: &Environment,
    names: &BTreeSet<StateKey>,
  ) -> Option<AlternativeStates> {
    names
      .iter()
      .any(|name| self.values.contains_key(name))
      .then(|| AlternativeStates {
        pending: vec![env.clone()],
        names: names.clone(),
      })
  }
}

pub(in crate::interface::application::engine::analysis) struct AlternativeStates
{
  pending: Vec<Environment>,
  names: BTreeSet<StateKey>,
}

impl AlternativeStates {
  fn restricted(
    mut env: Environment,
    choice: usize,
    right: bool,
  ) -> Environment {
    let mut cache = HashMap::new();
    let previous = env.alternatives.values.clone();
    let decisions = env.alternatives.decisions.clone();
    let mut nodes = decisions.borrow_mut();
    for (name, value) in &previous {
      let value = nodes.restrict(*value, choice, right, &mut cache);
      if let Decision::Value(value) = &nodes.nodes[value] {
        let value = value.clone();
        name.restore(&mut env, &value);
        env.alternatives.values.remove(name);
      } else {
        env.alternatives.values.insert(name.clone(), value);
      }
    }
    drop(nodes);
    env
  }
}

impl Iterator for AlternativeStates {
  type Item = Environment;
  fn next(&mut self) -> Option<Self::Item> {
    let mut env = self.pending.pop()?;
    loop {
      let choice = {
        let nodes = env.alternatives.decisions.borrow();
        self
          .names
          .iter()
          .filter_map(|name| nodes.choice(*env.alternatives.values.get(name)?))
          .max()
      };
      let Some(choice) = choice else {
        return Some(env);
      };
      self
        .pending
        .push(Self::restricted(env.clone(), choice, true));
      env = Self::restricted(env, choice, false);
    }
  }
}

#[cfg(test)]
#[path = "../../../../../../tests/implementation/alternatives.rs"]
mod tests;
