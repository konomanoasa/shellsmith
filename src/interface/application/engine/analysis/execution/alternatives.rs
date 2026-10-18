use super::environment::{Binding, Environment};
use crate::interface::application::engine::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::interface::application::engine::analysis) enum StateKey {
  Variable(Vec<u8>),
  Function(Vec<u8>),
  Positionals,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum StateValue {
  Binding(Binding),
  Positionals(Option<Vec<Value>>),
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
  pub fn is_empty(&self) -> bool {
    self.values.is_empty()
  }

  pub fn forget_functions(&mut self) {
    let keys: Vec<_> = self
      .values
      .keys()
      .filter(|key| matches!(key, StateKey::Function(_)))
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

  pub fn joined(left: &Environment, right: &Environment) -> Self {
    if left.alternatives == right.alternatives
      && left.variables == right.variables
      && left.functions == right.functions
      && left.positional == right.positional
    {
      return left.alternatives.clone();
    }
    let names: BTreeSet<_> = left
      .variables
      .values
      .keys()
      .chain(right.variables.values.keys())
      .cloned()
      .map(StateKey::Variable)
      .chain(
        left
          .functions
          .values
          .keys()
          .chain(right.functions.values.keys())
          .cloned()
          .map(StateKey::Function),
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
      if matches!(nodes.nodes[value], Decision::Branch { .. }) {
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
          .filter_map(|name| {
            match nodes.nodes[*env.alternatives.values.get(name)?] {
              Decision::Branch { choice, .. } => Some(choice),
              Decision::Value(_) => None,
            }
          })
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
