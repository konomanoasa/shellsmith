use super::Workspace;
use super::derived::listing_key;
use super::documents::matches_query;
use crate::interface::application::engine::{
  Context, ContextId, DocumentId, Location, OccurrenceKind, Snapshot, Symbol,
  SymbolId, SymbolKind,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct SymbolDetails<'a> {
  pub symbol: &'a Symbol,
  pub location: Location,
  pub range: Option<Location>,
  pub container: Option<&'a Symbol>,
}

impl Workspace {
  #[must_use]
  pub fn document_symbol_details(
    &self,
    id: DocumentId,
  ) -> Vec<SymbolDetails<'_>> {
    self.symbol_details(Some(id), &[])
  }

  pub fn search_symbol_details<'a>(
    &'a self,
    query: &'a [u8],
  ) -> impl Iterator<Item = SymbolDetails<'a>> {
    self.symbol_details(None, query).into_iter()
  }

  fn symbol_details(
    &self,
    document: Option<DocumentId>,
    query: &[u8],
  ) -> Vec<SymbolDetails<'_>> {
    let mut groups = BTreeMap::new();
    for (symbol, location) in
      self.symbol_locations(&[]).filter(|(_, location)| {
        document.is_none_or(|id| location.snapshot.document == id)
      })
    {
      groups
        .entry(listing_key(symbol, &location))
        .or_insert_with(|| SymbolGroup {
          symbol,
          location,
          variants: Vec::new(),
        })
        .variants
        .push(symbol);
    }
    let symbols: Vec<_> = groups.into_values().collect();
    let mut hierarchy = Hierarchy::new(self, &symbols);
    symbols
      .into_iter()
      .filter(|group| matches_query(&group.symbol.name, query))
      .map(|group| {
        let mut variants = group.variants.into_iter().map(|symbol| {
          let site = self
            .occurrence(symbol.definition)
            .expect("symbol definition");
          let context = self.definition_context(symbol);
          let range = if site.kind == OccurrenceKind::ForVariable {
            Some(group.location.clone())
          } else {
            context
              .and_then(|context| self.navigation_location(&context.location))
              .filter(|range| range.contains(&group.location))
          };
          let enclosing = if symbol.kind == SymbolKind::Function {
            context.and_then(|context| context.parent)
          } else {
            Some(site.context)
          };
          let container = enclosing
            .and_then(|context| hierarchy.container(context))
            .and_then(|id| self.symbol(id));
          SymbolDetails {
            symbol: group.symbol,
            location: group.location.clone(),
            range,
            container,
          }
        });
        let mut details = variants.next().expect("symbol variant");
        for variant in variants {
          if variant.range != details.range {
            details.range = None;
          }
          if variant.container != details.container {
            details.container = None;
          }
        }
        details
      })
      .collect()
  }

  fn definition_context(&self, symbol: &Symbol) -> Option<&Context> {
    let site = self.occurrence(symbol.definition)?;
    let kind = match site.kind {
      OccurrenceKind::FunctionDefinition => "function_definition",
      OccurrenceKind::Assignment => "assignment_word",
      OccurrenceKind::DeclarationAssignment => "word",
      OccurrenceKind::VariableExpansion => "parameter_expansion",
      OccurrenceKind::Arithmetic => "arithmetic_assignment_expression",
      _ => return None,
    };
    let document = self.document(site.id.snapshot.document)?;
    let mut context = document.context(site.context)?;
    loop {
      if context.kind == kind {
        return Some(context);
      }
      context = document.context(context.parent?)?;
    }
  }
}

struct SymbolGroup<'a> {
  symbol: &'a Symbol,
  location: Location,
  variants: Vec<&'a Symbol>,
}

struct Hierarchy<'a> {
  workspace: &'a Workspace,
  functions: BTreeMap<ContextId, SymbolId>,
  registrations: BTreeMap<Snapshot, Vec<ContextId>>,
  containers: BTreeMap<ContextId, Option<SymbolId>>,
}

impl<'a> Hierarchy<'a> {
  fn new(workspace: &'a Workspace, symbols: &[SymbolGroup<'_>]) -> Self {
    let roots: BTreeSet<_> = symbols
      .iter()
      .map(|entry| entry.location.snapshot)
      .collect();
    let mut hierarchy = Self {
      workspace,
      functions: BTreeMap::new(),
      registrations: BTreeMap::new(),
      containers: BTreeMap::new(),
    };
    for group in symbols
      .iter()
      .filter(|group| group.symbol.kind == SymbolKind::Function)
    {
      for symbol in &group.variants {
        if let Some(context) = workspace.definition_context(symbol) {
          hierarchy.functions.insert(context.id, group.symbol.id);
        }
      }
    }
    for root in roots {
      let Some((_, indexed)) = workspace.indexed.get(&root.document) else {
        continue;
      };
      for (registration, body) in &indexed.bodies {
        let Some(site) = workspace.occurrence(*registration) else {
          continue;
        };
        for snapshot in body.documents.keys() {
          hierarchy
            .registrations
            .entry(*snapshot)
            .or_default()
            .push(site.context);
        }
      }
    }
    hierarchy
  }

  fn container(&mut self, start: ContextId) -> Option<SymbolId> {
    let mut pending = vec![(start, false)];
    let mut visiting = BTreeSet::new();
    while let Some((id, finish)) = pending.pop() {
      if self.containers.contains_key(&id) {
        continue;
      }
      if let Some(function) = self.functions.get(&id) {
        self.containers.insert(id, Some(*function));
        continue;
      }
      let parents = self.parents(id);
      if finish {
        let first = parents.first().and_then(|parent| self.containers[parent]);
        let container = parents
          .iter()
          .all(|parent| self.containers[parent] == first)
          .then_some(first)
          .flatten();
        self.containers.insert(id, container);
        visiting.remove(&id);
      } else if visiting.insert(id) {
        pending.push((id, true));
        pending.extend(parents.into_iter().map(|parent| (parent, false)));
      } else {
        self.containers.insert(id, None);
      }
    }
    self.containers[&start]
  }

  fn parents(&self, id: ContextId) -> Vec<ContextId> {
    let Some(context) = self
      .workspace
      .document(id.snapshot.document)
      .and_then(|document| document.context(id))
    else {
      return Vec::new();
    };
    context.parent.map_or_else(
      || {
        self
          .registrations
          .get(&id.snapshot)
          .cloned()
          .unwrap_or_default()
      },
      |parent| vec![parent],
    )
  }
}
