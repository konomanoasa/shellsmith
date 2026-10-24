use crate::interface::application::engine::{Error, Value, check_cancelled};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub(in crate::interface::application::engine::analysis) type ValueId = usize;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Expression {
  Literal(Arc<Value>),
  Choice {
    choice: usize,
    left: ValueId,
    right: ValueId,
  },
  Concat(ValueId, ValueId),
}

#[derive(Debug)]
struct Entry {
  expression: Expression,
  projected: Arc<Value>,
  choices: BTreeSet<usize>,
  width: Option<usize>,
  maximum: Option<usize>,
  ascii: bool,
}

#[derive(Debug, Default)]
pub(in crate::interface::application::engine::analysis) struct Values {
  entries: Vec<Entry>,
  index: BTreeMap<Expression, ValueId>,
}

impl Values {
  fn intern(&mut self, entry: Entry) -> ValueId {
    if let Some(id) = self.index.get(&entry.expression) {
      return *id;
    }
    let id = self.entries.len();
    self.index.insert(entry.expression.clone(), id);
    self.entries.push(entry);
    id
  }

  pub fn literal(&mut self, value: Arc<Value>) -> ValueId {
    let (width, ascii) = match value.as_ref() {
      Value::Literal(bytes) => (Some(bytes.len()), bytes.is_ascii()),
      _ => (None, false),
    };
    let maximum = width.filter(|_| !matches!(value.as_ref(), Value::Literal(bytes) if bytes.contains(&0)));
    self.intern(Entry {
      expression: Expression::Literal(value.clone()),
      projected: value,
      choices: BTreeSet::new(),
      width,
      maximum,
      ascii,
    })
  }

  pub fn projected(&self, id: ValueId) -> &Arc<Value> {
    &self.entries[id].projected
  }

  pub fn maximum(&self, id: ValueId) -> Option<usize> {
    self.entries[id].maximum
  }

  /// Character counts of ASCII values; differing widths are counted per path.
  pub fn ascii_length(
    &mut self,
    id: ValueId,
    cancelled: &AtomicBool,
  ) -> Result<Option<ValueId>, Error> {
    let entry = &self.entries[id];
    if !entry.ascii {
      return Ok(None);
    }
    let count = |width: usize| Value::Literal(width.to_string().into_bytes());
    if let Some(width) = entry.width {
      return Ok(Some(self.literal(Arc::new(count(width)))));
    }
    self
      .apply(id, id, |value, _| count(value.known_size()), cancelled)
      .map(Some)
  }

  pub fn choice(&self, id: ValueId) -> Option<usize> {
    self.entries[id].choices.last().copied()
  }

  pub fn branch(
    &mut self,
    choice: usize,
    left: ValueId,
    right: ValueId,
  ) -> ValueId {
    if left == right {
      return left;
    }
    let a = &self.entries[left];
    let b = &self.entries[right];
    let mut projected = a.projected.as_ref().clone();
    projected.join(&b.projected);
    let mut choices: BTreeSet<_> =
      a.choices.union(&b.choices).copied().collect();
    choices.insert(choice);
    self.intern(Entry {
      expression: Expression::Choice {
        choice,
        left,
        right,
      },
      projected: Arc::new(projected),
      choices,
      width: a.width.filter(|width| Some(*width) == b.width),
      maximum: a.maximum.zip(b.maximum).map(|(a, b)| a.max(b)),
      ascii: a.ascii && b.ascii,
    })
  }

  /// Values that cannot be concatenated independently are concatenated per
  /// path, so each path keeps its exact string.
  pub fn concat(
    &mut self,
    left: ValueId,
    right: ValueId,
    cancelled: &AtomicBool,
  ) -> Result<Option<ValueId>, Error> {
    if let Some(id) = self.independent_concat(left, right) {
      return Ok(Some(id));
    }
    if self.entries[left].maximum.is_none()
      || self.entries[right].maximum.is_none()
    {
      return Ok(None);
    }
    self
      .apply(
        left,
        right,
        |a, b| {
          let mut value = a.clone();
          value.concat(b, usize::MAX);
          value
        },
        cancelled,
      )
      .map(Some)
  }

  /// Fixed widths and independent choices let concatenation commute with
  /// projection without introducing strings from incompatible branches.
  fn independent_concat(
    &mut self,
    left: ValueId,
    right: ValueId,
  ) -> Option<ValueId> {
    if self.entries[left].width == Some(0) {
      return Some(right);
    }
    if self.entries[right].width == Some(0) {
      return Some(left);
    }
    let a = &self.entries[left];
    let b = &self.entries[right];
    a.maximum?;
    b.maximum?;
    let width = a.width?.checked_add(b.width?)?;
    if !a.choices.is_disjoint(&b.choices) {
      return None;
    }
    let mut projected = a.projected.as_ref().clone();
    projected.concat(&b.projected, usize::MAX);
    if a.choices.is_empty() && b.choices.is_empty() {
      return Some(self.literal(Arc::new(projected)));
    }
    let entry = Entry {
      expression: Expression::Concat(left, right),
      projected: Arc::new(projected),
      choices: a.choices.union(&b.choices).copied().collect(),
      width: Some(width),
      maximum: Some(width),
      ascii: a.ascii && b.ascii,
    };
    Some(self.intern(entry))
  }

  pub fn restrict(
    &mut self,
    root: ValueId,
    choice: usize,
    side: bool,
  ) -> ValueId {
    let mut cache = HashMap::new();
    let mut pending = vec![(root, false)];
    while let Some((id, visited)) = pending.pop() {
      if cache.contains_key(&id) {
        continue;
      }
      let entry = &self.entries[id];
      if !entry.choices.contains(&choice) {
        cache.insert(id, id);
        continue;
      }
      let (left, right) = match entry.expression {
        Expression::Choice {
          choice: current,
          left,
          right,
        } if current == choice => {
          cache.insert(id, if side { right } else { left });
          continue;
        }
        Expression::Choice { left, right, .. }
        | Expression::Concat(left, right) => (left, right),
        Expression::Literal(_) => unreachable!(),
      };
      if !visited {
        pending.extend([(id, true), (right, false), (left, false)]);
        continue;
      }
      let result = match self.entries[id].expression {
        Expression::Choice { choice, .. } => {
          self.branch(choice, cache[&left], cache[&right])
        }
        Expression::Concat(..) => self
          .independent_concat(cache[&left], cache[&right])
          .expect("restricted independent concatenation"),
        Expression::Literal(_) => unreachable!(),
      };
      cache.insert(id, result);
    }
    cache[&root]
  }

  pub fn apply(
    &mut self,
    left: ValueId,
    right: ValueId,
    operation: impl Fn(&Value, &Value) -> Value,
    cancelled: &AtomicBool,
  ) -> Result<ValueId, Error> {
    let mut cache = HashMap::new();
    let mut pending = vec![(left, right, false)];
    let mut branches = HashMap::new();
    while let Some((a, b, visited)) = pending.pop() {
      check_cancelled(cancelled)?;
      if cache.contains_key(&(a, b)) {
        continue;
      }
      let choice = self.choice(a).max(self.choice(b));
      let Some(choice) = choice else {
        let value = operation(self.projected(a), self.projected(b));
        let result = self.literal(Arc::new(value));
        cache.insert((a, b), result);
        continue;
      };
      if !visited {
        let low = (
          self.restrict(a, choice, false),
          self.restrict(b, choice, false),
        );
        let high = (
          self.restrict(a, choice, true),
          self.restrict(b, choice, true),
        );
        branches.insert((a, b), (low, high));
        pending.extend([
          (a, b, true),
          (high.0, high.1, false),
          (low.0, low.1, false),
        ]);
        continue;
      }
      let (low, high) = branches[&(a, b)];
      let result = self.branch(choice, cache[&low], cache[&high]);
      cache.insert((a, b), result);
    }
    Ok(cache[&(left, right)])
  }
}
