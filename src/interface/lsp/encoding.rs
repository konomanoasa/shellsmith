use crate::interface::application::engine::{
  self, DocumentId, Modifier, SemanticKind, SemanticSpan, SymbolDetails,
  SymbolKind,
};
use crate::interface::application::{Application, Error};
use async_lsp::lsp_types as lsp;
use async_lsp::{ErrorCode, ResponseError};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use url::Url;

#[derive(Clone, Copy, Default)]
pub(super) enum SymbolFormat {
  #[default]
  Flat,
  Hierarchical,
}

#[derive(Clone)]
pub(super) struct OpenDocument {
  pub(super) uri: Url,
  pub(super) version: i32,
}

pub(super) fn invalid(message: impl std::fmt::Display) -> ResponseError {
  ResponseError::new(ErrorCode::INVALID_PARAMS, message)
}

pub(super) fn fault(error: Error) -> ResponseError {
  let code = match error {
    Error::RenameRefused(_) => ErrorCode::REQUEST_FAILED,
    Error::Superseded => ErrorCode::CONTENT_MODIFIED,
    Error::InvalidName(_) | Error::Engine(engine::Error::InvalidPosition) => {
      ErrorCode::INVALID_PARAMS
    }
    Error::Engine(engine::Error::Cancelled) => ErrorCode::REQUEST_CANCELLED,
    _ => ErrorCode::INTERNAL_ERROR,
  };
  ResponseError::new(code, error)
}

pub(super) fn file_path(uri: &Url) -> Result<PathBuf, ResponseError> {
  if uri.scheme() != "file" || uri.query().is_some() || uri.fragment().is_some()
  {
    return Err(invalid("Expected a file URI"));
  }
  uri
    .to_file_path()
    .map_err(|()| invalid("Unsupported file URI"))
}

pub(super) fn uri(
  application: &Application,
  opened: &BTreeMap<PathBuf, OpenDocument>,
  path: &Path,
) -> Result<Url, Error> {
  if let Some(document) = opened.get(path) {
    if document
      .uri
      .to_file_path()
      .ok()
      .is_none_or(|client_path| application.absolute(&client_path) != path)
    {
      return Err(Error::Superseded);
    }
    return Ok(document.uri.clone());
  }
  Url::from_file_path(path)
    .map_err(|()| Error::Message("Cannot encode file URI".into()))
}

pub(super) fn document_source(
  application: &Application,
  id: DocumentId,
) -> Result<&str, Error> {
  Ok(
    application
      .workspace()
      .document(id)
      .ok_or(engine::Error::UnknownDocument(id))?
      .source(),
  )
}

pub(super) fn document_offset(
  application: &Application,
  id: DocumentId,
  position: lsp::Position,
) -> Result<usize, Error> {
  Ok(
    Lines::new(document_source(application, id)?)
      .offset(position)
      .ok_or(engine::Error::InvalidPosition)?,
  )
}

pub(super) fn workspace_edit(
  application: &Application,
  edits: Vec<crate::interface::application::RenameEdit>,
  opened: &BTreeMap<PathBuf, OpenDocument>,
  document_changes: bool,
) -> Result<lsp::WorkspaceEdit, Error> {
  let mut documents: BTreeMap<PathBuf, Vec<lsp::TextEdit>> = BTreeMap::new();
  let mut positions = Positions::new(application);
  for edit in edits {
    let range = positions.result_range(&edit.location)?;
    let path = positions.path(edit.location.snapshot.document)?;
    documents
      .entry(path.to_path_buf())
      .or_default()
      .push(lsp::TextEdit {
        range,
        new_text: edit.new_text,
      });
  }
  let mut output = lsp::WorkspaceEdit::default();
  if document_changes {
    output.document_changes = Some(lsp::DocumentChanges::Edits(
      documents
        .into_iter()
        .map(|(path, edits)| {
          let document = opened.get(&path);
          Ok(lsp::TextDocumentEdit {
            text_document: lsp::OptionalVersionedTextDocumentIdentifier {
              uri: uri(application, opened, &path)?,
              version: document.map(|document| document.version),
            },
            edits: edits.into_iter().map(lsp::OneOf::Left).collect(),
          })
        })
        .collect::<Result<_, Error>>()?,
    ));
  } else {
    output.changes = Some(
      documents
        .into_iter()
        .map(|(path, edits)| Ok((uri(application, opened, &path)?, edits)))
        .collect::<Result<_, Error>>()?,
    );
  }
  Ok(output)
}

pub(super) fn locations(
  application: &Application,
  opened: &BTreeMap<PathBuf, OpenDocument>,
  locations: &[engine::Location],
) -> Result<Vec<lsp::Location>, Error> {
  let mut positions = Positions::new(application);
  locations
    .iter()
    .map(|location| {
      let range = positions.result_range(location)?;
      let path = positions.path(location.snapshot.document)?;
      Ok(lsp::Location {
        uri: uri(application, opened, path)?,
        range,
      })
    })
    .collect()
}

#[expect(
  deprecated,
  reason = "SymbolInformation requires initializing its deprecated field; None omits it from the response."
)]
pub(super) fn symbols<'a>(
  application: &Application,
  opened: &BTreeMap<PathBuf, OpenDocument>,
  locations: impl Iterator<Item = SymbolDetails<'a>>,
) -> Result<Vec<lsp::SymbolInformation>, Error> {
  let mut symbols = Vec::new();
  let mut positions = Positions::new(application);
  for entry in locations {
    let symbol = entry.symbol;
    let location = entry.location;
    let Some(range) = positions.range(&location)? else {
      continue;
    };
    let path = positions.path(location.snapshot.document)?;
    symbols.push(lsp::SymbolInformation {
      name: String::from_utf8_lossy(&symbol.name).into_owned(),
      kind: symbol_kind(symbol.kind),
      location: lsp::Location {
        uri: uri(application, opened, path)?,
        range,
      },
      tags: None,
      deprecated: None,
      container_name: entry
        .container
        .map(|parent| String::from_utf8_lossy(&parent.name).into_owned()),
    });
  }
  Ok(symbols)
}

fn symbol_kind(kind: SymbolKind) -> lsp::SymbolKind {
  match kind {
    SymbolKind::Function => lsp::SymbolKind::FUNCTION,
    SymbolKind::Variable => lsp::SymbolKind::VARIABLE,
  }
}

#[expect(
  deprecated,
  reason = "DocumentSymbol requires initializing its deprecated field; None omits it from the response."
)]
pub(super) fn document_symbols(
  application: &Application,
  opened: &BTreeMap<PathBuf, OpenDocument>,
  id: DocumentId,
  format: SymbolFormat,
) -> Result<lsp::DocumentSymbolResponse, Error> {
  let entries = application.document_symbols(id)?;
  if matches!(format, SymbolFormat::Flat) {
    return symbols(application, opened, entries.map(|entry| entry.details))
      .map(lsp::DocumentSymbolResponse::Flat);
  }
  let lines = Lines::new(document_source(application, id)?);
  let entries: Vec<_> = entries.collect();
  let mut children: BTreeMap<Option<usize>, Vec<lsp::DocumentSymbol>> =
    BTreeMap::new();
  for (index, entry) in entries.into_iter().enumerate().rev() {
    let details = entry.details;
    let mut nested = children.remove(&Some(index)).unwrap_or_default();
    let Some((range, selection_range)) = lines
      .range(&details.range.as_ref().unwrap_or(&details.location).bytes)
      .zip(lines.range(&details.location.bytes))
    else {
      children.entry(entry.parent).or_default().extend(nested);
      continue;
    };
    nested.reverse();
    children
      .entry(entry.parent)
      .or_default()
      .push(lsp::DocumentSymbol {
        name: String::from_utf8_lossy(&details.symbol.name).into_owned(),
        detail: None,
        kind: symbol_kind(details.symbol.kind),
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children: (!nested.is_empty()).then_some(nested),
      });
  }
  let mut roots = children.remove(&None).unwrap_or_default();
  roots.reverse();
  Ok(lsp::DocumentSymbolResponse::Nested(roots))
}

pub(super) const TYPES: &[&str] = &[
  "comment",
  "keyword",
  "operator",
  "string",
  "number",
  "variable",
  "parameter",
  "function",
  "command",
];
pub(super) const MODIFIERS: &[&str] = &[
  "declaration",
  "definition",
  "modification",
  "readonly",
  "defaultLibrary",
  "environment",
];

pub(super) struct Positions<'a> {
  application: &'a Application,
  lines: BTreeMap<DocumentId, Lines<'a>>,
  paths: BTreeMap<DocumentId, &'a Path>,
}

impl<'a> Positions<'a> {
  pub(super) const fn new(application: &'a Application) -> Self {
    Self {
      application,
      lines: BTreeMap::new(),
      paths: BTreeMap::new(),
    }
  }

  /// Invalid byte ranges return `None`.
  pub(super) fn range(
    &mut self,
    location: &engine::Location,
  ) -> Result<Option<lsp::Range>, Error> {
    let id = location.snapshot.document;
    let lines = match self.lines.entry(id) {
      std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
      std::collections::btree_map::Entry::Vacant(entry) => {
        entry.insert(Lines::new(document_source(self.application, id)?))
      }
    };
    Ok(lines.range(&location.bytes))
  }

  /// Result ranges always lie within their documents.
  pub(super) fn result_range(
    &mut self,
    location: &engine::Location,
  ) -> Result<lsp::Range, Error> {
    self
      .range(location)?
      .ok_or_else(|| Error::Message("Invalid result range".into()))
  }

  pub(super) fn path(&mut self, id: DocumentId) -> Result<&'a Path, Error> {
    if let Some(path) = self.paths.get(&id) {
      return Ok(path);
    }
    let path = self
      .application
      .path(id)
      .ok_or_else(|| Error::Message("Unknown document path".into()))?;
    self.paths.insert(id, path);
    Ok(path)
  }
}

struct Lines<'a> {
  source: &'a str,
  starts: Vec<usize>,
  ends: Vec<usize>,
  excess: Vec<(usize, usize)>,
}
impl<'a> Lines<'a> {
  fn new(source: &'a str) -> Self {
    let mut total = 0;
    let excess = source
      .char_indices()
      .filter_map(|(offset, character)| {
        if character.is_ascii() {
          return None;
        }
        total += character.len_utf8() - character.len_utf16();
        Some((offset + character.len_utf8(), total))
      })
      .collect();
    let bytes = source.as_bytes();
    let mut starts = vec![0];
    let mut ends = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
      if matches!(bytes[offset], b'\r' | b'\n') {
        ends.push(offset);
        if bytes[offset] == b'\r' && bytes.get(offset + 1) == Some(&b'\n') {
          offset += 1;
        }
        starts.push(offset + 1);
      }
      offset += 1;
    }
    ends.push(source.len());
    Self {
      source,
      starts,
      ends,
      excess,
    }
  }

  fn offset(&self, position: lsp::Position) -> Option<usize> {
    let line = usize::try_from(position.line).ok()?;
    let Some(&start) = self.starts.get(line) else {
      return Some(self.source.len());
    };
    let end = self.ends[line];
    let target = usize::try_from(position.character).ok()?;
    let mut column = 0;
    for (offset, character) in self.source[start..end].char_indices() {
      if column == target {
        return Some(start + offset);
      }
      column += character.len_utf16();
      if column > target {
        return None;
      }
    }
    Some(end)
  }

  fn range(&self, bytes: &std::ops::Range<usize>) -> Option<lsp::Range> {
    let (sl, sc) = self.position(bytes.start)?;
    let (el, ec) = self.position(bytes.end)?;
    Some(lsp::Range::new(
      lsp::Position::new(sl, sc),
      lsp::Position::new(el, ec),
    ))
  }
  fn excess_at(&self, offset: usize) -> usize {
    let index = self.excess.partition_point(|(end, _)| *end <= offset);
    index.checked_sub(1).map_or(0, |index| self.excess[index].1)
  }
  /// Nonempty segments as (line, UTF-16 column, UTF-16 length).
  fn segments(
    &self,
    bytes: std::ops::Range<usize>,
  ) -> impl Iterator<Item = (u32, u32, u32)> + '_ {
    let mut offset = bytes.start;
    std::iter::from_fn(move || {
      while offset < bytes.end {
        let row = self.starts.partition_point(|start| *start <= offset) - 1;
        let text =
          &self.source[offset..bytes.end.min(self.ends[row]).max(offset)];
        let position = self.position(offset);
        offset = self
          .starts
          .get(row + 1)
          .copied()
          .unwrap_or(bytes.end)
          .min(bytes.end);
        if let Some((line, column)) = position
          && let Ok(length) = u32::try_from(text.encode_utf16().count())
          && length > 0
        {
          return Some((line, column, length));
        }
      }
      None
    })
  }

  fn position(&self, offset: usize) -> Option<(u32, u32)> {
    if !self.source.is_char_boundary(offset) {
      return None;
    }
    let line = self
      .starts
      .partition_point(|start| *start <= offset)
      .checked_sub(1)?;
    let offset = offset.min(self.ends[line]);
    Some((
      u32::try_from(line).ok()?,
      u32::try_from(
        offset
          - self.starts[line]
          - (self.excess_at(offset) - self.excess_at(self.starts[line])),
      )
      .ok()?,
    ))
  }
}

pub(super) fn encode(
  source: &str,
  spans: &[SemanticSpan],
  legend: &lsp::SemanticTokensLegend,
) -> Vec<lsp::SemanticToken> {
  let mut events: BTreeMap<usize, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
  for (index, span) in spans.iter().enumerate() {
    let range = &span.location.bytes;
    if range.is_empty()
      || !source.is_char_boundary(range.start)
      || !source.is_char_boundary(range.end)
      || !legend
        .token_types
        .iter()
        .any(|name| name.as_str() == token_kind(span.kind))
    {
      continue;
    }
    events.entry(range.start).or_default().0.push(index);
    events.entry(range.end).or_default().1.push(index);
  }
  let mut active = BTreeSet::new();
  let mut ranges = Vec::new();
  let mut previous = 0;
  for (offset, (starts, ends)) in events {
    if previous < offset
      && let Some((_, _, index)) = active.last()
    {
      ranges.push((previous, offset, *index));
    }
    for index in ends {
      let span = &spans[index];
      active.remove(&(
        span.semantic,
        std::cmp::Reverse(span.location.bytes.len()),
        index,
      ));
    }
    for index in starts {
      let span = &spans[index];
      active.insert((
        span.semantic,
        std::cmp::Reverse(span.location.bytes.len()),
        index,
      ));
    }
    previous = offset;
  }
  let lines = Lines::new(source);
  let mut tokens: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
  for (start, end, index) in ranges {
    let span = &spans[index];
    let Some(kind) = legend
      .token_types
      .iter()
      .position(|name| name.as_str() == token_kind(span.kind))
      .and_then(|i| u32::try_from(i).ok())
    else {
      continue;
    };
    let bits = span
      .modifiers
      .iter()
      .filter_map(|modifier| {
        legend
          .token_modifiers
          .iter()
          .position(|name| name.as_str() == token_modifier(*modifier))
      })
      .fold(0, |bits, index| bits | (1 << index));
    for (line, column, length) in lines.segments(start..end) {
      if let Some(last) = tokens.last_mut()
        && last.0 == line
        && last.1 + last.2 == column
        && last.3 == kind
        && last.4 == bits
      {
        last.2 += length;
      } else {
        tokens.push((line, column, length, kind, bits));
      }
    }
  }
  encode_relative(tokens)
}

fn encode_relative(
  tokens: Vec<(u32, u32, u32, u32, u32)>,
) -> Vec<lsp::SemanticToken> {
  let mut encoded = Vec::with_capacity(tokens.len());
  let (mut last_line, mut last_column) = (0, 0);
  for (line, column, length, kind, bits) in tokens {
    encoded.push(lsp::SemanticToken {
      delta_line: line - last_line,
      delta_start: if line == last_line {
        column - last_column
      } else {
        column
      },
      length,
      token_type: kind,
      token_modifiers_bitset: bits,
    });
    last_line = line;
    last_column = column;
  }
  encoded
}

const fn token_kind(kind: SemanticKind) -> &'static str {
  match kind {
    SemanticKind::Comment => "comment",
    SemanticKind::Keyword => "keyword",
    SemanticKind::Operator => "operator",
    SemanticKind::String => "string",
    SemanticKind::Number => "number",
    SemanticKind::Variable => "variable",
    SemanticKind::Parameter => "parameter",
    SemanticKind::Function => "function",
    SemanticKind::Command => "command",
  }
}
const fn token_modifier(modifier: Modifier) -> &'static str {
  match modifier {
    Modifier::Declaration => "declaration",
    Modifier::Definition => "definition",
    Modifier::Modification => "modification",
    Modifier::Readonly => "readonly",
    Modifier::DefaultLibrary => "defaultLibrary",
    Modifier::Environment => "environment",
  }
}
