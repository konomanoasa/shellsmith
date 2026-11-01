use crate::interface::application::engine::{
  self, Document, DocumentId, Error as EngineError, Location, check_cancelled,
};
use crate::interface::application::{Application, Error};
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatEdit {
  pub location: Location,
  pub new_text: String,
}

impl Application {
  pub fn format(
    &self,
    id: DocumentId,
    cancelled: &AtomicBool,
  ) -> Result<Vec<FormatEdit>, Error> {
    check_cancelled(cancelled)?;
    let document = self
      .workspace
      .document(id)
      .ok_or(engine::Error::UnknownDocument(id))?;
    if document.has_syntax_errors() {
      return Err(Error::FormattingRefused(
        "Cannot format a document containing ERROR or missing syntax".into(),
      ));
    }
    let (arena, root) =
      Builder::new(document.source(), document.syntax(), cancelled)?.build()?;
    let output = render(&arena, root, document.source(), cancelled)?;
    let result = collect_edits(document, output, cancelled)?;
    check_cancelled(cancelled)?;
    Ok(result)
  }
}

#[derive(Clone, Copy)]
enum Gap {
  Keep,
  Line(usize),
  Space,
}

struct Builder<'a> {
  source: &'a str,
  root: Node<'a>,
  arena: Arena,
  cancelled: &'a AtomicBool,
  docs: HashMap<usize, Id>,
  trivia: Vec<Node<'a>>,
  continuations: BTreeSet<usize>,
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
  let mut cursor = node.walk();
  node.children(&mut cursor).collect()
}

fn protected(node: Node<'_>) -> bool {
  matches!(
    node.kind(),
    "here_document_sequence"
      | "here_end"
      | "comment"
      | "single_quoted"
      | "dollar_single_quoted"
      | "backquote_substitution"
      | "literal"
      | "escaped_character"
      | "double_quote_text"
  )
}

impl<'a> Builder<'a> {
  fn new(
    source: &'a str,
    root: Node<'a>,
    cancelled: &'a AtomicBool,
  ) -> Result<Self, EngineError> {
    let mut trivia = Vec::new();
    let mut continuations = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
      check_cancelled(cancelled)?;
      if matches!(
        node.kind(),
        "comment" | "here_document_line_end" | "here_document"
      ) {
        trivia.push(node);
        continue;
      }
      if matches!(
        node.kind(),
        "backquote_substitution" | "single_quoted" | "dollar_single_quoted"
      ) {
        continue;
      }
      if node.kind() == "line_continuation" {
        continuations.insert(node.start_byte());
      }
      pending.extend(children(node).into_iter().rev());
    }
    trivia.sort_by_key(Node::start_byte);
    Ok(Self {
      source,
      root,
      arena: Arena::default(),
      cancelled,
      docs: HashMap::new(),
      trivia,
      continuations,
    })
  }

  fn build(mut self) -> Result<(Arena, Id), EngineError> {
    let mut order = Vec::new();
    let mut pending = vec![self.root];
    while let Some(node) = pending.pop() {
      check_cancelled(self.cancelled)?;
      order.push(node);
      if !protected(node) {
        pending.extend(children(node));
      }
    }
    for node in order.into_iter().rev() {
      check_cancelled(self.cancelled)?;
      let id = if matches!(
        node.kind(),
        "literal"
          | "double_quote_text"
          | "variable_name"
          | "arithmetic_variable"
          | "arithmetic_number"
      ) {
        let content = self.raw(node.byte_range());
        self.arena.add(Doc::Word(content))
      } else if protected(node) {
        self.raw(node.byte_range())
      } else {
        match node.kind() {
          "program" => self.program(node),
          "parameter_expansion" => self.parameter(node),
          "simple_command" => self.command(node),
          "and_or" => self.chain(node),
          "function_definition" => self.function(node),
          "brace_group" | "subshell" => self.block(node),
          "if_clause" | "else_part" => self.conditional(node),
          "while_clause" | "until_clause" | "for_clause" => {
            self.loop_clause(node)
          }
          "case_clause" => self.case_clause(node),
          "command_substitution_body" => self.substitution_body(node),
          "term" | "list" => self.list(node),
          "complete_commands" => self.complete_commands(node),
          _ => self.generic(node),
        }
      };
      self.docs.insert(node.id(), id);
    }
    let root = self.node(self.root);
    Ok((self.arena, root))
  }

  fn node(&self, node: Node<'_>) -> Id {
    self.docs[&node.id()]
  }

  fn raw(&mut self, range: Range<usize>) -> Id {
    let mut parts = Vec::new();
    let mut cursor = range.start;
    if cursor > 0
      && self.continuations.contains(&(cursor - 1))
      && self.source.as_bytes().get(cursor) == Some(&b'\n')
    {
      cursor += 1;
    }
    for &offset in self.continuations.range(range.clone()) {
      if self.source.as_bytes().get(offset + 1) == Some(&b'\n') {
        if cursor < offset {
          parts.push(self.arena.source(cursor..offset));
        }
        cursor = (offset + 2).min(range.end);
      }
    }
    if cursor < range.end {
      parts.push(self.arena.source(cursor..range.end));
    }
    if parts.is_empty() {
      self.arena.source(range.end..range.end)
    } else if parts.len() == 1 {
      parts[0]
    } else {
      self.arena.sequence(parts)
    }
  }

  fn generic(&mut self, node: Node<'_>) -> Id {
    let mut cursor = node.start_byte();
    let mut parts = Vec::new();
    for child in children(node) {
      parts.push(self.raw(cursor..child.start_byte()));
      let content = self.node(child);
      if cursor > node.start_byte()
        && matches!(node.kind(), "word" | "assignment_word" | "double_quoted")
        && !matches!(self.arena.0[content], Doc::Word(_))
      {
        parts.push(self.arena.add(Doc::WordBoundary(content)));
      }
      parts.push(content);
      cursor = child.end_byte();
    }
    parts.push(self.raw(cursor..node.end_byte()));
    self.arena.sequence(parts)
  }

  fn safe_gap(&self, range: Range<usize>) -> bool {
    let bytes = self.source.as_bytes();
    let mut offset = range.start;
    while offset < range.end {
      match bytes[offset] {
        b' ' | b'\t' | b'\n' => offset += 1,
        b'\\'
          if self.continuations.contains(&offset)
            && bytes.get(offset + 1) == Some(&b'\n')
            && offset + 1 < range.end =>
        {
          offset += 2;
        }
        _ => return false,
      }
    }
    true
  }

  fn flat_gap(&mut self, range: Range<usize>) -> Id {
    if self.safe_gap(range.clone()) && self.source[range.clone()].contains('\n')
    {
      let text = &self.source[range.clone()];
      let end = text.find(['\\', '\n']).unwrap();
      if end == 0 {
        self.arena.text(" ")
      } else {
        self.raw(range.start..range.start + end)
      }
    } else {
      self.raw(range)
    }
  }

  fn whitespace(&mut self, range: Range<usize>, mode: Gap) -> Id {
    let text = &self.source[range.clone()];
    if range.start == 0 && text.bytes().all(|b| matches!(b, b' ' | b'\t')) {
      return self.arena.text("");
    }
    let newlines = text.bytes().filter(|&byte| byte == b'\n').count();
    if !matches!(mode, Gap::Keep) {
      let separator = text.find([';', '&']).map(|offset| range.start + offset);
      let safe = separator.map_or_else(
        || self.safe_gap(range.clone()),
        |offset| {
          self.safe_gap(range.start..offset)
            && self.safe_gap(offset + 1..range.end)
        },
      );
      if !safe {
        return self.raw(range);
      }
      let spacing = match mode {
        Gap::Line(count) => {
          let lines = text
            .bytes()
            .enumerate()
            .filter(|&(offset, byte)| {
              byte == b'\n'
                && !(range.start + offset > 0
                  && self.continuations.contains(&(range.start + offset - 1)))
            })
            .count();
          self.arena.add(Doc::Line(if count == 2 {
            2
          } else {
            count.max(lines)
          }))
        }
        Gap::Space => self.arena.text(" "),
        Gap::Keep => unreachable!(),
      };
      if let Some(offset) =
        separator.filter(|&offset| self.source.as_bytes()[offset] == b'&')
      {
        let prefix = self.flat_gap(range.start..offset);
        let marker = self.raw(offset..offset + 1);
        return self.arena.sequence(vec![prefix, marker, spacing]);
      }
      return spacing;
    }
    let clean = text.trim_matches([' ', '\t', '\n', ';']);
    let layout_only = clean.is_empty()
      || self.safe_gap(range.clone())
      || clean == "&" && newlines > 0;
    if !layout_only {
      return self.raw(range);
    }
    if newlines == 0 {
      self.raw(range)
    } else {
      let first = text.find('\n').unwrap();
      let prefix = self.raw(range.start..range.start + first);
      let line = self.arena.add(Doc::Line(newlines));
      self.arena.sequence(vec![prefix, line])
    }
  }

  fn trivia_in(
    &self,
    range: Range<usize>,
  ) -> impl Iterator<Item = Node<'a>> + '_ {
    let first = self
      .trivia
      .partition_point(|node| node.start_byte() < range.start);
    self.trivia[first..]
      .iter()
      .copied()
      .take_while(move |node| node.end_byte() <= range.end)
  }

  fn gap(&mut self, range: Range<usize>, mode: Gap) -> Id {
    let selected: Vec<_> = self.trivia_in(range.clone()).collect();
    if selected.is_empty() {
      return self.whitespace(range, mode);
    }
    let mut parts = Vec::new();
    let mut cursor = range.start;
    let mut first_standalone = true;
    let mut after_here = false;
    for item in selected {
      if matches!(item.kind(), "here_document_line_end" | "here_document") {
        // The introducer's newline, stripped tabs, body and terminator stay together.
        let prefix = &self.source[cursor..item.start_byte()];
        if !prefix.trim_matches([' ', '\t', ';']).is_empty() {
          parts.push(self.raw(cursor..item.start_byte()));
        }
        parts.push(self.raw(item.byte_range()));
        after_here = self.source[item.byte_range()].ends_with('\n');
      } else {
        let line_start = self.source[..item.start_byte()]
          .rfind('\n')
          .map_or(0, |p| p + 1);
        let standalone = self.source[line_start..item.start_byte()]
          .bytes()
          .all(|b| matches!(b, b' ' | b'\t'));
        let before = if standalone {
          if first_standalone { mode } else { Gap::Keep }
        } else if matches!(mode, Gap::Line(_)) {
          Gap::Space
        } else {
          Gap::Keep
        };
        if after_here
          && self.source[cursor..item.start_byte()]
            .bytes()
            .all(|b| matches!(b, b' ' | b'\t'))
        {
          parts.push(self.arena.add(Doc::Indent));
        } else {
          parts.push(self.whitespace(cursor..item.start_byte(), before));
        }
        parts.push(self.raw(item.byte_range()));
        if standalone {
          first_standalone = false;
        }
        after_here = false;
      }
      cursor = item.end_byte();
    }
    if after_here
      && self.source[cursor..range.end]
        .bytes()
        .all(|b| matches!(b, b' ' | b'\t'))
    {
      parts.push(self.arena.add(Doc::Indent));
    } else {
      let tail = match mode {
        Gap::Line(n) if first_standalone => Gap::Line(n),
        Gap::Line(_) | Gap::Space => Gap::Line(1),
        Gap::Keep => Gap::Keep,
      };
      parts.push(self.whitespace(cursor..range.end, tail));
    }
    self.arena.sequence(parts)
  }

  fn program(&mut self, node: Node<'a>) -> Id {
    let mut tail = self.source.len()
      - self
        .source
        .bytes()
        .rev()
        .take_while(|&b| b == b'\n')
        .count();
    if let Some(protected) = self
      .trivia
      .iter()
      .find(|item| item.start_byte() < tail && item.end_byte() > tail)
    {
      tail = protected.end_byte();
    }
    if let Some(commands) = node.child_by_field_name("commands") {
      let leading = self.gap(0..commands.start_byte(), Gap::Keep);
      let tail = tail.max(commands.end_byte());
      let trailing = self.gap(commands.end_byte()..tail, Gap::Keep);
      let ending = self.raw(tail..self.source.len());
      self
        .arena
        .sequence(vec![leading, self.node(commands), trailing, ending])
    } else if !self.trivia.is_empty() {
      let content = self.gap(0..tail, Gap::Keep);
      let ending = self.raw(tail..self.source.len());
      self.arena.sequence(vec![content, ending])
    } else {
      self.raw(0..self.source.len())
    }
  }

  fn statements(node: Node<'a>) -> Vec<Node<'a>> {
    let mut result = Vec::new();
    let mut pending = vec![node];
    while let Some(item) = pending.pop() {
      if item.kind() == "and_or" {
        result.push(item);
      } else if matches!(
        item.kind(),
        "compound_list"
          | "term"
          | "list"
          | "complete_command"
          | "complete_commands"
      ) {
        pending.extend(children(item).into_iter().rev());
      }
    }
    result
  }

  fn is_block(node: Node<'_>) -> bool {
    let mut pending = vec![node];
    while let Some(item) = pending.pop() {
      if matches!(item.kind(), "function_definition" | "compound_command") {
        return true;
      }
      if matches!(
        item.kind(),
        "and_or" | "pipeline" | "pipe_sequence" | "command"
      ) {
        pending.extend(children(item));
      }
    }
    false
  }

  fn statements_doc(&mut self, nodes: &[Node<'a>]) -> Id {
    let mut parts = Vec::new();
    for (index, &node) in nodes.iter().enumerate() {
      if let Some(previous) = index.checked_sub(1).map(|i| nodes[i]) {
        let mode = if Self::is_block(previous) && Self::is_block(node) {
          Gap::Line(2)
        } else {
          Gap::Keep
        };
        parts.push(self.gap(previous.end_byte()..node.start_byte(), mode));
      }
      parts.push(self.node(node));
    }
    self.arena.sequence(parts)
  }

  fn list(&mut self, node: Node<'a>) -> Id {
    let nodes = Self::statements(node);
    self.statements_doc(&nodes)
  }

  fn complete_commands(&mut self, node: Node<'a>) -> Id {
    let nodes = Self::statements(node);
    let content = self.statements_doc(&nodes);
    let tail = self.raw(
      nodes.last().map_or(node.start_byte(), Node::end_byte)..node.end_byte(),
    );
    self.arena.sequence(vec![content, tail])
  }

  fn parameter(&mut self, node: Node<'a>) -> Id {
    for ancestor in std::iter::successors(node.parent(), Node::parent) {
      match ancestor.kind() {
        "arithmetic_expansion" => return self.generic(node),
        "command_substitution" | "simple_command" => break,
        _ => {}
      }
    }
    let Some(parameter) = node.child_by_field_name("parameter") else {
      return self.generic(node);
    };
    let braced = children(node).iter().any(|child| child.kind() == "{");
    if !braced && parameter.kind() == "variable_name" {
      let before = self.raw(node.start_byte()..parameter.start_byte());
      let open = self.arena.text("{");
      let content = self.node(parameter);
      let close = self.arena.text("}");
      let after = self.raw(parameter.end_byte()..node.end_byte());
      return self
        .arena
        .sequence(vec![before, open, content, close, after]);
    }
    let mut following = node.end_byte();
    while self.continuations.contains(&following)
      && self.source.as_bytes().get(following + 1) == Some(&b'\n')
    {
      following += 2;
    }
    let adjacent = self.source[following..]
      .chars()
      .next()
      .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    if braced
      && node.child_by_field_name("operator").is_none()
      && matches!(
        parameter.kind(),
        "positional_parameter" | "special_parameter"
      )
      && parameter.end_byte() - parameter.start_byte() == 1
      && !adjacent
    {
      let mut cursor = node.start_byte();
      let mut parts = Vec::new();
      for child in children(node) {
        parts.push(self.raw(cursor..child.start_byte()));
        if !matches!(child.kind(), "{" | "}") {
          parts.push(self.node(child));
        }
        cursor = child.end_byte();
      }
      parts.push(self.raw(cursor..node.end_byte()));
      return self.arena.sequence(parts);
    }
    self.generic(node)
  }

  fn command(&mut self, node: Node<'a>) -> Id {
    let mut atoms = Vec::new();
    let mut pending = vec![node];
    while let Some(item) = pending.pop() {
      if matches!(item.kind(), "word" | "assignment_word" | "io_redirect") {
        atoms.push(item);
      } else if matches!(
        item.kind(),
        "simple_command"
          | "cmd_prefix"
          | "cmd_suffix"
          | "cmd_name"
          | "cmd_word"
      ) {
        pending.extend(children(item).into_iter().rev());
      }
    }
    if atoms.is_empty() {
      return self.generic(node);
    }
    let command = node
      .child_by_field_name("name")
      .or_else(|| node.child_by_field_name("word"));
    let name = command.and_then(|command| {
      atoms.iter().position(|atom| {
        atom.start_byte() >= command.start_byte()
          && atom.end_byte() <= command.end_byte()
      })
    });
    let mut gaps = Vec::new();
    for pair in atoms.windows(2) {
      let range = pair[0].end_byte()..pair[1].start_byte();
      if !self.safe_gap(range.clone()) {
        return self.generic(node);
      }
      gaps.push(self.flat_gap(range));
    }
    let atom_ids = atoms.iter().map(|&atom| self.node(atom)).collect();
    let content = self.arena.add(Doc::Command {
      atoms: atom_ids,
      gaps,
      name,
    });
    let before = self.raw(node.start_byte()..atoms[0].start_byte());
    let after = self.raw(atoms.last().unwrap().end_byte()..node.end_byte());
    self.arena.sequence(vec![before, content, after])
  }

  fn substitution_body(&mut self, node: Node<'a>) -> Id {
    let commands = children(node)
      .into_iter()
      .find(|child| child.kind() == "complete_commands");
    let Some(commands) = commands else {
      return self.raw(node.byte_range());
    };
    let before = node.start_byte()..commands.start_byte();
    let after = commands.end_byte()..node.end_byte();
    let content = self.node(commands);
    if !self.safe_gap(before.clone()) || !self.safe_gap(after.clone()) {
      let leading = self.gap(before, Gap::Keep);
      let body = self.arena.sequence(vec![leading, content]);
      let body = self.arena.nest(2, body);
      let trailing = self.gap(after, Gap::Keep);
      return self.arena.sequence(vec![body, trailing]);
    }
    let leading = self.substitution_spacing(before);
    let leading = if matches!(&self.arena.0[leading], Doc::Source(range) if range.is_empty())
      && self.source[commands.start_byte()..].starts_with('(')
    {
      self.arena.text(" ")
    } else {
      leading
    };
    let trailing = self.substitution_spacing(after);
    let flat_body = self.arena.sequence(vec![leading, content]);
    let flat_body = self.arena.nest(2, flat_body);
    let flat = self.arena.sequence(vec![flat_body, trailing]);
    let line = self.arena.add(Doc::Line(1));
    let inside = self.arena.sequence(vec![leading, line, content]);
    let inside = self.arena.nest(2, inside);
    let close = self.arena.add(Doc::Line(1));
    let broken = self.arena.sequence(vec![inside, trailing, close]);
    self.arena.add(Doc::NaturalChoice { flat, broken })
  }
  fn substitution_spacing(&mut self, range: Range<usize>) -> Id {
    let end = self.source[range.clone()]
      .find(['\\', '\n'])
      .map_or(range.end, |offset| range.start + offset);
    self.raw(range.start..end)
  }
}

fn keyword<'a>(node: Node<'a>, kind: &str) -> Node<'a> {
  children(node)
    .into_iter()
    .find(|child| child.kind() == kind)
    .expect("valid keyword")
}

impl<'a> Builder<'a> {
  fn closing_gap(&mut self, range: Range<usize>, mode: Gap) -> Id {
    let gap = self.gap(range, mode);
    let gap = self.arena.nest(2, gap);
    let align = self.arena.add(Doc::Indent);
    self.arena.sequence(vec![gap, align])
  }

  fn function(&mut self, node: Node<'a>) -> Id {
    let name = node
      .child_by_field_name("name")
      .expect("valid function name");
    let body = node
      .child_by_field_name("body")
      .expect("valid function body");
    let open = keyword(node, "(");
    let close = keyword(node, ")");
    let mut parts = vec![self.raw(node.start_byte()..name.end_byte())];
    if !self.safe_gap(name.end_byte()..open.start_byte()) {
      parts.push(self.raw(name.end_byte()..open.start_byte()));
    }
    parts.push(self.raw(open.start_byte()..close.end_byte()));
    let brace = children(body)
      .into_iter()
      .find(|child| child.kind() == "compound_command")
      .and_then(|compound| {
        children(compound)
          .into_iter()
          .find(|child| child.kind() == "brace_group")
      });
    if brace.is_some() && self.safe_gap(close.end_byte()..body.start_byte()) {
      parts.push(self.arena.text(" "));
    } else {
      parts.push(self.gap(close.end_byte()..body.start_byte(), Gap::Keep));
    }
    parts.push(self.node(body));
    parts.push(self.raw(body.end_byte()..node.end_byte()));
    self.arena.sequence(parts)
  }

  fn block(&mut self, node: Node<'a>) -> Id {
    let body = node.child_by_field_name("body").expect("valid block body");
    let statements = Self::statements(body);
    let Some(first) = statements.first() else {
      return self.generic(node);
    };
    let last = statements.last().unwrap();
    let prefix = self.raw(node.start_byte()..body.start_byte());
    let leading = self.gap(body.start_byte()..first.start_byte(), Gap::Keep);
    let content = self.statements_doc(&statements);
    let inside = self.arena.sequence(vec![leading, content]);
    let inside = self.arena.nest(2, inside);
    let trailing =
      self.closing_gap(last.end_byte()..body.end_byte(), Gap::Keep);
    let suffix = self.raw(body.end_byte()..node.end_byte());
    self.arena.sequence(vec![prefix, inside, trailing, suffix])
  }

  fn inline_comment(&self, start: usize, end: usize) -> Option<Node<'a>> {
    let line_end = self.source[start..end]
      .find('\n')
      .map_or(end, |offset| start + offset);
    self
      .trivia_in(start..line_end)
      .find(|item| item.kind() == "comment")
  }

  fn branch(
    &mut self,
    start: Node<'a>,
    condition: Node<'a>,
    delimiter: Node<'a>,
    body: Node<'a>,
  ) -> Id {
    let conditions = Self::statements(condition);
    let statements = Self::statements(body);
    let first = conditions.first().expect("valid condition");
    let last = conditions.last().unwrap();
    let body_first = statements.first().expect("valid branch");
    let prefix = self.raw(start.byte_range());
    let leading = self.gap(start.end_byte()..first.start_byte(), Gap::Keep);
    let content = self.statements_doc(&conditions);
    let content = self.arena.sequence(vec![leading, content]);
    let content = self.arena.nest(2, content);
    let mut parts = vec![prefix, content];
    let delimiter_comment =
      self.inline_comment(delimiter.end_byte(), body_first.start_byte());
    let earlier_comment = self
      .trivia_in(last.end_byte()..delimiter.start_byte())
      .any(|item| item.kind() == "comment");
    let mut body_start = delimiter.end_byte();
    if delimiter_comment.is_some() && earlier_comment {
      parts.push(
        self.closing_gap(last.end_byte()..delimiter.start_byte(), Gap::Line(1)),
      );
      parts.push(self.raw(delimiter.byte_range()));
    } else {
      let separator = condition
        .child_by_field_name("terminator")
        .and_then(|separator| separator.child_by_field_name("operator"));
      let remaining = separator.map_or(last.end_byte(), |node| node.end_byte());
      if separator.is_some_and(|separator| {
        children(separator).iter().any(|child| child.kind() == "&")
      }) {
        parts.push(self.arena.text(" & "));
      } else {
        parts.push(self.arena.text("; "));
      }
      parts.push(self.raw(delimiter.byte_range()));
      if let Some(comment) = delimiter_comment {
        parts.push(self.raw(delimiter.end_byte()..comment.end_byte()));
        body_start = comment.end_byte();
      }
      let after = self.gap(remaining..delimiter.start_byte(), Gap::Line(1));
      parts.push(self.arena.nest(2, after));
    }
    let leading = self.gap(body_start..body_first.start_byte(), Gap::Line(1));
    let body_content = self.statements_doc(&statements);
    let body_content = self.arena.sequence(vec![leading, body_content]);
    parts.push(self.arena.nest(2, body_content));
    self.arena.sequence(parts)
  }

  fn conditional(&mut self, node: Node<'a>) -> Id {
    let body = node
      .child_by_field_name("consequence")
      .or_else(|| node.child_by_field_name("body"))
      .expect("valid conditional body");
    let statements = Self::statements(body);
    let last = statements.last().expect("valid conditional commands");
    let content = if let Some(condition) = node.child_by_field_name("condition")
    {
      let start = keyword(
        node,
        if node.kind() == "if_clause" {
          "if_keyword"
        } else {
          "elif_keyword"
        },
      );
      self.branch(start, condition, keyword(node, "then_keyword"), body)
    } else {
      let start = keyword(node, "else_keyword");
      let header = self.raw(start.byte_range());
      let leading =
        self.gap(start.end_byte()..statements[0].start_byte(), Gap::Line(1));
      let content = self.statements_doc(&statements);
      let inside = self.arena.sequence(vec![leading, content]);
      let inside = self.arena.nest(2, inside);
      self.arena.sequence(vec![header, inside])
    };
    let mut parts = vec![content];
    if let Some(alternative) = node.child_by_field_name("alternative") {
      parts.push(
        self
          .closing_gap(last.end_byte()..alternative.start_byte(), Gap::Line(1)),
      );
      parts.push(self.node(alternative));
      if node.kind() == "if_clause" {
        let end = keyword(node, "fi_keyword");
        parts.push(
          self.gap(alternative.end_byte()..end.start_byte(), Gap::Line(1)),
        );
        parts.push(self.raw(end.byte_range()));
      }
    } else if node.kind() == "if_clause" {
      let end = keyword(node, "fi_keyword");
      parts.push(
        self.closing_gap(last.end_byte()..end.start_byte(), Gap::Line(1)),
      );
      parts.push(self.raw(end.byte_range()));
    } else {
      parts
        .push(self.closing_gap(last.end_byte()..node.end_byte(), Gap::Line(1)));
    }
    self.arena.sequence(parts)
  }

  fn loop_clause(&mut self, node: Node<'a>) -> Id {
    let group = node.child_by_field_name("body").expect("valid loop group");
    let body = group.child_by_field_name("body").expect("valid loop body");
    let delimiter = keyword(group, "do_keyword");
    let end = keyword(group, "done_keyword");
    let statements = Self::statements(body);
    let last = statements.last().expect("valid loop commands");
    let content = if let Some(condition) = node.child_by_field_name("condition")
    {
      let start = keyword(
        node,
        if node.kind() == "while_clause" {
          "while_keyword"
        } else {
          "until_keyword"
        },
      );
      self.branch(start, condition, delimiter, body)
    } else {
      let header_end = node
        .child_by_field_name("words")
        .or_else(|| node.child_by_field_name("in"))
        .or_else(|| node.child_by_field_name("name"))
        .expect("valid for header")
        .end_byte();
      let mut parts = Vec::new();
      let mut cursor = node.start_byte();
      if let Some(words) = node.child_by_field_name("words") {
        let in_word = node.child_by_field_name("in").expect("word list has in");
        let words: Vec<_> = children(words)
          .into_iter()
          .filter(|word| word.kind() == "word")
          .collect();
        let mut atoms = vec![self.raw(node.start_byte()..in_word.end_byte())];
        let mut gaps = Vec::new();
        cursor = in_word.end_byte();
        for word in words {
          gaps.push(self.flat_gap(cursor..word.start_byte()));
          atoms.push(self.node(word));
          cursor = word.end_byte();
        }
        parts.push(self.arena.add(Doc::Command {
          atoms,
          gaps,
          name: Some(0),
        }));
      } else {
        for child in children(node)
          .into_iter()
          .filter(|child| child.end_byte() <= header_end)
        {
          parts.push(self.raw(cursor..child.start_byte()));
          parts.push(self.node(child));
          cursor = child.end_byte();
        }
      }
      parts.push(self.raw(cursor..header_end));
      let comment =
        self.inline_comment(delimiter.end_byte(), statements[0].start_byte());
      let earlier = self
        .trivia_in(header_end..delimiter.start_byte())
        .any(|item| item.kind() == "comment");
      let mut body_start = delimiter.end_byte();
      if earlier && comment.is_some() {
        parts.push(self.gap(header_end..delimiter.start_byte(), Gap::Line(1)));
        parts.push(self.raw(delimiter.byte_range()));
      } else {
        parts.push(self.arena.text("; "));
        parts.push(self.raw(delimiter.byte_range()));
        if let Some(comment) = comment {
          parts.push(self.raw(delimiter.end_byte()..comment.end_byte()));
          body_start = comment.end_byte();
        }
        let after = self.gap(header_end..delimiter.start_byte(), Gap::Line(1));
        parts.push(self.arena.nest(2, after));
      }
      let leading =
        self.gap(body_start..statements[0].start_byte(), Gap::Line(1));
      let body = self.statements_doc(&statements);
      let inside = self.arena.sequence(vec![leading, body]);
      parts.push(self.arena.nest(2, inside));
      self.arena.sequence(parts)
    };
    let trailing =
      self.closing_gap(last.end_byte()..end.start_byte(), Gap::Line(1));
    let end = self.raw(end.byte_range());
    self.arena.sequence(vec![content, trailing, end])
  }

  fn case_item_end(node: Node<'a>) -> usize {
    node.child_by_field_name("terminator").map_or_else(
      || {
        node.child_by_field_name("body").map_or_else(
          || keyword(node, ")").end_byte(),
          |body| {
            Self::statements(body)
              .last()
              .map_or(body.end_byte(), Node::end_byte)
          },
        )
      },
      |terminator| terminator.end_byte(),
    )
  }

  fn case_item(&mut self, node: Node<'a>) -> Id {
    let patterns = node
      .child_by_field_name("patterns")
      .expect("valid case pattern");
    let close = keyword(node, ")");
    let mut pattern_parts = Vec::new();
    let mut cursor = patterns.start_byte();
    for child in children(patterns) {
      pattern_parts.push(self.raw(cursor..child.start_byte()));
      if child.kind() != "(" {
        pattern_parts.push(self.node(child));
      }
      cursor = child.end_byte();
    }
    pattern_parts.push(self.raw(cursor..close.end_byte()));
    let pattern = self.arena.sequence(pattern_parts);
    let Some(body) = node.child_by_field_name("body") else {
      if let Some(terminator) = node.child_by_field_name("terminator") {
        let range = close.end_byte()..terminator.start_byte();
        let gap = if self.source[range.clone()].contains('\n') {
          self.closing_gap(range, Gap::Line(1))
        } else {
          self.gap(range, Gap::Space)
        };
        let end = self.raw(terminator.byte_range());
        return self.arena.sequence(vec![pattern, gap, end]);
      }
      return pattern;
    };
    let statements = Self::statements(body);
    if statements.is_empty() {
      return self.generic(node);
    }
    let first = statements[0];
    let last = *statements.last().unwrap();
    let terminator = node.child_by_field_name("terminator");
    let end = terminator.map_or(body.end_byte(), |item| item.start_byte());
    let content = self.statements_doc(&statements);
    let before = self.gap(close.end_byte()..first.start_byte(), Gap::Line(1));
    let inside = self.arena.sequence(vec![before, content]);
    let inside = self.arena.nest(2, inside);
    let mut broken = vec![pattern, inside];
    if let Some(terminator) = terminator {
      broken.push(self.closing_gap(last.end_byte()..end, Gap::Line(1)));
      broken.push(self.raw(terminator.byte_range()));
    }
    let broken = self.arena.sequence(broken);
    let mut commands = 0;
    let mut pending = vec![first];
    while let Some(item) = pending.pop() {
      if item.kind() == "command" {
        commands += 1;
      } else if matches!(item.kind(), "and_or" | "pipeline" | "pipe_sequence") {
        pending.extend(children(item));
      }
    }
    let multiple =
      statements.len() > 1 || commands > 1 || Self::is_block(first);
    if multiple
      || self.source[close.end_byte()..Self::case_item_end(node)].contains('\n')
    {
      return broken;
    }
    let before = self.arena.text(" ");
    let mut flat = vec![pattern, before, content];
    if let Some(terminator) = terminator {
      flat.push(self.gap(last.end_byte()..end, Gap::Space));
      flat.push(self.raw(terminator.byte_range()));
    }
    let flat = self.arena.sequence(flat);
    self.arena.add(Doc::NaturalChoice { flat, broken })
  }

  fn case_clause(&mut self, node: Node<'a>) -> Id {
    let items = node.child_by_field_name("items");
    let mut branches = Vec::new();
    let mut pending: Vec<_> = items.into_iter().collect();
    while let Some(item) = pending.pop() {
      if matches!(item.kind(), "case_item" | "case_item_ns") {
        branches.push(item);
      } else {
        pending.extend(children(item).into_iter().rev());
      }
    }
    let in_word = node.child_by_field_name("in").expect("valid case in");
    let end = keyword(node, "esac_keyword");
    let word = node.child_by_field_name("word").expect("valid case word");
    let before = self.raw(node.start_byte()..word.start_byte());
    let after = self.raw(word.end_byte()..in_word.end_byte());
    let mut parts = vec![before, self.node(word), after];
    let mut cursor = in_word.end_byte();
    let mut trailing_indent = 2;
    for branch in branches {
      let leading = self.gap(cursor..branch.start_byte(), Gap::Line(1));
      let content = self.case_item(branch);
      let item = self.arena.sequence(vec![leading, content]);
      parts.push(self.arena.nest(2, item));
      cursor = Self::case_item_end(branch);
      trailing_indent = if branch.child_by_field_name("terminator").is_some() {
        2
      } else {
        4
      };
    }
    let trailing = self.gap(
      cursor..end.start_byte(),
      if items.is_some() {
        Gap::Line(1)
      } else {
        Gap::Keep
      },
    );
    parts.push(self.arena.nest(trailing_indent, trailing));
    parts.push(self.arena.add(Doc::Indent));
    parts.push(self.raw(end.byte_range()));
    self.arena.sequence(parts)
  }
}

impl<'a> Builder<'a> {
  fn chain(&mut self, node: Node<'a>) -> Id {
    let mut commands = Vec::new();
    let mut operators = Vec::new();
    let mut pending = vec![node];
    while let Some(item) = pending.pop() {
      if item.kind() == "command" {
        commands.push(item);
      } else if matches!(item.kind(), "|" | "and_if" | "or_if") {
        operators.push(item);
      } else if matches!(item.kind(), "and_or" | "pipeline" | "pipe_sequence") {
        pending.extend(children(item).into_iter().rev());
      }
    }
    if operators.is_empty() {
      return self.generic(node);
    }
    let starts: Vec<_> = commands.iter().copied().map(stage_start).collect();
    let stages: Vec<_> = commands
      .iter()
      .zip(&starts)
      .map(|(&command, &start)| {
        let prefix = self.raw(start..command.start_byte());
        self.arena.sequence(vec![prefix, self.node(command)])
      })
      .collect();
    let gaps: Vec<_> = operators
      .iter()
      .enumerate()
      .flat_map(|(index, op)| {
        [
          commands[index].end_byte()..op.start_byte(),
          op.end_byte()..starts[index + 1],
        ]
      })
      .collect();
    let comments: Vec<_> = gaps
      .iter()
      .flat_map(|gap| self.trivia_in(gap.clone()))
      .collect();
    if !self.can_move_chain_comments(node, &gaps, &comments) {
      return self
        .preserved_chain(node, &commands, &operators, &starts, &stages);
    }
    let mut flat = vec![self.raw(node.start_byte()..starts[0]), stages[0]];
    let mut broken = flat.clone();
    for (index, &operator) in operators.iter().enumerate() {
      let left = commands[index].end_byte()..operator.start_byte();
      let right = operator.end_byte()..starts[index + 1];
      let left = self.chain_gap(left);
      let right = self.chain_gap(right);
      flat.push(left);
      flat.push(self.raw(operator.byte_range()));
      flat.push(right);
      flat.push(stages[index + 1]);
      let slash = self.arena.text("\\");
      if matches!(&self.arena.0[left], Doc::Source(range) if range.is_empty()) {
        broken.push(self.arena.text(" "));
      } else {
        broken.push(left);
      }
      broken.push(slash);
      let line = self.arena.add(Doc::Line(1));
      let operator = self.raw(operator.byte_range());
      let space = if matches!(&self.arena.0[right], Doc::Source(range) if range.is_empty())
      {
        self.arena.text(" ")
      } else {
        right
      };
      let stage =
        self
          .arena
          .sequence(vec![line, operator, space, stages[index + 1]]);
      broken.push(self.arena.nest(2, stage));
    }
    let tail = self.raw(commands.last().unwrap().end_byte()..node.end_byte());
    flat.push(tail);
    broken.push(tail);
    let flat = self.arena.sequence(flat);
    let broken = self.arena.sequence(broken);
    let content = self.arena.add(Doc::Choice { flat, broken });
    if comments.is_empty() {
      return content;
    }
    let mut parts = vec![self.arena.add(Doc::LineStart)];
    for comment in comments {
      parts.push(self.raw(comment.byte_range()));
      parts.push(self.arena.add(Doc::Line(1)));
    }
    parts.push(content);
    self.arena.sequence(parts)
  }

  fn chain_gap(&mut self, range: Range<usize>) -> Id {
    if self.trivia_in(range.clone()).next().is_some() {
      self.arena.text(" ")
    } else {
      self.flat_gap(range)
    }
  }

  fn can_move_chain_comments(
    &self,
    node: Node<'a>,
    gaps: &[Range<usize>],
    comments: &[Node<'a>],
  ) -> bool {
    for gap in gaps {
      let mut cursor = gap.start;
      for comment in self.trivia_in(gap.clone()) {
        if comment.kind() != "comment"
          || !self.safe_gap(cursor..comment.start_byte())
        {
          return false;
        }
        cursor = comment.end_byte();
      }
      if !self.safe_gap(cursor..gap.end) {
        return false;
      }
    }
    let Some(last) = comments.last() else {
      return true;
    };
    // Moving a comment must not cross another comment or detach a source annotation.
    if !self
      .trivia_in(node.start_byte()..last.end_byte())
      .map(|item| item.id())
      .eq(comments.iter().map(Node::id))
    {
      return false;
    }
    if comments.iter().any(|comment| {
      let text = &self.source[comment.byte_range()];
      text.starts_with("# shellsmith source=") || text.starts_with("#!")
    }) {
      return false;
    }
    let before = &self.source[..node.start_byte()];
    let mut lines = before.rsplitn(3, '\n');
    let prefix = lines.next().unwrap_or_default();
    !(prefix.trim().is_empty()
      && lines.next().is_some_and(|line| {
        line.trim_start().starts_with("# shellsmith source=")
      }))
  }

  fn preserved_chain(
    &mut self,
    node: Node<'a>,
    commands: &[Node<'a>],
    operators: &[Node<'a>],
    starts: &[usize],
    stages: &[Id],
  ) -> Id {
    let mut parts = vec![self.raw(node.start_byte()..starts[0]), stages[0]];
    for (index, &operator) in operators.iter().enumerate() {
      let before =
        self.gap(commands[index].end_byte()..operator.start_byte(), Gap::Keep);
      let token = self.raw(operator.byte_range());
      let after = self.gap(operator.end_byte()..starts[index + 1], Gap::Keep);
      let stage =
        self
          .arena
          .sequence(vec![before, token, after, stages[index + 1]]);
      parts.push(self.arena.nest(2, stage));
    }
    parts.push(self.raw(commands.last().unwrap().end_byte()..node.end_byte()));
    self.arena.sequence(parts)
  }
}

fn stage_start(command: Node<'_>) -> usize {
  command
    .parent()
    .and_then(|sequence| sequence.parent())
    .filter(|pipeline| pipeline.kind() == "pipeline")
    .and_then(|pipeline| pipeline.child_by_field_name("negation"))
    .filter(|negation| {
      negation.end_byte() <= command.start_byte()
        && command.parent().is_some_and(|sequence| {
          sequence
            .child_by_field_name("command")
            .is_some_and(|first| first.id() == command.id())
        })
    })
    .map_or(command.start_byte(), |negation| negation.start_byte())
}

type Id = usize;

enum Doc {
  Word(Id),
  WordBoundary(Id),
  NaturalChoice {
    flat: Id,
    broken: Id,
  },
  Source(Range<usize>),
  Text(String),
  Sequence(Vec<Id>),
  Line(usize),
  LineStart,
  Indent,
  Nest(usize, Id),
  Choice {
    flat: Id,
    broken: Id,
  },
  Command {
    atoms: Vec<Id>,
    gaps: Vec<Id>,
    name: Option<usize>,
  },
}

#[derive(Default)]
struct Arena(Vec<Doc>);

impl Arena {
  fn add(&mut self, doc: Doc) -> Id {
    let id = self.0.len();
    self.0.push(doc);
    id
  }

  fn source(&mut self, range: Range<usize>) -> Id {
    self.add(Doc::Source(range))
  }

  fn text(&mut self, text: impl Into<String>) -> Id {
    self.add(Doc::Text(text.into()))
  }

  fn sequence(&mut self, items: Vec<Id>) -> Id {
    self.add(Doc::Sequence(items))
  }

  fn nest(&mut self, amount: usize, item: Id) -> Id {
    self.add(Doc::Nest(amount, item))
  }
}

#[derive(Clone)]
enum Piece {
  Source(Range<usize>),
  Text(String),
}

#[derive(Clone, Default)]
struct Output {
  pieces: Vec<Piece>,
  text: String,
  line: usize,
  indent: usize,
  pending_lines: usize,
  pending_indent: Option<usize>,
}

impl Output {
  fn append(&mut self, text: &str) {
    if let Some(offset) = text.rfind('\n') {
      self.line = self.text.len() + offset + 1;
    }
    self.text.push_str(text);
  }

  fn source(&mut self, range: Range<usize>, source: &str) {
    if range.is_empty() {
      return;
    }
    self.flush(&source[range.clone()]);
    self.append(&source[range.clone()]);
    if let Some(Piece::Source(previous)) = self.pieces.last_mut()
      && previous.end == range.start
    {
      previous.end = range.end;
    } else {
      self.pieces.push(Piece::Source(range));
    }
  }

  fn text(&mut self, text: &str) {
    if text.is_empty() {
      return;
    }
    self.flush(text);
    self.generated(text);
  }

  fn generated(&mut self, text: &str) {
    self.append(text);
    if let Some(Piece::Text(previous)) = self.pieces.last_mut() {
      previous.push_str(text);
    } else if !text.is_empty() {
      self.pieces.push(Piece::Text(text.into()));
    }
  }

  fn indentation(&mut self, indent: usize) {
    if self.pending_lines > 0 || self.text.ends_with('\n') {
      self.pending_indent = Some(indent);
    }
  }

  fn newline(&mut self, count: usize, indent: usize) {
    let existing = self.text.bytes().rev().take_while(|&b| b == b'\n').count();
    self.pending_lines = self.pending_lines.max(count.saturating_sub(existing));
    self.pending_indent = Some(indent);
  }

  fn flush(&mut self, following: &str) {
    let Some(indent) = self.pending_indent.take() else {
      return;
    };
    let existing = following.bytes().take_while(|&b| b == b'\n').count();
    let lines =
      std::mem::take(&mut self.pending_lines).saturating_sub(existing);
    self.generated(&"\n".repeat(lines));
    if existing == 0 {
      self.generated(&" ".repeat(indent));
    }
    self.indent = indent;
  }

  fn current_indent(&self) -> usize {
    self.pending_indent.unwrap_or(self.indent)
  }

  fn line(&self) -> String {
    if self.pending_lines > 0
      || self.pending_indent.is_some() && self.text.ends_with('\n')
    {
      " ".repeat(self.current_indent())
    } else {
      self.text[self.line..].to_owned()
    }
  }
}

#[derive(Clone, Copy)]
enum Frame {
  End,
  Node(Id, usize),
  Gap(CommandGap),
}

#[derive(Clone, Copy)]
struct CommandGap {
  gap: Id,
  atom: Id,
  last: bool,
  forced: bool,
  indent: usize,
}

fn expand_command(
  atoms: &[Id],
  gaps: &[Id],
  indent: usize,
  stack: &mut Vec<Frame>,
) {
  for index in (0..atoms.len()).rev() {
    stack.push(Frame::Node(atoms[index], indent));
    if index > 0 {
      stack.push(Frame::Node(gaps[index - 1], indent));
    }
  }
}

fn preview(
  arena: &Arena,
  source: &str,
  frames: impl IntoIterator<Item = Frame>,
  first_line: bool,
  cancelled: &AtomicBool,
) -> Result<String, EngineError> {
  let mut output = Output::default();
  let mut frames = frames.into_iter();
  let mut stack = Vec::new();
  while let Some(frame) = stack.pop().or_else(|| frames.next()) {
    check_cancelled(cancelled)?;
    if first_line && let Frame::Gap(gap) = frame {
      let separator = preview(
        arena,
        source,
        [Frame::Node(gap.gap, gap.indent)],
        false,
        cancelled,
      )?;
      output.text(if separator.is_empty() {
        " "
      } else {
        &separator
      });
      output.text("\\");
      break;
    }
    let (id, indent) = match frame {
      Frame::End => continue,
      Frame::Node(id, indent)
      | Frame::Gap(CommandGap {
        gap: id, indent, ..
      }) => (id, indent),
    };
    match &arena.0[id] {
      Doc::Source(range) => {
        let value = &source[range.clone()];
        if first_line && let Some(end) = value.find('\n') {
          output.text(&value[..end]);
          break;
        }
        output.text(value);
      }
      Doc::Text(value) => {
        if first_line && let Some(end) = value.find('\n') {
          output.text(&value[..end]);
          break;
        }
        output.text(value);
      }
      Doc::Sequence(items) => {
        stack.extend(items.iter().rev().map(|&id| Frame::Node(id, indent)));
      }
      Doc::Line(count) => {
        if first_line {
          break;
        }
        output.newline(*count, indent);
      }
      Doc::LineStart => {
        if first_line {
          break;
        }
        let count = usize::from(!output.line().trim().is_empty());
        output.newline(count, indent);
      }
      Doc::Indent => output.indentation(indent),
      Doc::Nest(extra, item) => stack.push(Frame::Node(*item, indent + extra)),
      Doc::WordBoundary(_) => {}
      Doc::Word(content) => stack.push(Frame::Node(*content, indent)),
      Doc::NaturalChoice { .. } if first_line => break,
      Doc::Choice { flat, .. } | Doc::NaturalChoice { flat, .. } => {
        stack.push(Frame::Node(*flat, indent));
      }
      Doc::Command { atoms, gaps, .. } => {
        expand_command(atoms, gaps, indent, &mut stack);
      }
    }
  }
  Ok(output.text)
}

fn fits(
  arena: &Arena,
  source: &str,
  output: &Output,
  item: Id,
  indent: usize,
  rest: &[Frame],
  cancelled: &AtomicBool,
) -> Result<bool, EngineError> {
  let mut text = output.line();
  text.push_str(&preview(
    arena,
    source,
    vec![Frame::Node(item, indent)],
    false,
    cancelled,
  )?);
  text.push_str(&preview(
    arena,
    source,
    rest.iter().rev().copied(),
    true,
    cancelled,
  )?);
  fits_width(&text, cancelled)
}

impl CommandGap {
  fn render(
    self,
    arena: &Arena,
    source: &str,
    output: &mut Output,
    rest: &[Frame],
    cancelled: &AtomicBool,
  ) -> Result<Option<Id>, EngineError> {
    let separator = preview(
      arena,
      source,
      [Frame::Node(self.gap, self.indent)],
      false,
      cancelled,
    )?;
    let mut text = output.line();
    text.push_str(&separator);
    text.push_str(&preview(
      arena,
      source,
      [Frame::Node(self.atom, self.indent)],
      false,
      cancelled,
    )?);
    if self.last {
      // The next stack entry is this atom; only inspect what follows it.
      text.push_str(&preview(
        arena,
        source,
        rest[..rest.len() - 1].iter().rev().copied(),
        true,
        cancelled,
      )?);
    } else {
      let following = match rest.get(rest.len() - 2) {
        Some(Frame::Gap(next)) => preview(
          arena,
          source,
          [Frame::Node(next.gap, self.indent)],
          false,
          cancelled,
        )?,
        _ => String::new(),
      };
      text.push_str(if following.is_empty() {
        " "
      } else {
        &following
      });
      text.push('\\');
    }
    if self.forced || !fits_width(&text, cancelled)? {
      output.text(if separator.is_empty() {
        " "
      } else {
        &separator
      });
      output.text("\\");
      output.newline(1, self.indent + 2);
      Ok(None)
    } else {
      Ok(Some(self.gap))
    }
  }
}

fn render(
  arena: &Arena,
  root: Id,
  source: &str,
  cancelled: &AtomicBool,
) -> Result<Output, EngineError> {
  render_frames(
    arena,
    source,
    cancelled,
    Output::default(),
    vec![Frame::Node(root, 0)],
    false,
  )
}

fn render_frames(
  arena: &Arena,
  source: &str,
  cancelled: &AtomicBool,
  mut output: Output,
  mut stack: Vec<Frame>,
  natural: bool,
) -> Result<Output, EngineError> {
  while let Some(frame) = stack.pop() {
    check_cancelled(cancelled)?;
    let (id, indent) = match frame {
      Frame::End => break,
      Frame::Node(id, indent) => (id, indent),
      Frame::Gap(gap) => {
        let Some(id) =
          gap.render(arena, source, &mut output, &stack, cancelled)?
        else {
          continue;
        };
        (id, gap.indent)
      }
    };
    match &arena.0[id] {
      Doc::Source(range) => output.source(range.clone(), source),
      Doc::Text(text) => output.text(text),
      Doc::Sequence(items) => {
        stack.extend(items.iter().rev().map(|&id| Frame::Node(id, indent)));
      }
      Doc::Line(count) => {
        output.newline(*count, indent);
      }
      Doc::LineStart => {
        let count = usize::from(!output.line().trim().is_empty());
        output.newline(count, indent);
      }
      Doc::Indent => output.indentation(indent),
      Doc::Nest(extra, item) => stack.push(Frame::Node(*item, indent + extra)),
      Doc::WordBoundary(next) => {
        if !natural {
          let next = preview(
            arena,
            source,
            [Frame::Node(*next, indent)],
            true,
            cancelled,
          )?;
          if !fits_width(&format!("{}{next}", output.line()), cancelled)? {
            output.text("\\\n");
          }
        }
      }
      Doc::Word(content) => {
        if natural
          || fits(arena, source, &output, *content, indent, &stack, cancelled)?
        {
          stack.push(Frame::Node(*content, indent));
        } else {
          render_word(
            arena,
            source,
            &mut output,
            (*content, indent),
            &stack,
            cancelled,
          )?;
        }
      }
      Doc::NaturalChoice { flat, broken } => {
        let base = output.current_indent();
        let choice =
          if fits(arena, source, &output, *flat, base, &stack, cancelled)? {
            flat
          } else {
            broken
          };
        stack.push(Frame::Node(*choice, base));
      }
      Doc::Choice { flat, broken } => {
        let base = output.current_indent();
        if natural
          || fits(arena, source, &output, *flat, base, &stack, cancelled)?
        {
          stack.push(Frame::Node(*flat, base));
        } else if let Some(trial) =
          natural_layout(arena, source, &output, (id, base), &stack, cancelled)?
        {
          output = trial;
        } else {
          stack.push(Frame::Node(*broken, base));
        }
      }
      Doc::Command { atoms, gaps, name } => {
        let base = output.current_indent();
        if natural || fits(arena, source, &output, id, base, &stack, cancelled)?
        {
          expand_command(atoms, gaps, base, &mut stack);
        } else if let Some(trial) =
          natural_layout(arena, source, &output, (id, base), &stack, cancelled)?
        {
          output = trial;
        } else {
          expand_broken_command(atoms, gaps, *name, base, &mut stack);
        }
      }
    }
  }
  Ok(output)
}

fn natural_layout(
  arena: &Arena,
  source: &str,
  output: &Output,
  item: (Id, usize),
  rest: &[Frame],
  cancelled: &AtomicBool,
) -> Result<Option<Output>, EngineError> {
  let mut stack = rest.to_vec();
  stack.push(Frame::End);
  stack.push(Frame::Node(item.0, item.1));
  let result =
    render_frames(arena, source, cancelled, output.clone(), stack, true)?;
  let suffix =
    preview(arena, source, rest.iter().rev().copied(), true, cancelled)?;
  let mut measured = result.text[output.line..].to_owned();
  // A trailing layout newline is deferred until the following source is emitted.
  if result.pending_lines > 0 {
    measured.push('\n');
    measured.push_str(&" ".repeat(result.current_indent()));
  }
  measured.push_str(&suffix);
  Ok(fits_width(&measured, cancelled)?.then_some(result))
}

fn wrap_word(
  text: &str,
  suffix: &str,
  output: &mut Output,
  cancelled: &AtomicBool,
) -> Result<(), EngineError> {
  let mut remaining = text;
  while !remaining.is_empty() {
    check_cancelled(cancelled)?;
    let prefix = output.line();
    if fits_width(&format!("{prefix}{remaining}{suffix}"), cancelled)? {
      output.text(remaining);
      break;
    }
    let mut split = None;
    for (offset, _) in remaining.char_indices().skip(1) {
      if remaining[..offset].ends_with('\\')
        || remaining[..offset].contains('\n')
      {
        continue;
      }
      if !fits_width(&format!("{prefix}{}\\", &remaining[..offset]), cancelled)?
      {
        break;
      }
      split = Some(offset);
    }
    let Some(offset) = split else {
      output.text(remaining);
      break;
    };
    output.text(&remaining[..offset]);
    output.text("\\\n");
    remaining = &remaining[offset..];
  }
  Ok(())
}

fn expand_broken_command(
  atoms: &[Id],
  gaps: &[Id],
  name: Option<usize>,
  base: usize,
  stack: &mut Vec<Frame>,
) {
  for index in (0..atoms.len()).rev() {
    stack.push(Frame::Node(
      atoms[index],
      if index == 0 { base } else { base + 2 },
    ));
    if index > 0 {
      stack.push(Frame::Gap(CommandGap {
        gap: gaps[index - 1],
        atom: atoms[index],
        last: index + 1 == atoms.len(),
        forced: name == Some(index - 1),
        indent: base,
      }));
    }
  }
}

fn render_word(
  arena: &Arena,
  source: &str,
  output: &mut Output,
  item: (Id, usize),
  rest: &[Frame],
  cancelled: &AtomicBool,
) -> Result<(), EngineError> {
  let text = preview(
    arena,
    source,
    [Frame::Node(item.0, item.1)],
    false,
    cancelled,
  )?;
  let suffix =
    preview(arena, source, rest.iter().rev().copied(), true, cancelled)?;
  wrap_word(&text, &suffix, output, cancelled)
}

struct Edit {
  range: Range<usize>,
  text: String,
}

fn push_edit(
  edits: &mut Vec<Edit>,
  source: &str,
  mut range: Range<usize>,
  mut text: String,
) {
  let old = &source[range.clone()];
  let prefix: usize = old
    .chars()
    .zip(text.chars())
    .take_while(|(left, right)| left == right)
    .map(|(character, _)| character.len_utf8())
    .sum();
  range.start += prefix;
  text.drain(..prefix);
  let suffix: usize = source[range.clone()]
    .chars()
    .rev()
    .zip(text.chars().rev())
    .take_while(|(left, right)| left == right)
    .map(|(character, _)| character.len_utf8())
    .sum();
  range.end -= suffix;
  text.truncate(text.len() - suffix);
  if !range.is_empty() || !text.is_empty() {
    edits.push(Edit { range, text });
  }
}

fn lsp_range(source: &str, range: &Range<usize>) -> Range<usize> {
  let bytes = source.as_bytes();
  let middle = |offset: usize| {
    offset > 0
      && bytes.get(offset - 1) == Some(&b'\r')
      && bytes.get(offset) == Some(&b'\n')
  };
  range.start - usize::from(middle(range.start))
    ..range.end + usize::from(middle(range.end))
}

fn collect_edits(
  document: &Document,
  output: Output,
  cancelled: &AtomicBool,
) -> Result<Vec<FormatEdit>, EngineError> {
  let source = document.source();
  let mut edits = Vec::new();
  let mut cursor = 0;
  let mut pending = String::new();
  // A moved keyword must not displace the unchanged comments or here-document
  // that follow it in the output but precede it in the original source.
  let mut following = vec![source.len(); output.pieces.len() + 1];
  for (index, piece) in output.pieces.iter().enumerate().rev() {
    following[index] = match piece {
      Piece::Source(range) => following[index + 1].min(range.start),
      Piece::Text(_) => following[index + 1],
    };
  }
  for (index, piece) in output.pieces.into_iter().enumerate() {
    check_cancelled(cancelled)?;
    match piece {
      Piece::Source(range)
        if range.start >= cursor && following[index + 1] >= range.end =>
      {
        push_edit(
          &mut edits,
          source,
          cursor..range.start,
          std::mem::take(&mut pending),
        );
        cursor = range.end;
      }
      Piece::Source(range) => pending.push_str(&source[range]),
      Piece::Text(text) => pending.push_str(&text),
    }
  }
  push_edit(&mut edits, source, cursor..source.len(), pending);
  let mut result = Vec::new();
  let mut index = 0;
  while index < edits.len() {
    check_cancelled(cancelled)?;
    let start = index;
    let mut range = lsp_range(source, &edits[index].range);
    index += 1;
    while let Some(next) = edits.get(index) {
      let expanded = lsp_range(source, &next.range);
      if expanded.start > range.end {
        break;
      }
      range.end = expanded.end;
      index += 1;
    }
    let mut new_text = source[range.clone()].to_owned();
    for edit in edits[start..index].iter().rev() {
      new_text.replace_range(
        edit.range.start - range.start..edit.range.end - range.start,
        &edit.text,
      );
    }
    result.push(FormatEdit {
      location: Location {
        snapshot: document.snapshot(),
        bytes: range,
      },
      new_text,
    });
  }
  Ok(result)
}

fn fits_width(text: &str, cancelled: &AtomicBool) -> Result<bool, EngineError> {
  for line in text.split('\n') {
    let mut column = 0;
    for (index, part) in line.split('\t').enumerate() {
      check_cancelled(cancelled)?;
      if index > 0 {
        column += 8 - column % 8;
      }
      column += part.width();
      if column > 80 {
        check_cancelled(cancelled)?;
        return Ok(false);
      }
    }
  }
  check_cancelled(cancelled)?;
  Ok(true)
}
