use crate::interface::application::engine::workspace::parser;
use crate::interface::application::engine::{OriginPart, Value, ValueIssue};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use tree_sitter::Node;

const IFS_WHITESPACE: &[u8] = b" \t\n\r\x0b\x0c";

#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::interface::application::engine) enum WordContext {
  Argument,
  Assignment,
}

pub(in crate::interface::application::engine) fn literal(
  node: Node<'_>,
  source: &[u8],
) -> Word {
  let mut word = Word::default();
  for part in Parts::new(node, false, source) {
    match part {
      Part::Fragment(fragment) => word.append(fragment, usize::MAX),
      Part::Expansion(_, _) => word.fail(ValueIssue::Dynamic),
      Part::Damaged(_) => word.fail(ValueIssue::Recovery),
      Part::Invalid(issue) => word.fail(issue),
    }
  }
  word
}

/// Does not expand or split fields.
pub(in crate::interface::application::engine) fn literal_value(
  node: Node<'_>,
  source: &[u8],
) -> Value {
  let word = literal(node, source);
  if let Some(issue) = word.issue {
    return Value::Unknown(issue);
  }
  let field = word.joined();
  match Pattern::compile(&field.bytes, &field.quoted, true, true) {
    Ok(pattern) if pattern.is_literal() => Value::Literal(field.bytes),
    _ => Value::Unknown(ValueIssue::PathnameExpansion),
  }
}

pub(in crate::interface::application::engine) fn has_guaranteed_field(
  node: Node<'_>,
  source: &[u8],
) -> bool {
  Parts::new(node, false, source).any(|part| match part {
    Part::Fragment(fragment) => fragment.guarantees_field(),
    Part::Expansion(node, true) => single_field_expansion(node, source),
    _ => false,
  })
}

fn single_field_expansion(node: Node<'_>, source: &[u8]) -> bool {
  node.child_by_field_name("operator").is_none()
    && node
      .child_by_field_name("parameter")
      .is_none_or(|parameter| lexical(parameter, source) != b"@")
}

/// The flag guarantees one field regardless of expansions, IFS or directory contents.
pub(in crate::interface::application::engine) fn syntax_fields(
  node: Node<'_>,
  source: &[u8],
) -> (Vec<Field>, bool) {
  let mut partial = Word::default();
  let mut unknown_fields = false;
  for part in Parts::new(node, false, source) {
    match part {
      Part::Fragment(fragment) => partial.append(fragment, usize::MAX),
      Part::Damaged(_) => partial.fail(ValueIssue::Recovery),
      Part::Invalid(issue) => partial.fail(issue),
      Part::Expansion(expansion, quoted) => {
        partial.extend(
          Word::value(Value::Unknown(ValueIssue::Dynamic), quoted),
          usize::MAX,
        );
        unknown_fields |=
          !(quoted && single_field_expansion(expansion, source));
      }
    }
  }
  let fields = partial
    .fields(
      WordContext::Argument,
      &Value::Unknown(ValueIssue::Dynamic),
      false,
    )
    .unwrap_or_default();
  let single = !unknown_fields
    && fields.len() == 1
    && Pattern::compile(&fields[0].bytes, &fields[0].quoted, true, true)
      .is_ok_and(|pattern| pattern.is_literal());
  (fields, single)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::interface::application::engine) struct Fragment {
  pub bytes: Vec<u8>,
  pub source: Option<Range<usize>>,
  pub quoted: bool,
  pub split: bool,
}

impl Fragment {
  /// Even empty quotes preserve a field.
  fn guarantees_field(&self) -> bool {
    self.quoted || (!self.split && !self.bytes.is_empty())
  }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::interface::application::engine) struct Word {
  pub fragments: Vec<Fragment>,
  pub issue: Option<ValueIssue>,
  unknown: BTreeMap<usize, Value>,
  counts: BTreeMap<usize, FieldCount>,
  /// Unquoted empty positional parameters, whose fields may be discarded.
  empty_parameters: BTreeSet<usize>,
  scalar_value: Option<Value>,
  bytes: usize,
  breaks: Vec<(usize, Option<Vec<u8>>)>,
}

/// Unknown counts describe a run whose fields share the associated value constraint.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::interface::application::engine) enum FieldCount {
  #[default]
  One,
  Unknown {
    minimum: usize,
    issue: ValueIssue,
  },
}

impl FieldCount {
  pub fn boundary(self) -> Result<(), ValueIssue> {
    match self {
      Self::One => Ok(()),
      Self::Unknown { issue, .. } => Err(issue),
    }
  }

  pub fn minimum(self) -> usize {
    match self {
      Self::One => 1,
      Self::Unknown { minimum, .. } => minimum,
    }
  }

  fn concatenate(self, other: Self) -> Self {
    let issue = self
      .boundary()
      .err()
      .into_iter()
      .chain(other.boundary().err())
      .reduce(ValueIssue::combine);
    issue.map_or(Self::One, |issue| Self::Unknown {
      minimum: self
        .minimum()
        .saturating_add(other.minimum())
        .saturating_sub(usize::from(self.minimum() > 0 && other.minimum() > 0)),
      issue,
    })
  }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::interface::application::engine) struct Field {
  pub bytes: Vec<u8>,
  pub origins: Vec<OriginPart>,
  pub quoted: Vec<bool>,
  pub constraint: Option<Value>,
  pub count: FieldCount,
}

impl Field {
  pub fn unknown(value: Value, count: FieldCount) -> Self {
    Self {
      constraint: Some(value),
      count,
      ..Self::default()
    }
  }

  fn mark_unknown(&mut self, value: &Value) {
    let mut constraint = self.value();
    constraint.concat(value, usize::MAX);
    self.constraint = Some(constraint);
  }

  pub fn value(&self) -> Value {
    self
      .constraint
      .clone()
      .unwrap_or_else(|| Value::Literal(self.bytes.clone()))
  }

  fn push(&mut self, fragment: &Fragment, range: Range<usize>) {
    if range.is_empty() {
      return;
    }
    if let Some(value) = &mut self.constraint {
      value.concat(
        &Value::Literal(fragment.bytes[range.clone()].to_vec()),
        usize::MAX,
      );
    }
    let start = self.bytes.len();
    self.bytes.extend_from_slice(&fragment.bytes[range.clone()]);
    self
      .quoted
      .extend(std::iter::repeat_n(fragment.quoted, range.len()));
    let source = fragment.source.as_ref().map(|source| {
      if source.len() == fragment.bytes.len() {
        source.start + range.start..source.start + range.end
      } else {
        source.clone()
      }
    });
    let part = OriginPart::new(start..self.bytes.len(), source);
    if let Some(last) = self.origins.last_mut()
      && last.merge(&part)
    {
      return;
    }
    self.origins.push(part);
  }
}

impl Word {
  pub fn parameters(
    values: &[(Value, FieldCount)],
    quoted: bool,
    separator: Option<&Value>,
    limit: usize,
  ) -> Self {
    let mut word = Self::default();
    for (i, (value, count)) in values.iter().enumerate() {
      if i > 0 {
        word.breaks.push((
          word.fragments.len(),
          separator
            .and_then(|value| value.literal().ok())
            .map(<[u8]>::to_vec),
        ));
      }
      let index = word.fragments.len();
      word.extend(Self::value(value.clone(), quoted), limit);
      if count.boundary().is_err() {
        word.counts.insert(index, *count);
      } else if !quoted && value == &Value::Literal(Vec::new()) {
        word.empty_parameters.insert(index);
      }
    }
    if let Some(separator) = separator
      && (!word.counts.is_empty() || separator.issue().is_some())
    {
      word.scalar_value = Some(join_parameters(values, separator, limit));
    }
    word
  }

  pub fn unknown(issue: ValueIssue) -> Self {
    Self {
      issue: Some(issue),
      ..Self::default()
    }
  }

  pub fn value(value: Value, quoted: bool) -> Self {
    match value {
      Value::Literal(bytes) => Self {
        bytes: bytes.len(),
        fragments: vec![Fragment {
          bytes,
          source: None,
          quoted,
          split: !quoted,
        }],
        ..Self::default()
      },
      value @ (Value::Partial(_) | Value::Unknown(_)) => Self {
        fragments: vec![Fragment {
          bytes: Vec::new(),
          source: None,
          quoted,
          split: !quoted,
        }],
        bytes: value.known_size(),
        unknown: BTreeMap::from([(0, value)]),
        ..Self::default()
      },
    }
  }

  pub fn fail(&mut self, issue: ValueIssue) {
    self.issue = Some(self.issue.map_or(issue, |old| old.combine(issue)));
  }

  /// Rejected bytes leave an empty fragment so later indices stay aligned.
  pub fn append(&mut self, mut fragment: Fragment, limit: usize) {
    if let Some(value) = &mut self.scalar_value {
      value.concat(&Value::Literal(fragment.bytes.clone()), limit);
    }
    self.merge_optional_edge(false);
    if fragment.bytes.contains(&0) {
      self.fail(ValueIssue::Nul);
      fragment.bytes.clear();
    } else if fragment.bytes.len() > limit.saturating_sub(self.bytes) {
      self.fail(ValueIssue::SizeLimit);
      fragment.bytes.clear();
    }
    self.bytes += fragment.bytes.len();
    self.fragments.push(fragment);
  }

  pub fn extend(&mut self, mut other: Self, limit: usize) {
    let scalar = if self.scalar_value.is_some() || other.scalar_value.is_some()
    {
      let mut value = self.scalar();
      value.concat(&other.scalar(), limit);
      Some(value)
    } else {
      None
    };
    self.scalar_value = None;
    if !other.fragments.is_empty() {
      self.merge_optional_edge(false);
    }
    if !self.fragments.is_empty()
      && self
        .breaks
        .last()
        .is_none_or(|(index, _)| *index != self.fragments.len())
    {
      other.merge_optional_edge(true);
    }
    for value in other.unknown.values() {
      let size = value.known_size();
      if size > limit.saturating_sub(self.bytes) {
        self.fail(ValueIssue::SizeLimit);
      } else {
        self.bytes += size;
      }
    }
    self.unknown.extend(
      other
        .unknown
        .iter()
        .map(|(index, issue)| (self.fragments.len() + index, issue.clone())),
    );
    self.empty_parameters.extend(
      other
        .empty_parameters
        .iter()
        .map(|index| self.fragments.len() + index),
    );
    let counts: Vec<_> = other
      .counts
      .iter()
      .map(|(index, count)| (self.fragments.len() + index, *count))
      .collect();
    self
      .breaks
      .extend(other.breaks.iter().map(|(index, separator)| {
        (self.fragments.len() + index, separator.clone())
      }));
    if let Some(issue) = other.issue {
      self.fail(issue);
    }
    for fragment in other.fragments {
      self.append(fragment, limit);
    }
    self.counts.extend(counts);
    self.scalar_value = scalar;
  }

  fn merge_optional_edge(&mut self, first: bool) {
    while !self.breaks.is_empty() {
      let index = if first { 0 } else { self.breaks.len() - 1 };
      let boundary = self.breaks[index].0;
      let range = if first {
        0..boundary
      } else {
        boundary..self.fragments.len()
      };
      if range.is_empty()
        || self
          .part_count(range)
          .is_none_or(|count| count.minimum() > 0)
      {
        break;
      }
      self.breaks.remove(index);
    }
  }

  /// Fragments in `range` with their unknown parts, without field state.
  fn slice(&self, range: Range<usize>) -> Self {
    Self {
      fragments: self.fragments[range.clone()].to_vec(),
      unknown: self
        .unknown
        .range(range.clone())
        .map(|(index, value)| (index - range.start, value.clone()))
        .collect(),
      ..Self::default()
    }
  }

  fn part_count(&self, range: Range<usize>) -> Option<FieldCount> {
    let mut count = None;
    for index in range {
      let next = if let Some(count) = self.counts.get(&index) {
        *count
      } else if self.fragments[index].guarantees_field() {
        FieldCount::One
      } else {
        continue;
      };
      count =
        Some(count.map_or(next, |count: FieldCount| count.concatenate(next)));
    }
    count
  }

  fn scalar_issue(&self) -> Option<ValueIssue> {
    self.issue.map(|issue| self.value_issue(issue))
  }

  pub fn boundary_issue(&self) -> Option<ValueIssue> {
    self
      .issue
      .into_iter()
      .chain(
        self
          .counts
          .values()
          .filter_map(|count| count.boundary().err()),
      )
      .chain(
        self
          .unknown
          .iter()
          .filter(|(index, _)| !self.fragments[**index].quoted)
          .filter_map(|(_, value)| value.issue()),
      )
      .reduce(ValueIssue::combine)
  }

  fn value_issue(&self, issue: ValueIssue) -> ValueIssue {
    self
      .unknown
      .values()
      .filter_map(Value::issue)
      .chain(
        self
          .counts
          .values()
          .filter_map(|count| count.boundary().err()),
      )
      .fold(issue, ValueIssue::combine)
  }

  pub fn with_scalar(mut self, value: Value) -> Self {
    self.scalar_value = Some(value);
    self
  }

  pub fn scalar(&self) -> Value {
    match self.scalar_issue() {
      Some(issue) => Value::Unknown(issue),
      None => self
        .scalar_value
        .clone()
        .unwrap_or_else(|| self.joined().value()),
    }
  }

  /// Unquoted `$*` separators stay unquoted even when field splitting is disabled.
  fn joined(&self) -> Field {
    if let Some(value) = &self.scalar_value {
      return Field::unknown(value.clone(), FieldCount::One);
    }
    let mut field = Field::default();
    let mut breaks = self.breaks.iter().peekable();
    for (i, part) in self.fragments.iter().enumerate() {
      while breaks.peek().is_some_and(|(index, _)| *index == i) {
        if let Some((_, Some(separator))) = breaks.next() {
          field.push(
            &Fragment {
              bytes: separator.clone(),
              source: None,
              quoted: false,
              split: false,
            },
            0..separator.len(),
          );
        }
      }
      if let Some(value) = self.unknown.get(&i) {
        field.mark_unknown(value);
      }
      field.push(part, 0..part.bytes.len());
    }
    field
  }

  pub fn fields(
    &self,
    context: WordContext,
    ifs: &Value,
    utf8: bool,
  ) -> Result<Vec<Field>, ValueIssue> {
    if let Some(issue) = self.issue {
      return Err(self.value_issue(issue));
    }
    if context == WordContext::Assignment {
      return Ok(vec![self.joined()]);
    }
    let mut fields = Vec::new();
    let mut start = 0;
    let mut quoted_parameters = false;
    for end in self
      .breaks
      .iter()
      .map(|(index, _)| *index)
      .chain(std::iter::once(self.fragments.len()))
    {
      quoted_parameters |= start > 0
        && self
          .fragments
          .get(start)
          .is_some_and(|fragment| fragment.quoted);
      let count = self.part_count(start..end).unwrap_or(FieldCount::One);
      let part = self.slice(start..end);
      if let Err(issue) = count.boundary() {
        if end - start == 1 && self.fragments[start].quoted {
          let mut field = part.joined();
          field.count = count;
          fields.push(field);
          start = end;
          continue;
        }
        let value = part
          .unknown
          .values()
          .filter_map(Value::issue)
          .fold(issue, ValueIssue::combine);
        let splitting = self
          .counts
          .range(start..end)
          .any(|(index, _)| !self.fragments[*index].quoted)
          || part
            .unknown
            .keys()
            .any(|index| !part.fragments[*index].quoted);
        let issue = if splitting { value } else { issue };
        fields.extend(self.unknown_fields(
          start..end,
          ifs,
          utf8,
          value,
          issue,
          splitting,
        ));
      } else {
        match part.split(ifs, utf8, usize::MAX) {
          Ok((split, _))
            if split.is_empty()
              && end < self.fragments.len()
              && self.empty_parameters.range(start..end).next().is_some() =>
          {
            match separates_empty_parameters(ifs, quoted_parameters) {
              Ok(true) => fields.push(part.joined()),
              Ok(false) => {}
              Err(issue) => fields.push(Field::unknown(
                Value::Literal(Vec::new()),
                FieldCount::Unknown { minimum: 0, issue },
              )),
            }
          }
          Ok((split, _)) => fields.extend(split),
          Err(issue) => fields.extend(self.unknown_fields(
            start..end,
            ifs,
            utf8,
            issue,
            part.boundary_issue().unwrap_or(issue),
            true,
          )),
        }
      }
      start = end;
    }
    Ok(fields)
  }

  fn unknown_fields(
    &self,
    range: Range<usize>,
    ifs: &Value,
    utf8: bool,
    value: ValueIssue,
    issue: ValueIssue,
    splitting: bool,
  ) -> Vec<Field> {
    let (mut fields, rest, open) =
      self.leading_fields(range.clone(), ifs, utf8, value);
    let minimum = if splitting {
      usize::from((rest..range.end).any(|index| {
        !self.counts.contains_key(&index)
          && self.fragments[index].guarantees_field()
      }))
    } else {
      self
        .part_count(rest..range.end)
        .map_or(0, FieldCount::minimum)
    };
    fields.push(Field::unknown(
      Value::Unknown(value),
      FieldCount::Unknown {
        minimum: minimum.saturating_sub(usize::from(open)),
        issue,
      },
    ));
    fields
  }

  /// Fields unaffected by unknown expansions or globbing, the remaining fragment
  /// index, and whether the last field stays open.
  fn leading_fields(
    &self,
    range: Range<usize>,
    ifs: &Value,
    utf8: bool,
    issue: ValueIssue,
  ) -> (Vec<Field>, usize, bool) {
    let end = |splitting: bool| {
      range.start
        + range
          .clone()
          .take_while(|index| {
            let fragment = &self.fragments[*index];
            !self.counts.contains_key(index)
              && (fragment.quoted
                || (!self.unknown.contains_key(index)
                  && (splitting || !fragment.split)
                  && !fragment.bytes.iter().any(|byte| b"*?[".contains(byte))))
          })
          .count()
    };
    [end(true), end(false)]
      .into_iter()
      .find_map(|end| {
        let (mut fields, open) = self
          .slice(range.start..end)
          .split(ifs, utf8, usize::MAX)
          .ok()?;
        if open {
          fields
            .last_mut()
            .expect("open field")
            .mark_unknown(&Value::Unknown(issue));
        }
        Some((fields, end, open))
      })
      .expect("fields without splitting")
  }

  /// The flag means following bytes continue the last field.
  pub fn split(
    &self,
    ifs: &Value,
    utf8: bool,
    maximum: usize,
  ) -> Result<(Vec<Field>, bool), ValueIssue> {
    if let Some(issue) = self.issue {
      return Err(issue);
    }
    let mut field = Field::default();
    let needs_split = self
      .fragments
      .iter()
      .any(|part| part.split && !part.quoted && !part.bytes.is_empty());
    let separators = if needs_split {
      let Value::Literal(bytes) = ifs else {
        return Err(ifs.issue().expect("unknown IFS"));
      };
      characters(bytes, utf8)
        .map_err(|_| ValueIssue::FieldSplitting)?
        .iter()
        .map(|c| c.to_string().into_bytes())
        .collect::<Vec<_>>()
    } else {
      Vec::new()
    };
    let mut fields = Vec::new();
    let mut keep = false;
    let mut whitespace = false;
    for (index, part) in self.fragments.iter().enumerate() {
      if part.quoted && part.bytes.is_empty() {
        if whitespace && keep {
          fields.push(std::mem::take(&mut field));
        }
        whitespace = false;
        keep = true;
      }
      if let Some(value) = self.unknown.get(&index) {
        if !part.quoted {
          return Err(self.value_issue(value.issue().expect("unknown field")));
        }
        field.mark_unknown(value);
      }
      let mut i = 0;
      while let Some(byte) = part.bytes.get(i) {
        let separator = if part.split && !part.quoted {
          separators
            .iter()
            .find(|s| part.bytes[i..].starts_with(s))
            .map(Vec::len)
        } else {
          None
        };
        if fields.len() + 1 == maximum {
          if separator.is_some() && IFS_WHITESPACE.contains(byte) && !keep {
            i += 1;
            continue;
          }
          field.push(part, i..i + 1);
          i += 1;
          keep = true;
          continue;
        }
        if separator.is_some() && IFS_WHITESPACE.contains(byte) {
          i += 1;
          whitespace = true;
          continue;
        }
        if let Some(length) = separator {
          i += length;
          fields.push(std::mem::take(&mut field));
          keep = false;
          whitespace = false;
          continue;
        }
        if whitespace && keep {
          fields.push(std::mem::take(&mut field));
        }
        whitespace = false;
        field.push(part, i..i + 1);
        i += 1;
        keep = true;
      }
    }
    if maximum != usize::MAX {
      while field.bytes.last().is_some_and(|byte| {
        IFS_WHITESPACE.contains(byte)
          && separators.iter().any(|separator| separator == &[*byte])
      }) && field.quoted.last() == Some(&false)
      {
        field.bytes.pop();
        field.quoted.pop();
      }
    }
    let open = keep && !whitespace;
    if keep {
      fields.push(field);
    }
    Ok((fields, open))
  }
}

/// dash keeps an empty positional parameter before a separator unless IFS
/// starts with IFS white space and no quoted `$@` field precedes it in the word.
fn separates_empty_parameters(
  ifs: &Value,
  quoted_parameters: bool,
) -> Result<bool, ValueIssue> {
  if quoted_parameters {
    return Ok(true);
  }
  match ifs.known_prefix().first() {
    Some(byte) => Ok(!IFS_WHITESPACE.contains(byte)),
    None if ifs.empty() == Some(true) => Ok(true),
    None => Err(ifs.issue().expect("unknown IFS")),
  }
}

fn join_parameters(
  values: &[(Value, FieldCount)],
  separator: &Value,
  limit: usize,
) -> Value {
  let mut result = Value::Literal(Vec::new());
  let mut present = Some(false);
  let mut cause = None;
  for (value, count) in values {
    let issue = count.boundary().err();
    let mut next = match issue {
      None => value.clone(),
      Some(_)
        if separator == &Value::Literal(Vec::new())
          && value == &Value::Literal(Vec::new()) =>
      {
        Value::Literal(Vec::new())
      }
      Some(issue) => Value::Unknown(
        value
          .issue()
          .into_iter()
          .chain(separator.issue())
          .fold(issue, ValueIssue::combine),
      ),
    };
    if present != Some(false) {
      let mut before = separator.clone();
      if present.is_none() {
        before.join_choice(&Value::Literal(Vec::new()), cause);
      }
      before.concat(&next, limit);
      next = before;
    }
    if count.minimum() == 0 {
      next.join_choice(&Value::Literal(Vec::new()), issue);
    }
    result.concat(&next, limit);
    cause = cause.into_iter().chain(issue).reduce(ValueIssue::combine);
    present = if present == Some(true) || count.minimum() > 0 {
      Some(true)
    } else {
      None
    };
  }
  result
}

pub(in crate::interface::application::engine) fn transformed(
  node: Node<'_>,
  source: &[u8],
) -> bool {
  source[node.byte_range()]
    .iter()
    .enumerate()
    .any(|(index, byte)| match byte {
      b'\t' => true,
      b'\\' => {
        let start = node.start_byte() + index;
        node
          .descendant_for_byte_range(start, start + 1)
          .is_none_or(|node| node.kind() != "line_continuation")
      }
      _ => false,
    })
}

pub(in crate::interface::application::engine) fn lexical(
  node: Node<'_>,
  source: &[u8],
) -> Vec<u8> {
  let mut value = Vec::new();
  continued_ranges(node, |range| value.extend_from_slice(&source[range]));
  value
}

/// Visits the bytes of `node` outside its line continuations, in order.
fn continued_ranges(node: Node<'_>, mut visit: impl FnMut(Range<usize>)) {
  let mut start = node.start_byte();
  for child in node.named_children(&mut node.walk()) {
    if child.kind() == "line_continuation" {
      visit(start..child.start_byte());
      start = (child.end_byte() + 1).min(node.end_byte());
    }
  }
  visit(start..node.end_byte());
}

fn dollar_escape(text: &[u8]) -> Result<Vec<u8>, ValueIssue> {
  let rest = text.get(1..).ok_or(ValueIssue::Recovery)?;
  let (&escape, digits) = rest.split_first().ok_or(ValueIssue::Recovery)?;
  let byte = match escape {
    b'a' => 7,
    b'b' => 8,
    b'e' => 27,
    b'f' => 12,
    b'n' => 10,
    b'r' => 13,
    b't' => 9,
    b'v' => 11,
    b'\\' | b'\'' | b'"' => escape,
    b'c' => match digits {
      [b'?'] => 127,
      [b'\\', b'\\'] => 28,
      [operand, continuation @ ..] => {
        let mut bytes = vec![operand.to_ascii_uppercase() & 31];
        bytes.extend_from_slice(continuation);
        return Ok(bytes);
      }
      [] => return Err(ValueIssue::UnsupportedExpansion),
    },
    b'x' => number(digits, 16)?.to_le_bytes()[0],
    b'0'..=b'7' => number(rest, 8)?.to_le_bytes()[0],
    b'u' | b'U' => return Ok(code_point(number(digits, 16)?)),
    _ => return Ok(text.to_vec()),
  };
  Ok(vec![byte])
}

fn number(digits: &[u8], radix: u32) -> Result<u32, ValueIssue> {
  if digits.is_empty() {
    return Ok(0);
  }
  std::str::from_utf8(digits)
    .ok()
    .and_then(|text| u32::from_str_radix(text, radix).ok())
    .ok_or(ValueIssue::Recovery)
}

/// Surrogates keep their three-byte form; values beyond Unicode add no bytes.
fn code_point(value: u32) -> Vec<u8> {
  if (0xd800..=0xdfff).contains(&value) {
    let continuation =
      |shift: u32| 0x80 | ((value >> shift) & 0x3f).to_le_bytes()[0];
    return vec![0xed, continuation(6), continuation(0)];
  }
  char::from_u32(value).map_or_else(Vec::new, |c| c.to_string().into_bytes())
}

pub(in crate::interface::application::engine) enum Part<'a> {
  Fragment(Fragment),
  Expansion(Node<'a>, bool),
  /// Damaged syntax, whose value and effects are unknown.
  Damaged(Node<'a>),
  Invalid(ValueIssue),
}

enum Piece<'a> {
  Node(Node<'a>, bool),
  Text(Range<usize>, bool),
}

pub(in crate::interface::application::engine) struct Parts<'a, 's> {
  node: Node<'a>,
  pending: Vec<Piece<'a>>,
  strip_tabs: bool,
  source: &'s [u8],
  ready: Vec<Fragment>,
}

impl<'a, 's> Parts<'a, 's> {
  pub fn new(node: Node<'a>, quoted: bool, source: &'s [u8]) -> Self {
    Self {
      node,
      pending: vec![Piece::Node(node, quoted)],
      strip_tabs: false,
      source,
      ready: Vec::new(),
    }
  }

  fn here_body(&mut self, node: Node<'a>) {
    let mut pieces = Vec::new();
    let mut start = node.start_byte();
    for child in node.children(&mut node.walk()) {
      if start < child.start_byte() {
        pieces.push(Piece::Text(start..child.start_byte(), true));
      }
      pieces.push(Piece::Node(child, true));
      start = child.end_byte();
      if child.kind() == "line_continuation" {
        start = (start + 1).min(node.end_byte());
      }
    }
    if start < node.end_byte() {
      pieces.push(Piece::Text(start..node.end_byte(), true));
    }
    self.pending.extend(pieces.into_iter().rev());
  }

  fn children(&mut self, node: Node<'a>, quoted: bool) {
    self.pending.extend(
      node
        .named_children(&mut node.walk())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|child| Piece::Node(child, quoted)),
    );
  }

  fn pattern(&mut self, node: Node<'a>, quoted: bool) -> Option<Vec<u8>> {
    let children: Vec<_> = node.children(&mut node.walk()).collect();
    if children
      .iter()
      .all(|child| child.kind() == "line_continuation")
    {
      Some(lexical(node, self.source))
    } else {
      self.pending.extend(
        children
          .into_iter()
          .rev()
          .map(|child| Piece::Node(child, quoted)),
      );
      None
    }
  }
}

/// `start` indicates whether this chunk begins a line of a `<<-` body.
fn strip_leading_tabs(bytes: &mut Vec<u8>, mut start: bool) {
  bytes.retain(|byte| {
    if start && *byte == b'\t' {
      return false;
    }
    start = *byte == b'\n';
    true
  });
}

impl Parts<'_, '_> {
  pub fn strip_tabs(mut self, strip: bool) -> Self {
    self.strip_tabs = strip;
    self
  }

  fn continued(&self, newline: usize) -> bool {
    let Some(backslash) = newline
      .checked_sub(1)
      .filter(|&backslash| self.source[backslash] == b'\\')
    else {
      return false;
    };
    let mut scope = Some(self.node);
    while let Some(node) = scope
      && !node.byte_range().contains(&backslash)
    {
      scope = node.parent();
    }
    scope
      .and_then(|node| node.descendant_for_byte_range(backslash, newline))
      .is_some_and(|node| node.kind() == "line_continuation")
  }

  fn fragment(&self, mut fragment: Fragment) -> Fragment {
    if self.strip_tabs
      && let Some(range) = &fragment.source
      && fragment.bytes.len() == range.len()
    {
      let mut preceding = range.start;
      while preceding > 0 && self.source[preceding - 1] == b'\t' {
        preceding -= 1;
      }
      strip_leading_tabs(
        &mut fragment.bytes,
        preceding == 0
          || (self.source[preceding - 1] == b'\n'
            && !self.continued(preceding - 1)),
      );
      if fragment.bytes.len() != range.len() {
        fragment.source = None;
      }
    }
    fragment
  }

  fn end_quote(&mut self, node: Node<'_>) {
    if let Some(quote) = std::iter::successors(node.parent(), Node::parent)
      .find(|parent| parent.kind() == "dollar_single_quoted")
    {
      self.pending.retain(|part| match part {
        Piece::Node(node, _) => node.start_byte() >= quote.end_byte(),
        Piece::Text(range, _) => range.start >= quote.end_byte(),
      });
    }
  }

  fn literal(&mut self, node: Node<'_>, quoted: bool) {
    let mut ranges = Vec::new();
    continued_ranges(node, |range| ranges.push(range));
    for range in ranges.into_iter().rev() {
      self.ready.push(self.text(range, quoted));
    }
  }

  fn text(&self, range: Range<usize>, quoted: bool) -> Fragment {
    Fragment {
      bytes: self.source[range.clone()].to_vec(),
      source: Some(range),
      quoted,
      split: false,
    }
  }
}

impl<'a> Iterator for Parts<'a, '_> {
  type Item = Part<'a>;
  fn next(&mut self) -> Option<Self::Item> {
    if let Some(fragment) = self.ready.pop() {
      return Some(Part::Fragment(self.fragment(fragment)));
    }
    while let Some(piece) = self.pending.pop() {
      let (node, quoted) = match piece {
        Piece::Node(node, quoted) => (node, quoted),
        Piece::Text(range, quoted) => {
          return Some(Part::Fragment(self.fragment(self.text(range, quoted))));
        }
      };
      if parser::damaged(node) {
        return Some(Part::Damaged(node));
      }
      let mut protected = quoted;
      let bytes = match node.kind() {
        "cmd_name" | "cmd_word" | "word" | "assignment_value"
        | "parameter_word" | "parameter_pattern" | "filename" => {
          self.children(node, quoted);
          continue;
        }
        "here_document_body"
        | "quoted_here_document_body"
        | "here_document_sequence" => {
          self.here_body(node);
          continue;
        }
        "single_quoted" | "double_quoted" | "dollar_single_quoted" => {
          self.children(node, true);
          if node.kind() == "double_quoted"
            && node
              .named_children(&mut node.walk())
              .any(|child| child.kind() != "line_continuation")
          {
            continue;
          }
          return Some(Part::Fragment(Fragment {
            bytes: Vec::new(),
            source: None,
            quoted: true,
            split: false,
          }));
        }
        "parameter_expansion"
        | "arithmetic_expansion"
        | "command_substitution"
        | "backquote_substitution"
        | "tilde_expansion" => return Some(Part::Expansion(node, quoted)),
        "here_document_line_end" => self.source[node.byte_range()].to_vec(),
        "literal"
        | "single_quote_content"
        | "double_quote_text"
        | "dollar_single_quote_text"
        | "here_document_text"
        | "quoted_here_document_text" => {
          self.literal(node, quoted);
          return self
            .ready
            .pop()
            .map(|fragment| Part::Fragment(self.fragment(fragment)));
        }
        "line_continuation" => continue,
        "escaped_character"
        | "double_quote_escape"
        | "here_document_escape" => {
          protected = true;
          lexical(node, self.source)
            .get(1..)
            .map(<[u8]>::to_vec)
            .unwrap_or_default()
        }
        "dollar_single_quote_escape" => {
          match dollar_escape(&lexical(node, self.source)) {
            Ok(mut bytes) => {
              if let Some(nul) = bytes.iter().position(|byte| *byte == 0) {
                bytes.truncate(nul);
                self.end_quote(node);
              }
              bytes
            }
            Err(issue) => return Some(Part::Invalid(issue)),
          }
        }
        kind if kind.starts_with("pattern_") => {
          let Some(bytes) = self.pattern(node, quoted) else {
            continue;
          };
          bytes
        }
        _ if !node.is_named() => self.source[node.byte_range()].to_vec(),
        _ => return Some(Part::Invalid(ValueIssue::Dynamic)),
      };
      return Some(Part::Fragment(self.fragment(Fragment {
        bytes,
        source: Some(node.byte_range()),
        quoted: protected,
        split: false,
      })));
    }
    None
  }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::interface::application) struct Pattern {
  atoms: Vec<Atom>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Atom {
  Literal(char),
  Any,
  Star,
  Class { negated: bool, items: Vec<Item> },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Item {
  Character(char),
  Range(char, char),
  Class(String),
}

fn text(bytes: &[u8], utf8: bool) -> Result<&str, ValueIssue> {
  if !utf8 && !bytes.is_ascii() {
    return Err(ValueIssue::Locale);
  }
  std::str::from_utf8(bytes).map_err(|_| ValueIssue::Locale)
}

pub(in crate::interface::application::engine) fn characters(
  bytes: &[u8],
  utf8: bool,
) -> Result<Vec<char>, ValueIssue> {
  text(bytes, utf8).map(|text| text.chars().collect())
}

impl Pattern {
  pub(in crate::interface::application::engine) fn word(
    word: &Word,
    utf8: bool,
    portable_collation: bool,
  ) -> Result<Self, ValueIssue> {
    if let Some(issue) = word.scalar_issue() {
      return Err(issue);
    }
    Self::field(&word.joined(), utf8, portable_collation)
  }

  pub(in crate::interface::application::engine) fn field(
    field: &Field,
    utf8: bool,
    portable_collation: bool,
  ) -> Result<Self, ValueIssue> {
    if let Some(issue) = field.constraint.as_ref().and_then(Value::issue) {
      return Err(issue);
    }
    Self::compile(&field.bytes, &field.quoted, utf8, portable_collation)
  }

  pub(in crate::interface::application) fn compile(
    bytes: &[u8],
    quoted: &[bool],
    utf8: bool,
    portable_collation: bool,
  ) -> Result<Self, ValueIssue> {
    let chars = text(bytes, utf8)?
      .char_indices()
      .map(|(i, c)| (c, quoted.get(i).copied().unwrap_or(false)))
      .collect::<Vec<_>>();
    let mut atoms = Vec::new();
    let mut i = 0;
    while let Some(&(c, protected)) = chars.get(i) {
      i += 1;
      if protected {
        atoms.push(Atom::Literal(c));
        continue;
      }
      match c {
        '*' => {
          if atoms.last() != Some(&Atom::Star) {
            atoms.push(Atom::Star);
          }
        }
        '?' => atoms.push(Atom::Any),
        '[' => {
          let bracket = bracket(&chars, i, portable_collation)?;
          if let Some((atom, end)) = bracket {
            atoms.push(atom);
            i = end;
          } else {
            atoms.push(Atom::Literal('['));
          }
        }
        // Unquoted backslashes come from expansions and escape the next
        // character.
        '\\' => {
          if let Some(&(next, false)) = chars.get(i) {
            atoms.push(Atom::Literal(next));
            i += 1;
          } else {
            atoms.push(Atom::Literal('\\'));
          }
        }
        _ => atoms.push(Atom::Literal(c)),
      }
    }
    Ok(Self { atoms })
  }

  pub(in crate::interface::application::engine) fn is_literal(&self) -> bool {
    self.atoms.iter().all(|a| matches!(a, Atom::Literal(_)))
  }

  pub(in crate::interface::application) fn literal(&self) -> Option<Vec<u8>> {
    self
      .atoms
      .iter()
      .map(|atom| match atom {
        Atom::Literal(c) => Some(*c),
        _ => None,
      })
      .collect::<Option<String>>()
      .map(String::into_bytes)
  }

  /// A leading period of an entry name is matched only explicitly.
  pub(in crate::interface::application) fn match_entry(
    &self,
    name: &[u8],
    utf8: bool,
    cancelled: &AtomicBool,
  ) -> Result<bool, ValueIssue> {
    let text = characters(name, utf8)?;
    if name.starts_with(b".") && !self.starts_dot() {
      return Ok(false);
    }
    Ok(self.matches(&text, cancelled))
  }

  fn starts_dot(&self) -> bool {
    self.atoms.first() == Some(&Atom::Literal('.'))
  }

  pub(in crate::interface::application::engine) fn matches(
    &self,
    text: &[char],
    cancelled: &AtomicBool,
  ) -> bool {
    Self::prefixes(&self.atoms, text, false, cancelled)
      .last()
      .copied()
      .unwrap_or(false)
  }

  fn prefixes(
    atoms: &[Atom],
    text: &[char],
    reversed: bool,
    cancelled: &AtomicBool,
  ) -> Vec<bool> {
    let mut old = vec![false; text.len() + 1];
    old[0] = true;
    for i in 0..atoms.len() {
      if cancelled.load(Ordering::Relaxed) {
        return vec![];
      }
      let atom = &atoms[if reversed { atoms.len() - 1 - i } else { i }];
      let mut next = vec![false; text.len() + 1];
      if *atom == Atom::Star {
        next[0] = old[0];
      }
      for j in 0..text.len() {
        let c = text[if reversed { text.len() - 1 - j } else { j }];
        next[j + 1] = if *atom == Atom::Star {
          old[j + 1] || next[j]
        } else {
          old[j] && atom.accepts(c)
        };
      }
      old = next;
    }
    old
  }

  pub(in crate::interface::application::engine) fn start(
    &self,
    reversed: bool,
  ) -> Vec<usize> {
    self.closure(vec![0], reversed)
  }

  fn closure(&self, mut states: Vec<usize>, reversed: bool) -> Vec<usize> {
    let mut seen = vec![false; self.atoms.len() + 1];
    while let Some(next) = states.pop() {
      if std::mem::replace(&mut seen[next], true) {
        continue;
      }
      if next < self.atoms.len() && *self.atom(next, reversed) == Atom::Star {
        states.push(next + 1);
      }
    }
    seen
      .into_iter()
      .enumerate()
      .filter_map(|(index, present)| present.then_some(index))
      .collect()
  }

  fn atom(&self, index: usize, reversed: bool) -> &Atom {
    &self.atoms[if reversed {
      self.atoms.len() - 1 - index
    } else {
      index
    }]
  }

  pub(in crate::interface::application::engine) fn advance(
    &self,
    states: &[usize],
    byte: u8,
    reversed: bool,
  ) -> Vec<usize> {
    let next = states
      .iter()
      .filter_map(|index| {
        if *index == self.atoms.len() {
          return None;
        }
        let atom = self.atom(*index, reversed);
        if *atom == Atom::Star {
          Some(*index)
        } else {
          atom.accepts(char::from(byte)).then_some(index + 1)
        }
      })
      .collect();
    self.closure(next, reversed)
  }

  pub(in crate::interface::application::engine) fn accepts_end(
    &self,
    states: &[usize],
  ) -> bool {
    states.last() == Some(&self.atoms.len())
  }

  pub(in crate::interface::application::engine) fn remove(
    &self,
    value: &Value,
    suffix: bool,
    longest: bool,
    utf8: bool,
    cancelled: &AtomicBool,
  ) -> Value {
    let Value::Literal(bytes) = value else {
      if self.atoms.is_empty() {
        return value.clone();
      }
      return Value::Unknown(value.issue().expect("unknown pattern operand"));
    };
    let chars = match characters(bytes, utf8) {
      Ok(chars) => chars,
      Err(issue) => return Value::Unknown(issue),
    };
    let matches = Self::prefixes(&self.atoms, &chars, suffix, cancelled);
    let count = if longest {
      matches.iter().rposition(|matched| *matched)
    } else {
      matches.iter().position(|matched| *matched)
    };
    let Some(count) = count else {
      return value.clone();
    };
    let kept = if suffix {
      &chars[..chars.len() - count]
    } else {
      &chars[count..]
    };
    Value::Literal(kept.iter().collect::<String>().into_bytes())
  }
}

impl Atom {
  fn accepts(&self, c: char) -> bool {
    match self {
      Self::Literal(literal) => *literal == c,
      Self::Any | Self::Star => true,
      Self::Class { negated, items } => {
        items.iter().any(|item| item.accepts(c)) != *negated
      }
    }
  }
}

impl Item {
  fn accepts(&self, c: char) -> bool {
    match self {
      Self::Character(literal) => *literal == c,
      Self::Range(start, end) => (*start..=*end).contains(&c),
      Self::Class(class) => match class.as_str() {
        "alnum" => c.is_ascii_alphanumeric(),
        "alpha" => c.is_ascii_alphabetic(),
        "blank" => matches!(c, ' ' | '\t'),
        "cntrl" => c.is_ascii_control(),
        "digit" => c.is_ascii_digit(),
        "graph" => c.is_ascii_graphic(),
        "lower" => c.is_ascii_lowercase(),
        "print" => c.is_ascii_graphic() || c == ' ',
        "punct" => c.is_ascii_punctuation(),
        "space" => matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'),
        "upper" => c.is_ascii_uppercase(),
        "xdigit" => c.is_ascii_hexdigit(),
        _ => false,
      },
    }
  }
}

fn bracket(
  chars: &[(char, bool)],
  start: usize,
  portable: bool,
) -> Result<Option<(Atom, usize)>, ValueIssue> {
  let mut i = start;
  let negated = matches!(chars.get(i), Some(('!' | '^', false)));
  if negated {
    i += 1;
  }
  let first = i;
  let mut items = Vec::new();
  let mut invalid = false;
  while let Some(&(c, quoted)) = chars.get(i) {
    if c == ']' && !quoted && i > first {
      let atom = if invalid {
        Atom::Class {
          negated: false,
          items: Vec::new(),
        }
      } else {
        Atom::Class { negated, items }
      };
      return Ok(Some((atom, i + 1)));
    }
    let (item, next, equivalence) = if c == '['
      && !quoted
      && chars
        .get(i + 1)
        .is_some_and(|(c, q)| !q && matches!(c, ':' | '.' | '='))
    {
      if !portable {
        return Err(ValueIssue::Locale);
      }
      let delimiter = chars[i + 1].0;
      let end = (i + 2..chars.len().saturating_sub(1)).find(|n| {
        chars[*n] == (delimiter, false) && chars[*n + 1] == (']', false)
      });
      let Some(end) = end else {
        return Ok(None);
      };
      let text = chars[i + 2..end].iter().map(|p| p.0).collect::<String>();
      let item = if delimiter == ':' {
        if !matches!(
          text.as_str(),
          "alnum"
            | "alpha"
            | "blank"
            | "cntrl"
            | "digit"
            | "graph"
            | "lower"
            | "print"
            | "punct"
            | "space"
            | "upper"
            | "xdigit"
        ) {
          invalid = true;
        }
        Item::Class(text)
      } else if text.len() == 1 && text.is_ascii() {
        Item::Character(text.chars().next().expect("one character"))
      } else {
        return Err(ValueIssue::Locale);
      };
      (item, end + 2, delimiter == '=')
    } else if c == '\\' && !quoted && chars.get(i + 1).is_some() {
      (Item::Character(chars[i + 1].0), i + 2, false)
    } else {
      (Item::Character(c), i + 1, false)
    };
    if matches!(item, Item::Character(_))
      && !equivalence
      && chars.get(next) == Some(&('-', false))
      && chars.get(next + 1).is_some_and(|p| *p != (']', false))
    {
      if !portable {
        return Err(ValueIssue::Locale);
      }
      let Item::Character(from) = item else {
        unreachable!()
      };
      let to = chars[next + 1].0;
      if !from.is_ascii() || !to.is_ascii() {
        return Err(ValueIssue::Locale);
      }
      items.push(Item::Range(from, to));
      i = next + 2;
    } else {
      items.push(item);
      i = next;
    }
  }
  Ok(None)
}
