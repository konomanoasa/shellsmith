use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tree_sitter::{
  Node, ParseOptions, Parser, Query, QueryCursor, QueryCursorOptions,
  StreamingIterator, Tree,
};

use crate::interface::application::engine::check_cancelled;
use crate::interface::application::engine::word;
use crate::interface::application::engine::{
  Access, Classification, Context, ContextId, Error, Location, Occurrence,
  OccurrenceId, OccurrenceKind, Snapshot, Symbol, SymbolId, SymbolKind, Value,
  ValueIssue,
};
use crate::interface::application::engine::{ReferenceId, SourceReference};
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct Document {
  snapshot: Snapshot,
  pub(super) source: Arc<str>,
  pub(in crate::interface::application::engine) tree: Tree,
  facts: Facts,
  classifications: Vec<Classification>,
}

impl Document {
  pub fn parse(
    snapshot: Snapshot,
    bytes: &[u8],
    cancelled: &AtomicBool,
  ) -> Result<Self, Error> {
    check_cancelled(cancelled)?;
    if bytes.len() > u32::MAX as usize {
      return Err(Error::SourceTooLarge);
    }
    let source =
      std::str::from_utf8(bytes).map_err(|error| Error::InvalidEncoding {
        byte: error.valid_up_to(),
      })?;
    let mut parser = Parser::new();
    parser
      .set_language(&tree_sitter_sh::LANGUAGE.into())
      .map_err(|error| Error::Parser(error.to_string()))?;
    let mut progress = |_: &tree_sitter::ParseState| {
      if cancelled.load(Ordering::Relaxed) {
        ControlFlow::Break(())
      } else {
        ControlFlow::Continue(())
      }
    };
    let tree = parser.parse_with_options(
      &mut |offset, _| &bytes[offset..],
      None,
      Some(ParseOptions::new().progress_callback(&mut progress)),
    );
    check_cancelled(cancelled)?;
    let tree =
      tree.ok_or_else(|| Error::Parser("Parser did not complete".into()))?;
    Self::build(snapshot, source.into(), tree, cancelled)
  }

  fn build(
    snapshot: Snapshot,
    source: Arc<str>,
    tree: Tree,
    cancelled: &AtomicBool,
  ) -> Result<Self, Error> {
    let bytes = source.as_bytes();
    Ok(Self {
      snapshot,
      facts: extract(snapshot, bytes, &tree, cancelled)?,
      classifications: classify(snapshot, bytes, &tree, cancelled)?,
      source,
      tree,
    })
  }

  #[must_use]
  pub const fn snapshot(&self) -> Snapshot {
    self.snapshot
  }

  pub(super) fn instantiate(
    &self,
    snapshot: Snapshot,
    cancelled: &AtomicBool,
  ) -> Result<Self, Error> {
    Self::build(snapshot, self.source.clone(), self.tree.clone(), cancelled)
  }

  pub(in crate::interface::application::engine) fn argument_summaries(
    &self,
    analysis: Option<&crate::interface::application::engine::Analysis>,
  ) -> BTreeMap<
    (usize, usize),
    crate::interface::application::engine::ArgumentRole,
  > {
    let mut roles = analysis.map_or_else(BTreeMap::new, |analysis| {
      analysis.argument_summaries(self.snapshot())
    });
    for role in self.argument_roles() {
      roles
        .entry((role.location.bytes.start, role.location.bytes.end))
        .or_insert_with(|| role.clone());
    }
    roles
  }

  pub(in crate::interface::application::engine) fn has_body_candidates(
    &self,
  ) -> bool {
    self.occurrences().iter().any(body_candidate)
  }

  pub(in crate::interface::application::engine) fn command_at(
    &self,
    location: &Location,
  ) -> Option<tree_sitter::Node<'_>> {
    let node = self.tree.root_node().named_descendant_for_byte_range(
      location.bytes.start,
      location.bytes.end,
    )?;
    std::iter::successors(Some(node), tree_sitter::Node::parent)
      .find(|node| node.kind() == "simple_command")
  }

  /// Shell body candidates identified from syntax alone.
  pub(super) fn body_candidates(
    &self,
    cancelled: &AtomicBool,
  ) -> Result<
    impl Iterator<
      Item = (
        &Occurrence,
        crate::interface::application::engine::analysis::UnattemptedCommand,
      ),
    >,
    Error,
  > {
    check_cancelled(cancelled)?;
    Ok(
      self
        .occurrences()
        .iter()
        .filter(|site| body_candidate(site))
        .filter_map(move |site| {
          let node = self.command_at(&site.location)?;
          let command = crate::interface::application::engine::analysis::unattempted_command(
            self.snapshot(),
            node,
            self.source().as_bytes(),
          );
        Some((site, command))
      }),
    )
  }

  #[must_use]
  pub fn source(&self) -> &str {
    &self.source
  }

  #[must_use]
  pub fn has_syntax_errors(&self) -> bool {
    self.facts.syntax_errors
  }

  #[must_use]
  pub fn text(&self, location: &Location) -> Option<&str> {
    if location.snapshot != self.snapshot {
      return None;
    }
    self.source.get(location.bytes.clone())
  }

  #[must_use]
  pub fn contexts(&self) -> &[Context] {
    &self.facts.contexts
  }

  #[must_use]
  pub fn occurrences(&self) -> &[Occurrence] {
    &self.facts.occurrences
  }

  #[must_use]
  pub fn symbols(&self) -> &[Symbol] {
    &self.facts.symbols
  }

  #[must_use]
  pub fn classifications(&self) -> &[Classification] {
    &self.classifications
  }

  #[must_use]
  pub fn argument_roles(
    &self,
  ) -> &[crate::interface::application::engine::ArgumentRole] {
    &self.facts.argument_roles
  }

  #[must_use]
  pub fn references(&self) -> &[SourceReference] {
    &self.facts.references
  }

  #[must_use]
  pub fn reference(&self, id: ReferenceId) -> Option<&SourceReference> {
    (id.snapshot == self.snapshot)
      .then(|| self.facts.references.get(id.index))
      .flatten()
  }

  #[must_use]
  pub fn context(&self, id: ContextId) -> Option<&Context> {
    (id.snapshot == self.snapshot)
      .then(|| self.facts.contexts.get(id.index))
      .flatten()
  }

  #[must_use]
  pub fn occurrence(&self, id: OccurrenceId) -> Option<&Occurrence> {
    (id.snapshot == self.snapshot)
      .then(|| self.facts.occurrences.get(id.index))
      .flatten()
  }

  #[must_use]
  pub fn symbol(&self, id: SymbolId) -> Option<&Symbol> {
    (id.snapshot == self.snapshot)
      .then(|| self.facts.symbols.get(id.index))
      .flatten()
  }
}

#[derive(Debug, Default)]
struct Facts {
  pub syntax_errors: bool,
  pub contexts: Vec<Context>,
  pub occurrences: Vec<Occurrence>,
  pub symbols: Vec<Symbol>,
  pub references: Vec<SourceReference>,
  pub argument_roles: Vec<crate::interface::application::engine::ArgumentRole>,
}

#[derive(Clone, Copy, Default)]
struct State {
  context: Option<ContextId>,
  recovery: bool,
  transformed: bool,
  inert: bool,
}

fn extract(
  snapshot: Snapshot,
  source: &[u8],
  tree: &Tree,
  cancelled: &AtomicBool,
) -> Result<Facts, Error> {
  let mut facts = Facts::default();
  let mut pending = vec![(tree.root_node(), None, State::default())];
  while let Some((node, field, mut state)) = pending.pop() {
    check_cancelled(cancelled)?;
    let damaged = damaged(node);
    facts.syntax_errors |= damaged;
    state.recovery |= damaged;
    state.transformed |=
      matches!(node.kind(), "backquote_substitution" | "here_document_body");
    state.inert |= matches!(node.kind(), "here_end" | "tilde_expansion");
    if node == tree.root_node() || structural(node) {
      let id = ContextId {
        snapshot,
        index: facts.contexts.len(),
      };
      facts.contexts.push(Context {
        id,
        parent: state.context,
        kind: node.kind().into(),
        field: field.map(str::to_owned),
        location: Location {
          snapshot,
          bytes: node.byte_range(),
        },
      });
      state.context = Some(id);
    }
    if !state.recovery && !state.inert {
      facts.occurrence(snapshot, node, source, state);
      facts.reference(snapshot, node, source, state);
      if node.kind() == "simple_command"
        && (!state.transformed || !word::transformed(node, source))
      {
        facts.argument_roles.extend(
          crate::interface::application::engine::analysis::invariant_arguments(
            snapshot, node, source,
          ),
        );
      }
    }
    let mut cursor = node.walk();
    if cursor.goto_last_child() {
      loop {
        pending.push((cursor.node(), cursor.field_name(), state));
        if !cursor.goto_previous_sibling() {
          break;
        }
      }
    }
  }
  check_cancelled(cancelled)?;
  Ok(facts)
}

fn body_candidate(site: &Occurrence) -> bool {
  site.kind == OccurrenceKind::Command
    && (site.name.can_equal(b"trap") || site.name.can_equal(b"command"))
}

pub(in crate::interface::application::engine) fn command_word(
  node: Node<'_>,
) -> Option<Node<'_>> {
  node
    .child_by_field_name("name")
    .or_else(|| node.child_by_field_name("word"))
}

pub(in crate::interface::application::engine) fn damaged(
  node: Node<'_>,
) -> bool {
  node.is_error() || node.is_missing()
}

pub(in crate::interface::application::engine) fn subshell(kind: &str) -> bool {
  matches!(
    kind,
    "subshell" | "command_substitution" | "backquote_substitution"
  )
}

pub(in crate::interface::application::engine) fn asynchronous(
  node: Node<'_>,
) -> bool {
  if node.kind() != "and_or" {
    return false;
  }
  let separator =
    std::iter::successors(node.next_named_sibling(), Node::next_named_sibling)
      .find(|node| {
        matches!(node.kind(), "separator" | "separator_op" | "and_or")
      })
      .or_else(|| node.parent()?.parent()?.child_by_field_name("terminator"));
  separator
    .and_then(|node| {
      if node.kind() == "separator_op" {
        Some(node)
      } else {
        node.child_by_field_name("operator")
      }
    })
    .is_some_and(|operator| {
      operator
        .children(&mut operator.walk())
        .any(|child| child.kind() == "&")
    })
}

fn structural(node: Node<'_>) -> bool {
  node.is_named()
    && !damaged(node)
    && node
      .named_children(&mut node.walk())
      .any(|child| child.kind() != "line_continuation")
}

fn source_annotation(node: Node<'_>, source: &[u8]) -> Option<Vec<u8>> {
  if node.has_error() {
    return None;
  }
  let before = &source[..node.start_byte()];
  let newline = before.iter().rposition(|byte| *byte == b'\n')?;
  if !before[newline + 1..]
    .iter()
    .all(|byte| matches!(byte, b' ' | b'\t'))
  {
    return None;
  }
  let line_start = before[..newline]
    .iter()
    .rposition(|byte| *byte == b'\n')
    .map_or(0, |position| position + 1);
  let line = &source[line_start..newline];
  let line = if line_start == 0 {
    line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line)
  } else {
    line
  }
  .trim_ascii_start();
  let start = newline - line.len();
  let line = line.trim_ascii_end();
  let path = line.strip_prefix(b"# shellsmith source=")?.trim_ascii();
  let root = std::iter::successors(Some(node), Node::parent).last()?;
  let token =
    root.named_descendant_for_byte_range(start, start + line.len())?;
  let comment = std::iter::successors(Some(token), Node::parent)
    .find(|node| node.kind() == "comment")?;
  if comment.start_byte() != start || comment.end_byte() > newline {
    return None;
  }
  let path = match path.first()? {
    quote @ (b'\'' | b'"') => {
      let content = path.get(1..path.len().checked_sub(1)?)?;
      if path.last() != Some(quote) || content.contains(quote) {
        return None;
      }
      content
    }
    _ if path.iter().any(|byte| {
      byte.is_ascii_whitespace() || matches!(byte, b'\'' | b'"')
    }) =>
    {
      return None;
    }
    _ => path,
  };
  (!path.is_empty() && !path.contains(&0)).then(|| path.to_vec())
}

impl Facts {
  fn reference(
    &mut self,
    snapshot: Snapshot,
    node: Node<'_>,
    source: &[u8],
    state: State,
  ) {
    if node.kind() != "simple_command" {
      return;
    }
    let Some(command) = command_word(node) else {
      return;
    };
    if state.transformed && word::transformed(command, source) {
      return;
    }
    let name = word::literal_value(command, source);
    if matches!(&name, Value::Literal(name) if name != b"." && name != b"command")
    {
      return;
    }
    let Some(context) = state.context else {
      return;
    };
    let mut args: Vec<_> = node
      .child_by_field_name("suffix")
      .into_iter()
      .flat_map(|suffix| {
        suffix
          .named_children(&mut suffix.walk())
          .collect::<Vec<_>>()
      })
      .filter(|arg| arg.kind() == "word")
      .collect();
    let direct = name == Value::Literal(b".".to_vec());
    if direct
      && args.first().is_some_and(|arg| {
        word::literal_value(*arg, source) == Value::Literal(b"--".to_vec())
      })
    {
      args.remove(0);
    }
    let operand = args.first().copied();
    let value = if node.has_error() {
      Value::Unknown(ValueIssue::Recovery)
    } else if !direct {
      Value::Unknown(ValueIssue::Dynamic)
    } else if let Some(operand) = operand {
      if state.transformed && word::transformed(operand, source) {
        Value::Unknown(ValueIssue::TransformedSource)
      } else {
        match word::literal_value(operand, source) {
          Value::Literal(value) if value.is_empty() => {
            Value::Unknown(ValueIssue::EmptyOperand)
          }
          value => value,
        }
      }
    } else {
      Value::Unknown(ValueIssue::MissingOperand)
    };
    self.references.push(SourceReference {
      id: ReferenceId {
        snapshot,
        index: self.references.len(),
      },
      command: Location {
        snapshot,
        bytes: node.byte_range(),
      },
      operand: operand.map(|operand| Location {
        snapshot,
        bytes: operand.byte_range(),
      }),
      context,
      value,
      annotation: source_annotation(node, source),
    });
  }

  fn occurrence(
    &mut self,
    snapshot: Snapshot,
    node: Node<'_>,
    source: &[u8],
    state: State,
  ) {
    let Some((kind, access)) = role(node, source) else {
      return;
    };
    let Some(context) = state.context else {
      return;
    };
    let damaged_owner = kind != OccurrenceKind::Command
      && node.parent().is_some_and(|owner| {
        owner.has_error()
          && (kind != OccurrenceKind::FunctionDefinition
            || owner.children(&mut owner.walk()).any(|child| {
              Some(child) != owner.child_by_field_name("body")
                && child.has_error()
            }))
      });
    let name = if node.has_error() || node.is_missing() || damaged_owner {
      Value::Unknown(ValueIssue::Recovery)
    } else if state.transformed && word::transformed(node, source) {
      Value::Unknown(ValueIssue::TransformedSource)
    } else if kind == OccurrenceKind::Command {
      word::literal_value(node, source)
    } else {
      Value::Literal(word::lexical(node, source))
    };
    let id = OccurrenceId {
      snapshot,
      index: self.occurrences.len(),
    };
    if let Value::Literal(name) = &name {
      let symbol_kind = match access {
        Access::Definition => Some(SymbolKind::Function),
        Access::Write | Access::ReadWrite | Access::ReadConditionalWrite => {
          Some(SymbolKind::Variable)
        }
        _ => None,
      };
      if let Some(kind) = symbol_kind {
        self.symbols.push(Symbol {
          id: SymbolId {
            snapshot,
            index: self.symbols.len(),
          },
          kind,
          name: name.clone(),
          definition: id,
        });
      }
    }
    self.occurrences.push(Occurrence {
      id,
      context,
      location: Location {
        snapshot,
        bytes: node.byte_range(),
      },
      kind,
      access,
      name,
    });
  }
}

fn role(node: Node<'_>, source: &[u8]) -> Option<(OccurrenceKind, Access)> {
  use OccurrenceKind::{
    Arithmetic, Assignment, Command, DeclarationAssignment, ForVariable,
    FunctionDefinition, PositionalParameter, SpecialParameter,
    VariableExpansion,
  };
  let parent = node.parent()?;
  let role = match node.kind() {
    "fname" if parent.kind() == "function_definition" => {
      (FunctionDefinition, Access::Definition)
    }
    "cmd_name" | "cmd_word" => (Command, Access::Read),
    "name" if parent.kind() == "for_clause" => (ForVariable, Access::Write),
    "variable_name" if parent.kind() == "assignment_word" => {
      (Assignment, Access::Write)
    }
    "variable_name"
      if parent.kind() == "word"
        && parent.child_by_field_name("name") == Some(node) =>
    {
      (DeclarationAssignment, Access::Write)
    }
    "variable_name" if parent.kind() == "parameter_expansion" => {
      let operator = parent
        .child_by_field_name("operator")
        .map(|operator| word::lexical(operator, source));
      let access = match operator.as_deref() {
        Some(b"=" | b":=") => Access::ReadConditionalWrite,
        None
        | Some(
          b"-" | b":-" | b"+" | b":+" | b"?" | b":?" | b"#" | b"##" | b"%"
          | b"%%",
        ) => Access::Read,
        Some(_) => Access::Unknown,
      };
      (VariableExpansion, access)
    }
    "positional_parameter" if parent.kind() == "parameter_expansion" => {
      (PositionalParameter, Access::Read)
    }
    "special_parameter" if parent.kind() == "parameter_expansion" => {
      (SpecialParameter, Access::Read)
    }
    "arithmetic_variable" => (Arithmetic, arithmetic_access(node, source)),
    _ => return None,
  };
  Some(role)
}

fn arithmetic_access(mut node: Node<'_>, source: &[u8]) -> Access {
  while let Some(parent) = node.parent() {
    match parent.kind() {
      "parenthesized_arithmetic" => node = parent,
      "arithmetic_assignment_expression"
        if parent.child_by_field_name("left") == Some(node) =>
      {
        return match parent
          .child_by_field_name("operator")
          .map(|operator| word::lexical(operator, source))
          .as_deref()
        {
          Some(b"=") => Access::Write,
          Some(
            b"*=" | b"/=" | b"%=" | b"+=" | b"-=" | b"<<=" | b">>=" | b"&="
            | b"^=" | b"|=",
          ) => Access::ReadWrite,
          _ => Access::Unknown,
        };
      }
      "arithmetic_dynamic_expression" | "parenthesized_arithmetic_source" => {
        return Access::Unknown;
      }
      _ => return Access::Read,
    }
  }
  Access::Read
}

fn classify(
  snapshot: Snapshot,
  source: &[u8],
  tree: &Tree,
  cancelled: &AtomicBool,
) -> Result<Vec<Classification>, Error> {
  static QUERY: OnceLock<Result<Query, String>> = OnceLock::new();
  let query = QUERY
    .get_or_init(|| {
      Query::new(
        &tree_sitter_sh::LANGUAGE.into(),
        tree_sitter_sh::HIGHLIGHTS_QUERY,
      )
      .map_err(|error| error.to_string())
    })
    .as_ref()
    .map_err(|error| Error::Parser(error.clone()))?;
  let mut cursor = QueryCursor::new();
  let mut progress = |_: &tree_sitter::QueryCursorState| {
    if cancelled.load(Ordering::Relaxed) {
      ControlFlow::Break(())
    } else {
      ControlFlow::Continue(())
    }
  };
  let mut spans = BTreeMap::new();
  let mut matches = cursor.matches_with_options(
    query,
    tree.root_node(),
    source,
    QueryCursorOptions::new().progress_callback(&mut progress),
  );
  while let Some(result) = matches.next() {
    check_cancelled(cancelled)?;
    for capture in result.captures() {
      let node = capture.node;
      if node.is_missing() || node.has_error() || node.byte_range().is_empty() {
        continue;
      }
      let key = (node.start_byte(), node.end_byte());
      let entry = spans.entry(key).or_insert((0, capture.index));
      if result.pattern_index >= entry.0 {
        *entry = (result.pattern_index, capture.index);
      }
    }
  }
  check_cancelled(cancelled)?;
  if cursor.did_exceed_match_limit() {
    return Err(Error::Parser(
      "Classification query exceeded its match limit".into(),
    ));
  }
  Ok(
    spans
      .into_iter()
      .map(|((start, end), (_, capture))| Classification {
        location: Location {
          snapshot,
          bytes: start..end,
        },
        capture: query.capture_names()[capture as usize],
      })
      .collect(),
  )
}
