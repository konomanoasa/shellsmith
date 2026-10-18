use super::engine::{DocumentId, SymbolDetails};
use super::{Application, Error};
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct DocumentSymbol<'a> {
  pub details: SymbolDetails<'a>,
  /// Index of the parent in this document-symbol result.
  pub parent: Option<usize>,
}

impl Application {
  pub fn document_symbols(
    &self,
    id: DocumentId,
  ) -> Result<impl Iterator<Item = DocumentSymbol<'_>>, Error> {
    self
      .workspace
      .document(id)
      .ok_or(super::engine::Error::UnknownDocument(id))?;
    let mut entries = self.workspace.document_symbol_details(id);
    entries.sort_by_key(|entry| {
      (
        entry.location.bytes.start,
        std::cmp::Reverse(
          entry.range.as_ref().unwrap_or(&entry.location).bytes.end,
        ),
        entry.symbol.id,
      )
    });
    let indices: BTreeMap<_, _> = entries
      .iter()
      .enumerate()
      .map(|(index, entry)| (entry.symbol.id, index))
      .collect();
    let parents: Vec<_> = entries
      .iter()
      .map(|entry| {
        let range = entry.range.as_ref().unwrap_or(&entry.location);
        let mut container = entry.container;
        while let Some(parent) = container
          .and_then(|symbol| indices.get(&symbol.id))
          .copied()
        {
          let outer = &entries[parent];
          if outer.range.as_ref().is_some_and(|outer| {
            outer.snapshot == range.snapshot
              && outer.bytes.start <= range.bytes.start
              && range.bytes.end <= outer.bytes.end
          }) {
            return Some(parent);
          }
          container = outer.container;
        }
        None
      })
      .collect();
    Ok(
      entries
        .into_iter()
        .zip(parents)
        .map(|(details, parent)| DocumentSymbol { details, parent }),
    )
  }
}
