use super::{
  AnalysisResults, NameId, NameOccurrence, NameRelations, projection,
};
use crate::interface::application::engine::{
  Access, Error, Location, Snapshot, SymbolId, SymbolKind, check_cancelled,
};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::AtomicBool;

type SourceRange = (Snapshot, usize, usize);
pub type NameSite = (usize, usize);
pub type NameTargets = BTreeMap<usize, NameId>;
type Definitions =
  BTreeMap<(SymbolKind, Vec<u8>, SourceRange), BTreeMap<usize, Option<NameId>>>;

#[derive(Clone, Debug)]
pub struct EntryNames {
  pub relations: NameRelations,
  pub definitions: BTreeMap<SymbolId, Option<Location>>,
}

pub struct RelatedNames {
  pub sites: BTreeSet<NameSite>,
  pub members: BTreeSet<(usize, NameId)>,
}

#[derive(Clone, Debug, Default)]
pub struct NameResults {
  pub entries: Vec<EntryNames>,
  groups: BTreeMap<SourceRange, Vec<NameSite>>,
}

impl AnalysisResults<'_> {
  pub fn names(&self, cancelled: &AtomicBool) -> Result<NameResults, Error> {
    let mut result = NameResults::default();
    let mut blocked = BTreeMap::new();
    let mut snapshots = BTreeMap::new();
    for workspace in &self.entries {
      for document in workspace.documents().filter(|document| {
        workspace
          .derived_origin(document.snapshot().document)
          .is_none()
      }) {
        check_cancelled(cancelled)?;
        let snapshot = document.snapshot();
        if snapshots
          .insert(snapshot.document, snapshot)
          .is_some_and(|previous| previous != snapshot)
        {
          return Err(Error::StaleSnapshot(snapshot));
        }
        if !document.has_body_candidates() {
          continue;
        }
        blocked.entry(document.snapshot()).or_insert_with(|| {
          let workspaces = self.workspaces(document.snapshot());
          projection::blocked_projections(document, &workspaces)
        });
      }
    }
    for (entry, workspace) in self.entries.iter().enumerate() {
      check_cancelled(cancelled)?;
      let mut relations = workspace.name_relations(cancelled)?;
      for (index, site) in relations.occurrences.iter_mut().enumerate() {
        check_cancelled(cancelled)?;
        if workspace
          .derived_origin(site.site.snapshot().document)
          .is_some()
          && blocked.get(&site.location.snapshot).is_some_and(|ranges| {
            ranges.iter().any(|range| {
              range.start <= site.location.bytes.start
                && site.location.bytes.end <= range.end
            })
          })
        {
          site.compatible = false;
          site.identity = None;
        }
        let location = &site.location;
        result
          .groups
          .entry((location.snapshot, location.bytes.start, location.bytes.end))
          .or_default()
          .push((entry, index));
      }
      let symbols: BTreeSet<_> = relations
        .occurrences
        .iter()
        .flat_map(|site| {
          site.definitions.iter().chain(
            site
              .resolution
              .iter()
              .flat_map(|resolution| &resolution.candidates),
          )
        })
        .chain(relations.names.iter().flat_map(|name| &name.definitions))
        .copied()
        .collect();
      let mut definitions = BTreeMap::new();
      for symbol in symbols {
        check_cancelled(cancelled)?;
        let location = workspace
          .symbol(symbol)
          .and_then(|symbol| workspace.definition_location(symbol));
        definitions.insert(symbol, location);
      }
      result.entries.push(EntryNames {
        relations,
        definitions,
      });
    }
    Ok(result)
  }
}

impl NameResults {
  #[must_use]
  pub fn occurrence(&self, (entry, index): NameSite) -> &NameOccurrence {
    &self.entries[entry].relations.occurrences[index]
  }

  pub fn groups(&self) -> impl Iterator<Item = &[NameSite]> {
    self.groups.values().map(Vec::as_slice)
  }

  /// Unattempted occurrences stay available for safety checks, but do not veto reached contexts.
  pub fn reached<'a>(
    &'a self,
    sites: &'a [NameSite],
  ) -> impl Iterator<Item = NameSite> + 'a {
    sites
      .iter()
      .copied()
      .filter(|site| self.occurrence(*site).context.is_some())
  }

  pub fn at(
    &self,
    snapshot: Snapshot,
    offset: usize,
    editable: bool,
    cancelled: &AtomicBool,
  ) -> Result<Vec<NameSite>, Error> {
    let mut selected = Vec::new();
    let mut width = usize::MAX;
    for sites in self.groups() {
      check_cancelled(cancelled)?;
      let size = sites
        .iter()
        .flat_map(|site| {
          let occurrence = self.occurrence(*site);
          occurrence
            .spans
            .iter()
            .filter(move |_| !editable)
            .chain(occurrence.spelling.editable().filter(|_| editable))
        })
        .filter(|location| {
          location.snapshot == snapshot && location.bytes.contains(&offset)
        })
        .map(|location| location.bytes.len())
        .min();
      let Some(size) = size else { continue };
      if size < width {
        selected.clear();
        width = size;
      }
      if size == width {
        selected.extend(sites);
      }
    }
    Ok(selected)
  }

  #[must_use]
  pub fn identities(&self, sites: &[NameSite]) -> Option<NameTargets> {
    let mut targets = BTreeMap::new();
    for (entry, index) in self.reached(sites) {
      let site = &self.entries[entry].relations.occurrences[index];
      let identity = site.identity?;
      if targets
        .insert(entry, identity)
        .is_some_and(|previous| previous != identity)
      {
        return None;
      }
    }
    (!targets.is_empty()).then_some(targets)
  }

  pub fn corresponding_names(
    &self,
    selected: &NameTargets,
    cancelled: &AtomicBool,
  ) -> Result<Option<NameTargets>, Error> {
    let definitions: Vec<_> =
      self.definition_owners(cancelled)?.into_values().collect();
    let mut by_name: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (index, owners) in definitions.iter().enumerate() {
      for (entry, identity) in owners {
        check_cancelled(cancelled)?;
        if let Some(identity) = identity {
          by_name.entry((*entry, *identity)).or_default().push(index);
        }
      }
    }
    let Some((&entry, &identity)) = selected.first_key_value() else {
      return Ok(None);
    };
    let mut targets = BTreeMap::from([(entry, identity)]);
    let mut pending = VecDeque::from([(entry, identity)]);
    let mut visited = vec![false; definitions.len()];
    while let Some(member) = pending.pop_front() {
      check_cancelled(cancelled)?;
      for index in by_name.get(&member).into_iter().flatten() {
        if visited[*index] {
          continue;
        }
        visited[*index] = true;
        for (&entry, identity) in &definitions[*index] {
          check_cancelled(cancelled)?;
          let Some(identity) = *identity else { continue };
          match targets.entry(entry) {
            Entry::Vacant(slot) => {
              slot.insert(identity);
              pending.push_back((entry, identity));
            }
            Entry::Occupied(slot) if *slot.get() != identity => {
              return Ok(None);
            }
            Entry::Occupied(_) => {}
          }
        }
      }
    }
    Ok(
      selected
        .iter()
        .all(|(entry, identity)| targets.get(entry) == Some(identity))
        .then_some(targets),
    )
  }

  fn definition_owners(
    &self,
    cancelled: &AtomicBool,
  ) -> Result<Definitions, Error> {
    let mut definitions = Definitions::new();
    for (entry, result) in self.entries.iter().enumerate() {
      for site in &result.relations.occurrences {
        check_cancelled(cancelled)?;
        let Some(name) = site.name.map(|id| &result.relations.names[id.0])
        else {
          continue;
        };
        for symbol in &site.definitions {
          let Some(Some(location)) = result.definitions.get(symbol) else {
            continue;
          };
          definitions
            .entry((
              name.kind,
              name.text.clone(),
              (location.snapshot, location.bytes.start, location.bytes.end),
            ))
            .or_default()
            .entry(entry)
            .and_modify(|identity| {
              if *identity != site.identity {
                *identity = None;
              }
            })
            .or_insert(site.identity);
        }
      }
    }
    Ok(definitions)
  }

  #[must_use]
  pub fn navigation(
    &self,
    sites: &[NameSite],
    targets: &NameTargets,
  ) -> Option<(Location, bool)> {
    let mut reached = self.reached(sites);
    let first = reached.next()?;
    let location = self.occurrence(first).navigation()?;
    let mut declaration = false;
    for site in std::iter::once(first).chain(reached) {
      let occurrence = self.occurrence(site);
      if occurrence.identity.as_ref()? != targets.get(&site.0)?
        || occurrence.navigation().as_ref() != Some(&location)
      {
        return None;
      }
      declaration |=
        matches!(occurrence.access, Access::Definition | Access::Write);
    }
    Some((location, declaration))
  }

  /// Potential relationships include unattempted uses and shared source spellings.
  pub fn related_occurrences(
    &self,
    candidates: Vec<NameSite>,
    cancelled: &AtomicBool,
  ) -> Result<RelatedNames, Error> {
    let mut by_name: BTreeMap<_, Vec<NameSite>> = BTreeMap::new();
    let mut by_source: BTreeMap<_, Vec<NameSite>> = BTreeMap::new();
    for (entry, result) in self.entries.iter().enumerate() {
      for (index, site) in result.relations.occurrences.iter().enumerate() {
        check_cancelled(cancelled)?;
        for id in &site.names {
          by_name
            .entry((entry, *id))
            .or_default()
            .push((entry, index));
        }
        for location in
          std::iter::once(&site.location).chain(site.spelling.editable())
        {
          by_source
            .entry((
              location.snapshot,
              location.bytes.start,
              location.bytes.end,
            ))
            .or_default()
            .push((entry, index));
        }
      }
    }
    let mut sites = BTreeSet::new();
    let mut members = BTreeSet::new();
    let mut expanded = BTreeSet::new();
    let mut pending: VecDeque<_> = candidates.into();
    while let Some(site) = pending.pop_front() {
      check_cancelled(cancelled)?;
      if !sites.insert(site) {
        continue;
      }
      let occurrence = self.occurrence(site);
      for id in &occurrence.names {
        if members.insert((site.0, *id)) {
          pending
            .extend(by_name.get(&(site.0, *id)).into_iter().flatten().copied());
        }
      }
      for location in std::iter::once(&occurrence.location)
        .chain(occurrence.spelling.editable())
      {
        let range =
          (location.snapshot, location.bytes.start, location.bytes.end);
        if expanded.insert(range) {
          pending.extend(by_source.get(&range).into_iter().flatten().copied());
        }
      }
    }
    Ok(RelatedNames { sites, members })
  }
}
