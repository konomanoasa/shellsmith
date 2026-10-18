use crate::interface::application::engine::{OriginPart, Value, ValueIssue};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use tree_sitter::Node;

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
  for part in Parts::new(node, source) {
    match part {
      Part::Fragment(fragment) => word.append(fragment, usize::MAX),
      Part::Expansion(_, _) => word.fail(ValueIssue::Dynamic),
      Part::Invalid(issue) => word.fail(issue),
    }
  }
  word
}

pub(in crate::interface::application::engine) fn literal_value(
  node: Node<'_>,
  source: &[u8],
) -> Value {
  match literal(node, source).fields(
    WordContext::Argument,
    &Value::Literal(b" \t\n".to_vec()),
    false,
  ) {
    Ok(fields) if fields.len() == 1 => {
      let field = &fields[0];
      match Pattern::compile(&field.bytes, &field.quoted, true, true) {
        Ok(pattern) if pattern.is_literal() => {
          Value::Literal(field.bytes.clone())
        }
        _ => Value::Unknown(ValueIssue::PathnameExpansion),
      }
    }
    Ok(fields) if fields.is_empty() => Value::Literal(Vec::new()),
    Ok(_) => Value::Unknown(ValueIssue::FieldSplitting),
    Err(issue) => Value::Unknown(issue),
  }
}

pub(in crate::interface::application::engine) fn has_guaranteed_field(
  node: Node<'_>,
  source: &[u8],
) -> bool {
  Parts::new(node, source).any(|part| match part {
    Part::Fragment(fragment) => fragment.quoted || !fragment.bytes.is_empty(),
    Part::Expansion(node, true) => {
      node.child_by_field_name("operator").is_none()
        && node
          .child_by_field_name("parameter")
          .is_none_or(|parameter| lexical(parameter, source) != b"@")
    }
    _ => false,
  })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::interface::application::engine) struct Fragment {
  pub bytes: Vec<u8>,
  pub source: Option<Range<usize>>,
  pub quoted: bool,
  pub split: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::interface::application::engine) struct Word {
  pub fragments: Vec<Fragment>,
  pub issue: Option<ValueIssue>,
  pub unknown_fields: bool,
  pub field_issue: Option<ValueIssue>,
  pub ambiguous_empty: bool,
  unknown: BTreeMap<usize, Value>,
  bytes: usize,
  breaks: Vec<(usize, Option<Vec<u8>>)>,
  assignment_issue: Option<ValueIssue>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::interface::application::engine) struct Field {
  pub bytes: Vec<u8>,
  pub origins: Vec<OriginPart>,
  pub quoted: Vec<bool>,
  pub constraint: Option<Value>,
}

impl Field {
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
    values: &[Value],
    quoted: bool,
    separator: Option<&[u8]>,
    limit: usize,
  ) -> Self {
    let mut word = Self {
      unknown_fields: values.len() != 1,
      ..Self::default()
    };
    if separator.is_none() {
      word.assignment_issue = Some(ValueIssue::Unspecified);
    }
    for (i, value) in values.iter().enumerate() {
      if i > 0 {
        word
          .breaks
          .push((word.fragments.len(), separator.map(<[u8]>::to_vec)));
      }
      word.extend(Self::value(value.clone(), quoted), limit);
    }
    word
  }

  pub fn unknown(issue: ValueIssue) -> Self {
    Self {
      issue: Some(issue),
      unknown_fields: true,
      ..Self::default()
    }
  }

  pub fn value(value: Value, quoted: bool) -> Self {
    match value {
      Value::Literal(bytes) => {
        let len = bytes.len();
        Self {
          fragments: vec![Fragment {
            bytes,
            source: None,
            quoted,
            split: !quoted,
          }],
          bytes: len,
          issue: None,
          unknown_fields: false,
          field_issue: None,
          ambiguous_empty: false,
          breaks: Vec::new(),
          assignment_issue: None,
          unknown: BTreeMap::new(),
        }
      }
      value @ (Value::Partial(_) | Value::Unknown(_)) => Self {
        fragments: vec![Fragment {
          bytes: Vec::new(),
          source: None,
          quoted,
          split: !quoted,
        }],
        bytes: value.known_size(),
        unknown: BTreeMap::from([(0, value)]),
        unknown_fields: !quoted,
        ..Self::default()
      },
    }
  }

  pub fn fail(&mut self, issue: ValueIssue) {
    self.issue = Some(self.issue.map_or(issue, |old| old.combine(issue)));
  }

  pub fn append(&mut self, fragment: Fragment, limit: usize) {
    if fragment.bytes.contains(&0) {
      self.fail(ValueIssue::Nul);
    } else if fragment.bytes.len() > limit.saturating_sub(self.bytes) {
      self.fail(ValueIssue::SizeLimit);
    } else if self.issue.is_none() {
      self.bytes += fragment.bytes.len();
      self.fragments.push(fragment);
    }
  }

  pub fn extend(&mut self, other: Self, limit: usize) {
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
    self
      .breaks
      .extend(other.breaks.iter().map(|(index, separator)| {
        (self.fragments.len() + index, separator.clone())
      }));
    self.assignment_issue = self.assignment_issue.or(other.assignment_issue);
    self.unknown_fields |= other.unknown_fields;
    self.ambiguous_empty |= other.ambiguous_empty;
    self.field_issue = self.field_issue.or(other.field_issue);
    if let Some(issue) = other.issue {
      self.fail(issue);
    }
    for fragment in other.fragments {
      self.append(fragment, limit);
    }
  }

  pub fn scalar(&self) -> Value {
    if let Some(issue) = self
      .issue
      .into_iter()
      .chain(self.assignment_issue)
      .reduce(ValueIssue::combine)
    {
      return Value::Unknown(issue);
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
              quoted: true,
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
    field.value()
  }

  pub fn fields(
    &self,
    context: WordContext,
    ifs: &Value,
    utf8: bool,
  ) -> Result<Vec<Field>, ValueIssue> {
    if let Some(issue) = self.issue {
      return Err(issue);
    }
    let mut field = Field::default();
    if context == WordContext::Assignment {
      if let Some(issue) = self.assignment_issue {
        return Err(issue);
      }
      if self.breaks.is_empty() {
        for (index, part) in self.fragments.iter().enumerate() {
          if let Some(value) = self.unknown.get(&index) {
            field.mark_unknown(value);
          }
          field.push(part, 0..part.bytes.len());
        }
      } else {
        let Value::Literal(bytes) = self.scalar() else {
          return Err(ValueIssue::Unspecified);
        };
        let len = bytes.len();
        field.push(
          &Fragment {
            bytes,
            source: None,
            quoted: true,
            split: false,
          },
          0..len,
        );
      }
      return Ok(vec![field]);
    }
    if let Some(issue) = self.field_issue {
      return Err(issue);
    }
    if self.ambiguous_empty
      && self.fragments.iter().all(|part| part.bytes.is_empty())
    {
      return Err(ValueIssue::Unspecified);
    }
    let mut fields = Vec::new();
    let mut start = 0;
    for end in self
      .breaks
      .iter()
      .map(|(index, _)| *index)
      .chain(std::iter::once(self.fragments.len()))
    {
      let part = Self {
        fragments: self.fragments[start..end].to_vec(),
        unknown: self
          .unknown
          .range(start..end)
          .map(|(i, issue)| (i - start, issue.clone()))
          .collect(),
        ..Self::default()
      };
      fields.extend(part.split(ifs, utf8, usize::MAX)?);
      start = end;
    }
    Ok(fields)
  }

  pub fn split(
    &self,
    ifs: &Value,
    utf8: bool,
    maximum: usize,
  ) -> Result<Vec<Field>, ValueIssue> {
    let mut field = Field::default();
    let needs_split = self
      .fragments
      .iter()
      .any(|part| part.split && !part.quoted && !part.bytes.is_empty());
    let separators = if needs_split {
      let Value::Literal(bytes) = ifs else {
        return Err(ValueIssue::FieldSplitting);
      };
      if bytes.iter().any(|byte| b"\r\x0b\x0c".contains(byte)) {
        return Err(ValueIssue::FieldSplitting);
      }
      if bytes.is_ascii() {
        bytes.iter().map(|byte| vec![*byte]).collect::<Vec<_>>()
      } else if utf8 {
        characters(bytes, true)
          .map_err(|_| ValueIssue::FieldSplitting)?
          .iter()
          .map(|c| c.to_string().into_bytes())
          .collect()
      } else {
        return Err(ValueIssue::FieldSplitting);
      }
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
          return Err(value.issue().expect("unknown field"));
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
          if separator.is_some() && b" \t\n".contains(byte) && !keep {
            i += 1;
            continue;
          }
          field.push(part, i..i + 1);
          i += 1;
          keep = true;
          continue;
        }
        if separator.is_some() && b" \t\n".contains(byte) {
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
        b" \t\n".contains(byte)
          && separators.iter().any(|separator| separator == &[*byte])
      }) && field.quoted.last() == Some(&false)
      {
        field.bytes.pop();
        field.quoted.pop();
      }
    }
    if keep {
      fields.push(field);
    }
    Ok(fields)
  }
}

pub(in crate::interface::application::engine) fn transformed(
  node: Node<'_>,
  source: &[u8],
) -> bool {
  source[node.byte_range()]
    .iter()
    .any(|byte| matches!(byte, b'\\' | b'\t'))
}

pub(in crate::interface::application::engine) fn lexical(
  node: Node<'_>,
  source: &[u8],
) -> Vec<u8> {
  let mut value = Vec::new();
  let mut start = node.start_byte();
  for child in node.named_children(&mut node.walk()) {
    if child.kind() == "line_continuation" {
      value.extend_from_slice(&source[start..child.start_byte()]);
      start = (child.end_byte() + 1).min(node.end_byte());
    }
  }
  value.extend_from_slice(&source[start..node.end_byte()]);
  value
}

fn dollar_escape(text: &[u8]) -> Result<u8, ValueIssue> {
  let rest = text.get(1..).ok_or(ValueIssue::Recovery)?;
  let (&escape, digits) = rest.split_first().ok_or(ValueIssue::Recovery)?;
  let byte = match escape {
    b'a' => 7,
    b'b' => 8,
    b'f' => 12,
    b'n' => 10,
    b'r' => 13,
    b't' => 9,
    b'v' => 11,
    b'\\' | b'\'' | b'"' => escape,
    b'e' | b'c' => return Err(ValueIssue::LocaleDependentEscape),
    b'x' if (1..=2).contains(&digits.len()) => return number(digits, 16),
    b'0'..=b'7' if rest.len() <= 3 => return number(rest, 8),
    _ => return Err(ValueIssue::UnspecifiedEscape),
  };
  Ok(byte)
}

fn number(digits: &[u8], radix: u32) -> Result<u8, ValueIssue> {
  std::str::from_utf8(digits)
    .ok()
    .and_then(|text| u8::from_str_radix(text, radix).ok())
    .ok_or(ValueIssue::UnspecifiedEscape)
}

pub(in crate::interface::application::engine) enum Part<'a> {
  Fragment(Fragment),
  Expansion(Node<'a>, bool),
  Invalid(ValueIssue),
}

enum Piece<'a> {
  Node(Node<'a>, bool),
  Text(Range<usize>, bool),
}

pub(in crate::interface::application::engine) struct Parts<'a, 's> {
  pending: Vec<Piece<'a>>,
  strip_tabs: bool,
  source: &'s [u8],
  ready: Vec<Fragment>,
}
impl<'a, 's> Parts<'a, 's> {
  pub fn new(node: Node<'a>, source: &'s [u8]) -> Self {
    Self {
      pending: vec![Piece::Node(node, false)],
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
impl Parts<'_, '_> {
  pub fn strip_tabs(mut self, strip: bool) -> Self {
    self.strip_tabs = strip;
    self
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
      let mut start = preceding == 0 || self.source[preceding - 1] == b'\n';
      fragment.bytes.retain(|byte| {
        if start && *byte == b'\t' {
          return false;
        }
        start = *byte == b'\n';
        true
      });
      if fragment.bytes.len() != range.len() {
        fragment.source = None;
      }
    }
    fragment
  }
  fn literal(&mut self, node: Node<'_>, quoted: bool) {
    let mut start = node.start_byte();
    for child in node.named_children(&mut node.walk()) {
      if child.kind() == "line_continuation" {
        let range = start..child.start_byte();
        self.ready.push(Fragment {
          bytes: self.source[range.clone()].to_vec(),
          source: Some(range),
          quoted,
          split: false,
        });
        start = (child.end_byte() + 1).min(node.end_byte());
      }
    }
    let range = start..node.end_byte();
    self.ready.push(Fragment {
      bytes: self.source[range.clone()].to_vec(),
      source: Some(range),
      quoted,
      split: false,
    });
    self.ready.reverse();
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
          return Some(Part::Fragment(self.fragment(Fragment {
            bytes: self.source[range.clone()].to_vec(),
            source: Some(range),
            quoted,
            split: false,
          })));
        }
      };
      if crate::interface::application::engine::workspace::parser::damaged(node)
        || node.is_missing()
      {
        return Some(Part::Invalid(ValueIssue::Recovery));
      }
      let mut protected = quoted;
      let bytes = match node.kind() {
        "cmd_name" | "cmd_word" | "word" | "assignment_value"
        | "parameter_word" | "parameter_pattern" | "filename" => {
          self.children(node, quoted);
          continue;
        }
        "here_document_body" | "here_document_sequence" => {
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
        "single_quote_content" | "here_document_line_end" => {
          self.source[node.byte_range()].to_vec()
        }
        "literal"
        | "double_quote_text"
        | "dollar_single_quote_text"
        | "here_document_text" => {
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
            Ok(byte) => vec![byte],
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
pub(crate) struct Pattern {
  atoms: Vec<Atom>,
  unclosed_bracket: bool,
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

pub(crate) fn characters(
  bytes: &[u8],
  utf8: bool,
) -> Result<Vec<char>, ValueIssue> {
  if !utf8 && !bytes.is_ascii() {
    return Err(ValueIssue::Locale);
  }
  std::str::from_utf8(bytes)
    .map(|s| s.chars().collect())
    .map_err(|_| ValueIssue::Locale)
}

impl Pattern {
  pub(in crate::interface::application::engine) fn word(
    word: &Word,
    utf8: bool,
    portable_collation: bool,
  ) -> Result<Self, ValueIssue> {
    if let Some(issue) = word.scalar().issue() {
      return Err(issue);
    }
    let bytes = word
      .fragments
      .iter()
      .flat_map(|p| p.bytes.iter().copied())
      .collect::<Vec<_>>();
    let quoted = word
      .fragments
      .iter()
      .flat_map(|p| std::iter::repeat_n(p.quoted, p.bytes.len()))
      .collect::<Vec<_>>();
    Self::compile(&bytes, &quoted, utf8, portable_collation)
  }

  pub(crate) fn compile(
    bytes: &[u8],
    quoted: &[bool],
    utf8: bool,
    portable_collation: bool,
  ) -> Result<Self, ValueIssue> {
    characters(bytes, utf8)?;
    let text = std::str::from_utf8(bytes).map_err(|_| ValueIssue::Locale)?;
    let chars = text
      .char_indices()
      .map(|(i, c)| (c, quoted.get(i).copied().unwrap_or(false)))
      .collect::<Vec<_>>();
    let mut atoms = Vec::new();
    let mut unclosed_bracket = false;
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
          if let Some((atom, end)) = bracket(&chars, i, portable_collation)? {
            atoms.push(atom);
            i = end;
          } else {
            unclosed_bracket = true;
            atoms.push(Atom::Literal('['));
          }
        }
        _ => atoms.push(Atom::Literal(c)),
      }
    }
    Ok(Self {
      atoms,
      unclosed_bracket,
    })
  }

  pub(crate) fn is_literal(&self) -> bool {
    self.atoms.iter().all(|a| matches!(a, Atom::Literal(_)))
  }
  pub(crate) fn ambiguous_glob(&self, cancelled: &AtomicBool) -> bool {
    !self.is_literal()
      && (self.unclosed_bracket
        || (self.starts_dot()
          && (self.matches(&['.'], cancelled)
            || self.matches(&['.', '.'], cancelled))))
  }

  pub(crate) fn ambiguous_dot(&self) -> bool {
    matches!(self.atoms.first(), Some(Atom::Class { negated: false, items }) if items.contains(&Item::Character('.')))
  }

  pub(crate) fn starts_dot(&self) -> bool {
    self.atoms.first() == Some(&Atom::Literal('.'))
  }

  pub(crate) fn matches(&self, text: &[char], cancelled: &AtomicBool) -> bool {
    self
      .prefixes(text, false, cancelled)
      .last()
      .copied()
      .unwrap_or(false)
  }

  fn prefixes(
    &self,
    text: &[char],
    reversed: bool,
    cancelled: &AtomicBool,
  ) -> Vec<bool> {
    let mut old = vec![false; text.len() + 1];
    old[0] = true;
    for i in 0..self.atoms.len() {
      if cancelled.load(Ordering::Relaxed) {
        return vec![];
      }
      let atom = &self.atoms[if reversed {
        self.atoms.len() - 1 - i
      } else {
        i
      }];
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

  pub(crate) fn remove(
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
    let matches = self.prefixes(&chars, suffix, cancelled);
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
  let negated = chars.get(i) == Some(&('!', false));
  if negated {
    i += 1;
  }
  let first = i;
  let mut items = Vec::new();
  while let Some(&(c, quoted)) = chars.get(i) {
    if c == ']' && !quoted && i > first {
      return Ok(Some((Atom::Class { negated, items }, i + 1)));
    }
    let (item, next) = if c == '['
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
          return Err(ValueIssue::Unspecified);
        }
        Item::Class(text)
      } else if text.len() == 1 && text.is_ascii() {
        Item::Character(text.chars().next().expect("one character"))
      } else {
        return Err(ValueIssue::Locale);
      };
      (item, end + 2)
    } else {
      (Item::Character(c), i + 1)
    };
    if chars.get(next) == Some(&('-', false))
      && chars.get(next + 1).is_some_and(|p| *p != (']', false))
    {
      if !portable {
        return Err(ValueIssue::Locale);
      }
      let Item::Character(from) = item else {
        return Err(ValueIssue::Unspecified);
      };
      let to = chars[next + 1].0;
      if from > to || !from.is_ascii() || !to.is_ascii() || to == '[' {
        return Err(ValueIssue::Unspecified);
      }
      items.push(Item::Range(from, to));
      i = next + 2;
      if chars.get(i) == Some(&('-', false))
        && chars.get(i + 1).is_some_and(|p| *p != (']', false))
      {
        return Err(ValueIssue::Unspecified);
      }
    } else {
      items.push(item);
      i = next;
    }
  }
  Ok(None)
}
