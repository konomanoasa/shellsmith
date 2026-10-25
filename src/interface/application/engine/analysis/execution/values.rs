use super::super::request::Budget;
use crate::interface::application::engine::{
  Error, PartialValue, Value, ValueConstraint, ValueIssue,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub(in crate::interface::application::engine::analysis) type ValueId = usize;
pub(in crate::interface::application::engine::analysis) type Guard = usize;
pub(in crate::interface::application::engine::analysis) const NEVER: Guard = 0;
pub(in crate::interface::application::engine::analysis) const ALWAYS: Guard = 1;
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Decision {
  choice: usize,
  low: Guard,
  high: Guard,
}

#[derive(Debug)]
pub(in crate::interface::application::engine::analysis) struct Conditions {
  nodes: Vec<Decision>,
  unique: HashMap<Decision, Guard>,
  cache: HashMap<(bool, Guard, Guard), Guard>,
  budget: Budget,
}

impl Default for Conditions {
  fn default() -> Self {
    Self {
      nodes: vec![
        Decision {
          choice: 0,
          low: 0,
          high: 0
        };
        2
      ],
      unique: HashMap::new(),
      cache: HashMap::new(),
      budget: Budget::default(),
    }
  }
}

impl Conditions {
  fn decision(&mut self, choice: usize, low: Guard, high: Guard) -> Guard {
    if low == high {
      return low;
    }
    let node = Decision { choice, low, high };
    if let Some(id) = self.unique.get(&node) {
      return *id;
    }
    self.budget.retain(std::mem::size_of::<Decision>() * 3);
    let id = self.nodes.len();
    self.nodes.push(node);
    self.unique.insert(node, id);
    id
  }

  pub(super) fn variable(&mut self, choice: usize) -> Guard {
    self.decision(choice, NEVER, ALWAYS)
  }

  pub(super) fn not(&mut self, root: Guard) -> Guard {
    let mut done = HashMap::from([(NEVER, ALWAYS), (ALWAYS, NEVER)]);
    let mut pending = vec![(root, false)];
    while let Some((id, visited)) = pending.pop() {
      if self.budget.exhausted() {
        return NEVER;
      }
      if done.contains_key(&id) {
        continue;
      }
      let node = self.nodes[id];
      if !visited {
        pending.extend([(id, true), (node.low, false), (node.high, false)]);
        continue;
      }
      self.budget.work(1);
      let result =
        self.decision(node.choice, done[&node.low], done[&node.high]);
      done.insert(id, result);
    }
    done[&root]
  }

  pub(in crate::interface::application::engine::analysis) fn and(
    &mut self,
    a: Guard,
    b: Guard,
  ) -> Guard {
    self.apply(false, a, b)
  }

  pub(in crate::interface::application::engine::analysis) fn or(
    &mut self,
    a: Guard,
    b: Guard,
  ) -> Guard {
    self.apply(true, a, b)
  }

  fn apply(&mut self, union: bool, a: Guard, b: Guard) -> Guard {
    let key = |a: Guard, b: Guard| (union, a.min(b), a.max(b));
    let root = key(a, b);
    let mut pending = vec![(root, false)];
    let mut children = HashMap::new();
    while let Some((current @ (_, a, b), visited)) = pending.pop() {
      if self.budget.exhausted() {
        return NEVER;
      }
      if self.cache.contains_key(&current) {
        continue;
      }
      self.budget.work(1);
      let simple = if a == b {
        Some(a)
      } else if union && (a == ALWAYS || b == ALWAYS) {
        Some(ALWAYS)
      } else if !union && (a == NEVER || b == NEVER) {
        Some(NEVER)
      } else if (union && a == NEVER) || (!union && a == ALWAYS) {
        Some(b)
      } else {
        None
      };
      if let Some(id) = simple {
        self.budget.retain(64);
        self.cache.insert(current, id);
        continue;
      }
      let top = [a, b]
        .into_iter()
        .filter(|id| *id > ALWAYS)
        .map(|id| self.nodes[id].choice)
        .max()
        .unwrap();
      if visited {
        let (low, high) = children[&current];
        let result = self.decision(top, self.cache[&low], self.cache[&high]);
        self.budget.retain(64);
        self.cache.insert(current, result);
        continue;
      }
      let split = |id: Guard| {
        let node = self.nodes[id];
        if id > ALWAYS && node.choice == top {
          (node.low, node.high)
        } else {
          (id, id)
        }
      };
      let (al, ah) = split(a);
      let (bl, bh) = split(b);
      let (low, high) = (key(al, bl), key(ah, bh));
      children.insert(current, (low, high));
      pending.extend([(current, true), (low, false), (high, false)]);
    }
    self.cache[&root]
  }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Expression {
  Literal(Arc<Value>),
  Choice(Guard, ValueId, ValueId),
  Concat(ValueId, ValueId),
  Absent,
}

#[derive(Debug)]
struct Entry {
  expression: Expression,
  projected: Option<Arc<Value>>,
  maximum: Option<usize>,
  ascii: bool,
}

#[derive(Clone, Copy, Debug)]
struct Output {
  guard: Guard,
  reset: Guard,
  value: ValueId,
}

type Results<S> = BTreeMap<S, Output>;
type Memo<S> = BTreeMap<(ValueId, S), Results<S>>;
type QueryState = (usize, Option<u8>);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Query {
  Length,
  Character(usize),
  Contains(u8),
}

impl Query {
  fn step(self, &(count, found): &QueryState, value: &Value) -> QueryState {
    let Value::Literal(bytes) = value else {
      unreachable!("finite byte query")
    };
    match self {
      Self::Length => (count.saturating_add(bytes.len()), None),
      Self::Contains(wanted) => {
        (usize::from(count != 0 || bytes.contains(&wanted)), None)
      }
      Self::Character(index) if count <= index => (
        count.saturating_add(bytes.len()).min(index + 1),
        bytes.get(index - count).copied().or(found),
      ),
      Self::Character(_) => (count, found),
    }
  }
}

enum Work<S> {
  Visit(ValueId, S),
  Choice(ValueId, S, Guard, ValueId, ValueId),
  Left(ValueId, S, ValueId, ValueId),
  Right(ValueId, S, ValueId, ValueId),
}

#[derive(Debug)]
pub(in crate::interface::application::engine::analysis) struct Values {
  entries: Vec<Entry>,
  index: BTreeMap<Expression, ValueId>,
  pub conditions: Conditions,
  queries: BTreeMap<Query, Memo<QueryState>>,
  reversed: BTreeMap<ValueId, ValueId>,
  budget: Budget,
}

impl Default for Values {
  fn default() -> Self {
    let budget = Budget::default();
    let mut result = Self {
      entries: Vec::new(),
      index: BTreeMap::new(),
      conditions: Conditions::default(),
      queries: BTreeMap::new(),
      reversed: BTreeMap::new(),
      budget: budget.clone(),
    };
    result.conditions.budget = budget;
    assert_eq!(result.literal(Arc::new(Value::Literal(Vec::new()))), 0);
    result.intern(Expression::Absent);
    result
  }
}

impl Values {
  pub fn set_budget(&mut self, budget: Budget) {
    self.budget = budget.clone();
    self.conditions.budget = budget;
  }

  pub(super) fn budget(&self) -> &Budget {
    &self.budget
  }

  fn intern(&mut self, expression: Expression) -> ValueId {
    if let Some(id) = self.index.get(&expression) {
      return *id;
    }
    let (projected, maximum, ascii, bytes) = match &expression {
      Expression::Literal(value) => {
        let maximum = match value.as_ref() {
          Value::Literal(bytes) if !bytes.contains(&0) => Some(bytes.len()),
          _ => None,
        };
        (
          Some(value.clone()),
          maximum,
          matches!(value.as_ref(), Value::Literal(bytes) if bytes.is_ascii()),
          value.known_size(),
        )
      }
      Expression::Choice(_, a, b) | Expression::Concat(a, b) => {
        let (left, right) = (&self.entries[*a], &self.entries[*b]);
        let maximum = left.maximum.zip(right.maximum).and_then(|(a, b)| {
          if matches!(expression, Expression::Concat(..)) {
            a.checked_add(b)
          } else {
            Some(a.max(b))
          }
        });
        (None, maximum, left.ascii && right.ascii, 0)
      }
      Expression::Absent => (None, Some(0), true, 0),
    };
    self
      .budget
      .retain(std::mem::size_of::<Entry>() + bytes + 64);
    let id = self.entries.len();
    self.index.insert(expression.clone(), id);
    self.entries.push(Entry {
      expression,
      projected,
      maximum,
      ascii,
    });
    id
  }

  pub fn literal(&mut self, value: Arc<Value>) -> ValueId {
    self.intern(Expression::Literal(value))
  }

  pub fn constant(&self, id: ValueId) -> Option<Arc<Value>> {
    match &self.entries[id].expression {
      Expression::Literal(value) => Some(value.clone()),
      _ => None,
    }
  }

  pub fn projected(
    &mut self,
    id: ValueId,
    cancelled: &AtomicBool,
  ) -> Result<Arc<Value>, Error> {
    self.budget.check(cancelled)?;
    if let Some(value) = &self.entries[id].projected {
      return Ok(value.clone());
    }
    let result = Arc::new(self.project(id, cancelled)?);
    self.budget.retain(result.known_size());
    self.entries[id].projected = Some(result.clone());
    self.budget.check(cancelled)?;
    Ok(result)
  }

  pub fn maximum(&self, id: ValueId) -> Option<usize> {
    self.entries[id].maximum
  }

  pub fn select(&mut self, guard: Guard, yes: ValueId, no: ValueId) -> ValueId {
    if guard == ALWAYS || yes == no {
      yes
    } else if guard == NEVER {
      no
    } else {
      self.intern(Expression::Choice(guard, yes, no))
    }
  }

  pub(super) fn mask(&mut self, id: ValueId, guard: Guard) -> ValueId {
    self.select(guard, id, 1)
  }

  fn concatenate(&mut self, a: ValueId, b: ValueId) -> ValueId {
    if let (Expression::Literal(left), Expression::Literal(right)) =
      (&self.entries[a].expression, &self.entries[b].expression)
    {
      let mut value = left.as_ref().clone();
      value.concat(right, usize::MAX);
      return self.literal(Arc::new(value));
    }
    if a == 1 || b == 1 {
      1
    } else if a == 0 {
      b
    } else if b == 0 {
      a
    } else {
      self.intern(Expression::Concat(a, b))
    }
  }

  pub fn concat(
    &mut self,
    a: ValueId,
    b: ValueId,
    cancelled: &AtomicBool,
  ) -> Result<Option<ValueId>, Error> {
    self.budget.check(cancelled)?;
    if self.maximum(a).is_none() || self.maximum(b).is_none() {
      return Ok(None);
    }
    Ok(Some(self.concatenate(a, b)))
  }

  fn merge<S: Ord>(
    &mut self,
    results: &mut Results<S>,
    state: S,
    output: Output,
  ) {
    if output.guard == NEVER {
      return;
    }
    let reset = self.conditions.and(output.guard, output.reset);
    if let Some(previous) = results.get_mut(&state) {
      previous.value = self.select(output.guard, output.value, previous.value);
      previous.guard = self.conditions.or(previous.guard, output.guard);
      previous.reset = self.conditions.or(previous.reset, reset);
    } else {
      self
        .budget
        .retain(std::mem::size_of::<S>() + std::mem::size_of::<Output>() + 48);
      results.insert(state, Output { reset, ..output });
    }
  }

  fn transfer<S: Clone + Ord>(
    &mut self,
    root: ValueId,
    initial: S,
    step: impl Fn(&mut Self, &S, &Value) -> (S, ValueId, bool),
    done: &mut Memo<S>,
    cancelled: &AtomicBool,
  ) -> Result<Results<S>, Error> {
    let mut pending = vec![Work::Visit(root, initial.clone())];
    while let Some(work) = pending.pop() {
      self.budget.work(1);
      self.budget.check(cancelled)?;
      match work {
        Work::Visit(id, state) => {
          if done.contains_key(&(id, state.clone())) {
            continue;
          }
          self.budget.retain(std::mem::size_of::<S>() + 64);
          match self.entries[id].expression.clone() {
            Expression::Absent => {
              done.insert((id, state), Results::new());
            }
            Expression::Literal(value) => {
              self.budget.work(value.known_size().div_ceil(16));
              let (next, value, reset) = step(self, &state, &value);
              done.insert(
                (id, state),
                BTreeMap::from([(
                  next,
                  Output {
                    guard: ALWAYS,
                    reset: if reset { ALWAYS } else { NEVER },
                    value,
                  },
                )]),
              );
            }
            Expression::Choice(g, a, b) => {
              pending.push(Work::Choice(id, state.clone(), g, a, b));
              pending.push(Work::Visit(a, state.clone()));
              pending.push(Work::Visit(b, state));
            }
            Expression::Concat(a, b) => {
              pending.push(Work::Left(id, state.clone(), a, b));
              pending.push(Work::Visit(a, state));
            }
          }
        }
        Work::Choice(id, state, guard, a, b) => {
          let not = self.conditions.not(guard);
          let mut results = Results::new();
          for (child, side) in [(a, guard), (b, not)] {
            for (next, output) in &done[&(child, state.clone())] {
              self.budget.work(1);
              self.budget.check(cancelled)?;
              let guard = self.conditions.and(side, output.guard);
              self.merge(
                &mut results,
                next.clone(),
                Output { guard, ..*output },
              );
            }
          }
          done.insert((id, state), results);
        }
        Work::Left(id, state, a, b) => {
          pending.push(Work::Right(id, state.clone(), a, b));
          for next in done[&(a, state)].keys() {
            pending.push(Work::Visit(b, next.clone()));
          }
        }
        Work::Right(id, state, a, b) => {
          let mut results = Results::new();
          for (middle, first) in &done[&(a, state.clone())] {
            for (next, second) in &done[&(b, middle.clone())] {
              self.budget.work(1);
              self.budget.check(cancelled)?;
              let guard = self.conditions.and(first.guard, second.guard);
              if guard == NEVER {
                continue;
              }
              let prefix = self.select(second.reset, 0, first.value);
              let value = self.concatenate(prefix, second.value);
              let reset = self.conditions.or(first.reset, second.reset);
              self.merge(
                &mut results,
                next.clone(),
                Output {
                  guard,
                  reset,
                  value,
                },
              );
            }
          }
          done.insert((id, state), results);
        }
      }
    }
    self.budget.check(cancelled)?;
    Ok(done[&(root, initial)].clone())
  }

  fn query(
    &mut self,
    root: ValueId,
    query: Query,
    cancelled: &AtomicBool,
  ) -> Result<BTreeMap<QueryState, Guard>, Error> {
    let mut memo = self.queries.remove(&query).unwrap_or_default();
    let result = self.transfer(
      root,
      (0, None),
      |_, state, value| (query.step(state, value), 0, false),
      &mut memo,
      cancelled,
    );
    self.queries.insert(query, memo);
    Ok(
      result?
        .into_iter()
        .map(|(state, output)| (state, output.guard))
        .collect(),
    )
  }

  fn reverse(
    &mut self,
    root: ValueId,
    cancelled: &AtomicBool,
  ) -> Result<ValueId, Error> {
    let mut pending = vec![(root, false)];
    while let Some((id, visited)) = pending.pop() {
      self.budget.work(1);
      self.budget.check(cancelled)?;
      if self.reversed.contains_key(&id) {
        continue;
      }
      let result = match self.entries[id].expression.clone() {
        Expression::Absent => 1,
        Expression::Literal(value) => {
          let Value::Literal(mut bytes) = value.as_ref().clone() else {
            unreachable!("finite reverse")
          };
          bytes.reverse();
          self.literal(Arc::new(Value::Literal(bytes)))
        }
        Expression::Concat(a, b) | Expression::Choice(_, a, b) if !visited => {
          pending.extend([(id, true), (a, false), (b, false)]);
          continue;
        }
        Expression::Concat(a, b) => {
          self.concatenate(self.reversed[&b], self.reversed[&a])
        }
        Expression::Choice(g, a, b) => {
          self.select(g, self.reversed[&a], self.reversed[&b])
        }
      };
      self.budget.retain(32);
      self.reversed.insert(id, result);
    }
    Ok(self.reversed[&root])
  }

  fn prefix(
    &mut self,
    root: ValueId,
    active: Guard,
    minimum: usize,
    cancelled: &AtomicBool,
  ) -> Result<Vec<u8>, Error> {
    let mut prefix = Vec::new();
    for index in 0..minimum {
      let mut bytes = BTreeSet::new();
      for ((_, byte), guard) in
        self.query(root, Query::Character(index), cancelled)?
      {
        if self.conditions.and(active, guard) != NEVER {
          bytes.insert(byte);
        }
      }
      if bytes.len() != 1 {
        break;
      }
      let Some(byte) = *bytes.first().unwrap() else {
        break;
      };
      prefix.push(byte);
    }
    Ok(prefix)
  }

  fn project(
    &mut self,
    root: ValueId,
    cancelled: &AtomicBool,
  ) -> Result<Value, Error> {
    if self.maximum(root).is_none() {
      return Ok(Value::join_all(
        self
          .candidates(root, cancelled)?
          .keys()
          .map(|id| self.constant(*id).unwrap().as_ref().clone()),
        ValueIssue::Dynamic,
      ));
    }
    let lengths = self.query(root, Query::Length, cancelled)?;
    let first = self.query(root, Query::Character(0), cancelled)?;
    let reverse = self.reverse(root, cancelled)?;
    let mut alphabet = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
      self.budget.work(1);
      self.budget.check(cancelled)?;
      if !seen.insert(id) {
        continue;
      }
      match &self.entries[id].expression {
        Expression::Literal(value) => {
          if let Value::Literal(bytes) = value.as_ref() {
            alphabet.extend(bytes);
          }
        }
        Expression::Choice(_, a, b) | Expression::Concat(a, b) => {
          pending.extend([*a, *b]);
        }
        Expression::Absent => {}
      }
    }
    let mut membership = BTreeMap::new();
    for byte in alphabet {
      let guards = self.query(root, Query::Contains(byte), cancelled)?;
      membership.insert(byte, guards.get(&(1, None)).copied().unwrap_or(NEVER));
    }
    let mut choices = Vec::new();
    for active in first.values().copied() {
      let possible: Vec<_> = lengths
        .iter()
        .filter_map(|((length, _), guard)| {
          (self.conditions.and(active, *guard) != NEVER).then_some(*length)
        })
        .collect();
      let Some(&minimum) = possible.first() else {
        continue;
      };
      let prefix = self.prefix(root, active, minimum, cancelled)?;
      let mut suffix = self.prefix(reverse, active, minimum, cancelled)?;
      suffix.reverse();
      let mut required = BTreeSet::new();
      for (byte, guard) in &membership {
        let absent = self.conditions.not(*guard);
        if self.conditions.and(active, absent) == NEVER {
          required.insert(*byte);
        }
      }
      choices.push(ValueConstraint {
        exact: possible.len() == 1 && prefix.len() == minimum,
        prefix,
        suffix,
        minimum,
        required,
      });
    }
    if choices.len() == 1 && choices[0].exact {
      Ok(Value::Literal(choices.remove(0).prefix))
    } else if choices.is_empty() {
      Ok(Value::Unknown(ValueIssue::Dynamic))
    } else {
      Ok(Value::Partial(PartialValue {
        choices,
        issue: ValueIssue::Dynamic,
      }))
    }
  }

  pub(super) fn candidates(
    &mut self,
    root: ValueId,
    cancelled: &AtomicBool,
  ) -> Result<BTreeMap<ValueId, Guard>, Error> {
    let results = self.transfer(
      root,
      0,
      |values, previous, value| {
        let mut combined = values.constant(*previous).unwrap().as_ref().clone();
        combined.concat(value, usize::MAX);
        (values.literal(Arc::new(combined)), 0, false)
      },
      &mut Memo::new(),
      cancelled,
    )?;
    Ok(
      results
        .into_iter()
        .map(|(id, output)| (id, output.guard))
        .collect(),
    )
  }

  pub fn combinations(
    &mut self,
    roots: &[ValueId],
    active: Guard,
    cancelled: &AtomicBool,
  ) -> Result<BTreeMap<Vec<Arc<Value>>, Guard>, Error> {
    let mut result = BTreeMap::from([(Vec::new(), active)]);
    for root in roots {
      let possible = self.candidates(*root, cancelled)?;
      let mut next = BTreeMap::new();
      for (prefix, guard) in result {
        for (id, condition) in &possible {
          self.budget.work(1);
          self.budget.check(cancelled)?;
          let guard = self.conditions.and(guard, *condition);
          if guard == NEVER {
            continue;
          }
          let mut fields = prefix.clone();
          fields.push(self.constant(*id).expect("concrete value"));
          self
            .budget
            .retain(fields.len() * std::mem::size_of::<Arc<Value>>() + 64);
          next.insert(fields, guard);
        }
      }
      result = next;
    }
    Ok(result)
  }

  pub fn contains(
    &mut self,
    root: ValueId,
    byte: u8,
    cancelled: &AtomicBool,
  ) -> Result<Guard, Error> {
    Ok(
      self
        .query(root, Query::Contains(byte), cancelled)?
        .get(&(1, None))
        .copied()
        .unwrap_or(NEVER),
    )
  }

  pub fn equals(
    &mut self,
    root: ValueId,
    bytes: &[u8],
    cancelled: &AtomicBool,
  ) -> Result<Guard, Error> {
    let mut guard = self
      .query(root, Query::Length, cancelled)?
      .get(&(bytes.len(), None))
      .copied()
      .unwrap_or(NEVER);
    for (index, byte) in bytes.iter().enumerate() {
      let query = self.query(root, Query::Character(index), cancelled)?;
      let mut same = NEVER;
      for ((_, found), candidate) in query {
        if found == Some(*byte) {
          same = self.conditions.or(same, candidate);
        }
      }
      guard = self.conditions.and(guard, same);
    }
    Ok(guard)
  }

  pub fn trim_newlines(
    &mut self,
    root: ValueId,
    cancelled: &AtomicBool,
  ) -> Result<ValueId, Error> {
    if self.maximum(root).is_none() {
      return self.apply(
        root,
        root,
        |value, _| {
          let mut value = value.clone();
          value.trim_newlines();
          value
        },
        cancelled,
      );
    }
    let reversed = self.reverse(root, cancelled)?;
    let outputs = self.transfer(
      reversed,
      true,
      |values, leading, value| {
        let bytes = value.literal().expect("finite output");
        let skip = if *leading {
          bytes.iter().take_while(|byte| **byte == b'\n').count()
        } else {
          0
        };
        let value =
          values.literal(Arc::new(Value::Literal(bytes[skip..].to_vec())));
        (*leading && skip == bytes.len(), value, false)
      },
      &mut Memo::new(),
      cancelled,
    )?;
    let mut result = 1;
    for output in outputs.values() {
      result = self.select(output.guard, output.value, result);
    }
    self.reverse(result, cancelled)
  }

  pub fn ascii_length(
    &mut self,
    root: ValueId,
    cancelled: &AtomicBool,
  ) -> Result<Option<ValueId>, Error> {
    if !self.entries[root].ascii || self.maximum(root).is_none() {
      return Ok(None);
    }
    let mut result = 1;
    for ((length, _), guard) in self.query(root, Query::Length, cancelled)? {
      let value =
        self.literal(Arc::new(Value::Literal(length.to_string().into_bytes())));
      result = self.select(guard, value, result);
    }
    Ok(Some(result))
  }

  pub fn remove(
    &mut self,
    root: ValueId,
    pattern: &crate::interface::application::engine::word::Pattern,
    suffix: bool,
    longest: bool,
    utf8: bool,
    cancelled: &AtomicBool,
  ) -> Result<ValueId, Error> {
    if !self.entries[root].ascii || self.maximum(root).is_none() {
      return self.apply(
        root,
        root,
        |input, _| pattern.remove(input, suffix, longest, utf8, cancelled),
        cancelled,
      );
    }
    let input = if suffix {
      self.reverse(root, cancelled)?
    } else {
      root
    };
    let mut start = pattern.start(suffix);
    if !longest && pattern.accepts_end(&start) {
      start.clear();
    }
    let outputs = self.transfer(
      input,
      start,
      |values, state, value| {
        let mut states = state.clone();
        let bytes = value.literal().expect("finite pattern input");
        let mut cut = None;
        for (index, byte) in bytes.iter().enumerate() {
          if states.is_empty() {
            break;
          }
          values.budget.work(states.len() + 1);
          states = pattern.advance(&states, *byte, suffix);
          if pattern.accepts_end(&states) {
            cut = Some(index + 1);
            if !longest {
              states.clear();
              break;
            }
          }
        }
        let kept = values.literal(Arc::new(Value::Literal(
          bytes[cut.unwrap_or(0)..].to_vec(),
        )));
        (states, kept, cut.is_some())
      },
      &mut Memo::new(),
      cancelled,
    )?;
    let mut result = 1;
    for output in outputs.values() {
      result = self.select(output.guard, output.value, result);
    }
    if suffix {
      self.reverse(result, cancelled)
    } else {
      Ok(result)
    }
  }

  pub fn apply(
    &mut self,
    left: ValueId,
    right: ValueId,
    operation: impl Fn(&Value, &Value) -> Value,
    cancelled: &AtomicBool,
  ) -> Result<ValueId, Error> {
    let left = self.candidates(left, cancelled)?;
    let right = self.candidates(right, cancelled)?;
    let mut result = 1;
    for (a, ga) in left {
      for (b, gb) in &right {
        self.budget.work(1);
        self.budget.check(cancelled)?;
        let guard = self.conditions.and(ga, *gb);
        if guard == NEVER {
          continue;
        }
        let (a, b) = (self.constant(a).unwrap(), self.constant(*b).unwrap());
        let value = self.literal(Arc::new(operation(&a, &b)));
        result = self.select(guard, value, result);
      }
    }
    Ok(result)
  }
}
