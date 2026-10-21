use super::{Document, Workspace};
use crate::interface::application::engine::{
  self, Analysis, DependencyState, DocumentId, Error, Location, Occurrence,
  OccurrenceId, Revision, Snapshot, Symbol, SymbolId, Value, check_cancelled,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OriginMapping {
  Linear(usize),
  Whole(Vec<Range<usize>>),
  Generated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OriginPart {
  pub bytes: Range<usize>,
  pub source: OriginMapping,
}

impl OriginPart {
  pub(in crate::interface::application::engine) fn new(
    bytes: Range<usize>,
    source: Option<Range<usize>>,
  ) -> Self {
    let source = match source {
      Some(source) if source.len() == bytes.len() => {
        OriginMapping::Linear(source.start)
      }
      Some(source) => OriginMapping::Whole(vec![source]),
      None => OriginMapping::Generated,
    };
    Self { bytes, source }
  }

  pub(in crate::interface::application::engine) fn merge(
    &mut self,
    other: &Self,
  ) -> bool {
    if self.bytes.end != other.bytes.start {
      return false;
    }
    let contiguous = match (&self.source, &other.source) {
      (OriginMapping::Linear(start), OriginMapping::Linear(next)) => {
        *start + self.bytes.len() == *next
      }
      (OriginMapping::Generated, OriginMapping::Generated) => true,
      _ => false,
    };
    if contiguous {
      self.bytes.end = other.bytes.end;
    }
    contiguous
  }

  pub(in crate::interface::application::engine) fn project(
    &self,
    bytes: Range<usize>,
  ) -> Option<Vec<Range<usize>>> {
    match &self.source {
      OriginMapping::Linear(start) => {
        let range = start + bytes.start - self.bytes.start
          ..start + bytes.end - self.bytes.start;
        Some(vec![range])
      }
      OriginMapping::Whole(ranges) if bytes == self.bytes => {
        Some(ranges.clone())
      }
      _ => None,
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivedOrigin {
  pub argument: Location,
  pub parts: Vec<OriginPart>,
}

impl DerivedOrigin {
  pub(in crate::interface::application::engine) fn project(
    &self,
    bytes: Range<usize>,
  ) -> Option<Vec<Location>> {
    if bytes.is_empty() {
      return self
        .project_point(bytes.start)
        .map(|location| vec![location]);
    }
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut covered = bytes.start;
    for part in &self.parts {
      let start = part.bytes.start.max(bytes.start);
      let end = part.bytes.end.min(bytes.end);
      if start >= end {
        continue;
      }
      if start != covered {
        return None;
      }
      for range in part.project(start..end)? {
        if let Some(last) = ranges.last_mut()
          && last.end == range.start
        {
          last.end = range.end;
        } else {
          ranges.push(range);
        }
      }
      covered = end;
    }
    (covered == bytes.end && !ranges.is_empty()).then(|| {
      ranges
        .into_iter()
        .map(|bytes| Location {
          snapshot: self.argument.snapshot,
          bytes,
        })
        .collect()
    })
  }

  fn project_point(&self, point: usize) -> Option<Location> {
    let mut position = None;
    for part in self
      .parts
      .iter()
      .filter(|part| part.bytes.start <= point && point <= part.bytes.end)
    {
      let next = match &part.source {
        OriginMapping::Linear(start) => start + point - part.bytes.start,
        OriginMapping::Whole(ranges) if point == part.bytes.start => {
          ranges.first()?.start
        }
        OriginMapping::Whole(ranges) if point == part.bytes.end => {
          ranges.last()?.end
        }
        _ => return None,
      };
      if position.is_some_and(|old| old != next) {
        return None;
      }
      position = Some(next);
    }
    position.map(|point| Location {
      snapshot: self.argument.snapshot,
      bytes: point..point,
    })
  }

  fn compose(&self, parts: &[OriginPart]) -> Vec<OriginPart> {
    let mut output: Vec<OriginPart> = Vec::new();
    for part in parts {
      let next = match &part.source {
        OriginMapping::Linear(start) => {
          for outer in &self.parts {
            let first = (*start).max(outer.bytes.start);
            let end = (start + part.bytes.len()).min(outer.bytes.end);
            if first >= end {
              continue;
            }
            let bytes =
              part.bytes.start + first - start..part.bytes.start + end - start;
            let source = match &outer.source {
              OriginMapping::Linear(offset) => {
                OriginMapping::Linear(offset + first - outer.bytes.start)
              }
              _ => outer
                .project(first..end)
                .map_or(OriginMapping::Generated, OriginMapping::Whole),
            };
            let next = OriginPart { bytes, source };
            if output.last_mut().is_none_or(|last| !last.merge(&next)) {
              output.push(next);
            }
          }
          continue;
        }
        OriginMapping::Whole(ranges) => {
          let projected: Option<Vec<_>> = ranges
            .iter()
            .map(|range| self.project(range.clone()))
            .collect();
          let source = projected.map_or(OriginMapping::Generated, |ranges| {
            OriginMapping::Whole(
              ranges
                .into_iter()
                .flatten()
                .map(|location| location.bytes)
                .collect(),
            )
          });
          OriginPart {
            bytes: part.bytes.clone(),
            source,
          }
        }
        OriginMapping::Generated => part.clone(),
      };
      if output.last_mut().is_none_or(|last| !last.merge(&next)) {
        output.push(next);
      }
    }
    output
  }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum MappingKey {
  Linear(usize),
  Whole(Vec<(usize, usize)>),
  Generated,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct OriginKey {
  start: usize,
  end: usize,
  source: MappingKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::interface::application::engine) struct DerivedSite {
  root: Snapshot,
  program: Option<Arc<[u8]>>,
  range: (usize, usize),
  argument: (usize, usize),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::interface::application::engine) struct DerivedKey {
  pub site: DerivedSite,
  pub body: Arc<[u8]>,
  parts: Arc<[OriginKey]>,
}

impl DerivedKey {
  fn new(
    mut origin: DerivedOrigin,
    body: &[u8],
    units: &std::collections::BTreeMap<DocumentId, DerivedUnit>,
  ) -> (Self, DerivedOrigin) {
    let parent = units.get(&origin.argument.snapshot.document);
    let range = (origin.argument.bytes.start, origin.argument.bytes.end);
    if let Some(unit) = parent {
      let argument = unit.origin.project(origin.argument.bytes.clone());
      origin.parts = unit.origin.compose(&origin.parts);
      origin.argument = argument
        .and_then(|ranges| {
          Some(Location {
            snapshot: unit.origin.argument.snapshot,
            bytes: ranges.first()?.bytes.start..ranges.last()?.bytes.end,
          })
        })
        .unwrap_or_else(|| unit.origin.argument.clone());
    }
    let key = Self {
      site: DerivedSite {
        root: origin.argument.snapshot,
        program: parent.map(|unit| unit.key.body.clone()),
        range,
        argument: (origin.argument.bytes.start, origin.argument.bytes.end),
      },
      body: body.into(),
      parts: origin
        .parts
        .iter()
        .map(|part| OriginKey {
          start: part.bytes.start,
          end: part.bytes.end,
          source: match &part.source {
            OriginMapping::Linear(start) => MappingKey::Linear(*start),
            OriginMapping::Whole(ranges) => MappingKey::Whole(
              ranges
                .iter()
                .map(|range| (range.start, range.end))
                .collect(),
            ),
            OriginMapping::Generated => MappingKey::Generated,
          },
        })
        .collect(),
    };
    (key, origin)
  }
}

#[derive(Clone, Debug)]
pub(in crate::interface::application::engine) struct DerivedUnit {
  pub key: DerivedKey,
  pub origin: DerivedOrigin,
  pub document: Arc<Document>,
}

#[derive(Clone, Debug, Default)]
pub(in crate::interface::application::engine) struct DerivedDocuments {
  pub candidates: BTreeMap<DerivedKey, DerivedOrigin>,
  pub bodies: BTreeMap<OccurrenceId, DeferredBody>,
}

#[derive(Clone, Debug, Default)]
pub(in crate::interface::application::engine) struct DeferredBody {
  pub documents: BTreeMap<Snapshot, Location>,
  pub incomplete: bool,
  pub conditional: bool,
}

impl DerivedDocuments {
  pub fn incomplete_bodies(
    &self,
    analysis: Option<&Analysis>,
  ) -> BTreeSet<Snapshot> {
    let mut incomplete: BTreeSet<_> = self
      .bodies
      .values()
      .flat_map(|body| {
        body.documents.iter().filter_map(|(snapshot, argument)| {
          (body.incomplete
            || body.conditional
            || analysis
              .and_then(|analysis| {
                analysis.argument_summary(engine::UseSite::argument(argument))
              })
              .is_some_and(|role| {
                role.kind != Some(engine::ArgumentKind::ShellBody)
              }))
          .then_some(*snapshot)
        })
      })
      .collect();
    let mut descendants: BTreeMap<_, Vec<_>> = BTreeMap::new();
    let mut pending: Vec<_> = incomplete.iter().copied().collect();
    for (site, body) in &self.bodies {
      descendants
        .entry(site.snapshot)
        .or_default()
        .extend(body.documents.keys().copied());
      if body.documents.len() > 1 {
        pending.extend(body.documents.keys().copied());
      }
    }
    while let Some(snapshot) = pending.pop() {
      if let Some(children) = descendants.remove(&snapshot) {
        for child in children {
          if incomplete.insert(child) {
            pending.push(child);
          }
        }
      }
    }
    incomplete
  }

  /// The candidates with the snapshots of their documents in `workspace`.
  pub(in crate::interface::application::engine) fn materialized<'a>(
    &'a self,
    workspace: &'a Workspace,
  ) -> impl Iterator<Item = (&'a DerivedKey, Snapshot)> + 'a {
    self.candidates.keys().filter_map(|key| {
      workspace
        .derived_keys
        .get(key)
        .map(|snapshot| (key, *snapshot))
    })
  }

  /// Materializes the derived document whose text is `body` at `origin`.
  pub fn derive(
    &mut self,
    workspace: &mut Workspace,
    origin: DerivedOrigin,
    body: &[u8],
    cancelled: &AtomicBool,
  ) -> Result<Snapshot, Error> {
    let (key, origin) = DerivedKey::new(origin, body, &workspace.derived);
    if !workspace.derived_keys.contains_key(&key) {
      workspace.add_derived(key.clone(), origin.clone(), cancelled)?;
    }
    let (key, snapshot) = workspace
      .derived_keys
      .get_key_value(&key)
      .expect("derived document");
    self.candidates.entry(key.clone()).or_insert(origin);
    Ok(*snapshot)
  }

  pub fn body(
    &mut self,
    workspace: &mut Workspace,
    registration: OccurrenceId,
    action: Result<Option<(DerivedOrigin, Value)>, ()>,
    limit: usize,
    cancelled: &AtomicBool,
  ) -> Result<Option<Snapshot>, Error> {
    let body = self.bodies.entry(registration).or_default();
    let (origin, value) = match action {
      Ok(Some(action)) => action,
      Ok(None) => return Ok(None),
      Err(()) => {
        body.incomplete = true;
        return Ok(None);
      }
    };
    let Value::Literal(value) = value else {
      body.incomplete = true;
      return Ok(None);
    };
    if value.len() > limit
      || value.contains(&0)
      || std::str::from_utf8(&value).is_err()
    {
      body.incomplete = true;
      return Ok(None);
    }
    let argument = origin.argument.clone();
    let snapshot = self.derive(workspace, origin, &value, cancelled)?;
    self
      .bodies
      .get_mut(&registration)
      .expect("body registration")
      .documents
      .insert(snapshot, argument);
    Ok(Some(snapshot))
  }

  pub fn collect(
    &mut self,
    workspace: &mut Workspace,
    mut pending: BTreeSet<Snapshot>,
    analysis: Option<&Analysis>,
    limit: usize,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let mut visited = BTreeSet::new();
    while let Some(snapshot) = pending.pop_first() {
      if !visited.insert(snapshot) {
        continue;
      }
      check_cancelled(cancelled)?;
      let document = workspace
        .documents
        .get(&snapshot.document)
        .ok_or(Error::UnknownDocument(snapshot.document))?
        .clone();
      for (site, command) in document.body_candidates(cancelled)? {
        check_cancelled(cancelled)?;
        let registration = &site.id;
        if self.bodies.contains_key(registration)
          || analysis.is_some_and(|analysis| {
            analysis
              .resolutions(engine::UseSite::Syntax(*registration))
              .next()
              .is_some()
          })
          || (!command.incomplete_body && command.bodies.is_empty())
        {
          continue;
        }
        self.bodies.insert(
          *registration,
          DeferredBody {
            incomplete: command.incomplete_body,
            conditional: site.name != Value::Literal(b"trap".to_vec()),
            ..DeferredBody::default()
          },
        );
        for ((start, end), (value, parts)) in &command.bodies {
          let origin = DerivedOrigin {
            argument: Location {
              snapshot,
              bytes: *start..*end,
            },
            parts: parts.clone(),
          };
          pending.extend(self.body(
            workspace,
            *registration,
            Ok(Some((origin, value.clone()))),
            limit,
            cancelled,
          )?);
        }
      }
    }
    Ok(())
  }
}

/// Identifies the definitions that symbol listings present as one entry: the
/// same name and kind defined at the same location.
pub(super) fn listing_key<'a>(
  symbol: &'a Symbol,
  location: &Location,
) -> (&'a [u8], Snapshot, usize, usize, engine::SymbolKind) {
  (
    symbol.name.as_slice(),
    location.snapshot,
    location.bytes.start,
    location.bytes.end,
    symbol.kind,
  )
}

pub(in crate::interface::application::engine) const fn reserved(
  id: DocumentId,
) -> bool {
  id.0 & (1 << 63) != 0
}

impl Workspace {
  pub fn semantic_spans(
    &self,
    id: DocumentId,
  ) -> Result<Vec<engine::SemanticSpan>, Error> {
    let document = self.document(id).ok_or(Error::UnknownDocument(id))?;
    let mut spans = document.semantic_spans(self.analysis());
    for group in self.projections(document.snapshot(), self.analysis()) {
      if !group.complete {
        continue;
      }
      let mut projected: BTreeMap<_, ProjectedSpan> = BTreeMap::new();
      for unit in &group.documents {
        for mut span in unit.document.semantic_spans(self.analysis()) {
          let Some(locations) = self.project_locations(&span.location) else {
            continue;
          };
          for location in locations
            .into_iter()
            .filter(|location| location.snapshot == document.snapshot())
          {
            span.location = location;
            span.semantic = true;
            projected
              .entry((span.location.bytes.start, span.location.bytes.end))
              .and_modify(|old| {
                old.join(&span, unit.document.snapshot().document);
              })
              .or_insert_with(|| {
                ProjectedSpan::new(
                  span.clone(),
                  unit.document.snapshot().document,
                )
              });
          }
        }
      }
      for projected in projected.into_values() {
        if projected.compatible
          && projected.documents.len() == group.documents.len()
        {
          spans.push(projected.span);
        }
      }
    }
    spans.sort_by_key(|span| {
      (
        span.location.bytes.start,
        span.location.bytes.end,
        span.kind,
      )
    });
    spans.dedup();
    Ok(spans)
  }

  /// Collects embedded syntax without assuming an execution environment.
  pub fn index_document(
    &mut self,
    id: DocumentId,
    limits: engine::AnalysisLimits,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    self.index(id, limits, cancelled)?;
    self.prune_unused_derived();
    Ok(())
  }

  /// Indexes `id` and keeps unretained derived documents, which an analysis
  /// in progress may still use.
  pub(in crate::interface::application::engine) fn index(
    &mut self,
    id: DocumentId,
    limits: engine::AnalysisLimits,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    check_cancelled(cancelled)?;
    if self
      .indexed
      .get(&id)
      .is_some_and(|(limit, _)| *limit == limits.value_bytes)
    {
      return Ok(());
    }
    let document = self.document(id).ok_or(Error::UnknownDocument(id))?;
    let snapshot = document.snapshot();
    limits.validate()?;
    document.prepare_deferred(cancelled)?;
    if !document.has_body_candidates() {
      self.indexed.insert(
        id,
        (limits.value_bytes, Arc::new(DerivedDocuments::default())),
      );
      return Ok(());
    }
    let mut staged = self.clone();
    let mut derived = DerivedDocuments::default();
    derived.collect(
      &mut staged,
      [snapshot].into(),
      None,
      limits.value_bytes,
      cancelled,
    )?;
    staged
      .indexed
      .insert(id, (limits.value_bytes, Arc::new(derived)));
    check_cancelled(cancelled)?;
    *self = staged;
    Ok(())
  }

  pub(in crate::interface::application::engine) fn add_derived(
    &mut self,
    mut key: DerivedKey,
    origin: DerivedOrigin,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    if self.next_derived >= 1 << 63 {
      return Err(Error::InvalidAnalysis(
        "Derived document IDs exhausted".into(),
      ));
    }
    let id = DocumentId(u64::MAX - self.next_derived);
    self.next_derived += 1;
    let snapshot = Snapshot {
      document: id,
      revision: Revision(1),
    };
    let document = Arc::new(
      if let Some((body, program)) = self.programs.get_key_value(&key.body) {
        key.body = body.clone();
        program.instantiate(snapshot, cancelled)?
      } else {
        Document::parse(snapshot, &key.body, cancelled)?
      },
    );
    self.insert_arc(document.clone())?;
    self
      .programs
      .entry(key.body.clone())
      .or_insert_with(|| document.clone());
    self.derived_keys.insert(key.clone(), snapshot);
    self.derived.insert(
      id,
      DerivedUnit {
        key,
        origin,
        document,
      },
    );
    Ok(())
  }

  pub(super) fn prune_derived(
    &mut self,
    entry: DocumentId,
    derived: &DerivedDocuments,
  ) {
    let documents = derived
      .materialized(self)
      .map(|(_, snapshot)| snapshot.document)
      .collect();
    self.derived_entries.insert(entry, documents);
    self.prune_unused_derived();
  }

  pub(super) fn prune_unused_derived(&mut self) {
    let retained: BTreeSet<_> = self
      .derived_entries
      .values()
      .flatten()
      .copied()
      .chain(
        self
          .indexed
          .values()
          .flat_map(|(_, derived)| derived.materialized(self))
          .map(|(_, snapshot)| snapshot.document),
      )
      .collect();
    let removed: Vec<_> = self
      .derived
      .keys()
      .filter(|id| !retained.contains(id))
      .copied()
      .collect();
    for id in &removed {
      self.discard(*id);
    }
    if !removed.is_empty() {
      self.changed();
    }
  }

  #[must_use]
  pub fn derived_origin(
    &self,
    id: DocumentId,
  ) -> Option<&crate::interface::application::engine::DerivedOrigin> {
    self.derived.get(&id).map(|unit| &unit.origin)
  }

  #[must_use]
  pub fn source_snapshot(&self, snapshot: Snapshot) -> Snapshot {
    self
      .derived
      .get(&snapshot.document)
      .map_or(snapshot, |unit| unit.origin.argument.snapshot)
  }

  #[must_use]
  pub fn project_locations(
    &self,
    location: &crate::interface::application::engine::Location,
  ) -> Option<Vec<crate::interface::application::engine::Location>> {
    self.document(location.snapshot.document)?.text(location)?;
    let locations = match self.derived_origin(location.snapshot.document) {
      Some(origin) => origin.project(location.bytes.clone())?,
      None => vec![location.clone()],
    };
    for projected in &locations {
      self
        .document(projected.snapshot.document)?
        .text(projected)?;
    }
    Some(locations)
  }

  #[must_use]
  pub fn navigation_location(
    &self,
    location: &crate::interface::application::engine::Location,
  ) -> Option<crate::interface::application::engine::Location> {
    let locations = self.project_locations(location)?;
    let first = locations.first()?;
    let last = locations.last()?;
    Some(crate::interface::application::engine::Location {
      snapshot: first.snapshot,
      bytes: first.bytes.start..last.bytes.end,
    })
  }

  /// The navigation location of the definition of `symbol`.
  #[must_use]
  pub fn definition_location(&self, symbol: &Symbol) -> Option<Location> {
    let site = self.occurrence(symbol.definition)?;
    self.navigation_location(&site.location)
  }

  pub fn projected_symbols(
    &self,
    id: DocumentId,
  ) -> impl Iterator<Item = (&Symbol, crate::interface::application::engine::Location)>
  {
    self
      .search_symbol_locations(&[])
      .filter(move |(_, location)| location.snapshot.document == id)
  }

  pub fn search_symbol_locations<'a>(
    &'a self,
    query: &'a [u8],
  ) -> impl Iterator<
    Item = (&'a Symbol, crate::interface::application::engine::Location),
  > {
    let mut seen = BTreeSet::new();
    self
      .symbol_locations(query)
      .filter(move |(symbol, location)| {
        seen.insert(listing_key(symbol, location))
      })
  }

  pub(super) fn symbol_locations<'a>(
    &'a self,
    query: &'a [u8],
  ) -> impl Iterator<Item = (&'a Symbol, Location)> {
    let mut visible = BTreeSet::new();
    let roots: BTreeSet<_> = self
      .search_symbols(query)
      .filter(|symbol| self.derived.contains_key(&symbol.id.snapshot.document))
      .map(|symbol| self.source_snapshot(symbol.id.snapshot))
      .collect();
    for root in roots {
      for group in self.projections(root, None).filter(|group| group.complete) {
        let mut matches: BTreeMap<
          _,
          (BTreeSet<SymbolId>, BTreeSet<DocumentId>),
        > = BTreeMap::new();
        for unit in &group.documents {
          for symbol in unit.document.symbols() {
            let Some(location) = self.definition_location(symbol) else {
              continue;
            };
            let (symbols, documents) =
              matches.entry(listing_key(symbol, &location)).or_default();
            symbols.insert(symbol.id);
            documents.insert(symbol.id.snapshot.document);
          }
        }
        visible.extend(
          matches
            .into_values()
            .filter(|(_, documents)| documents.len() == group.documents.len())
            .flat_map(|(ids, _)| ids),
        );
      }
    }
    self.search_symbols(query).filter_map(move |symbol| {
      if self.derived.contains_key(&symbol.id.snapshot.document)
        && !visible.contains(&symbol.id)
      {
        return None;
      }
      Some((symbol, self.definition_location(symbol)?))
    })
  }
}

impl Workspace {
  /// Resolved operands from the current analysis, projected to the requested document.
  #[must_use]
  pub fn source_links(
    &self,
    id: DocumentId,
  ) -> Vec<(crate::interface::application::engine::Location, Snapshot)> {
    let Some(analysis) = self.analysis() else {
      return Vec::new();
    };
    let mut inputs: BTreeMap<
      _,
      crate::interface::application::engine::SourceInput,
    > = BTreeMap::new();
    for (reference, _, input) in analysis.source_inputs() {
      inputs
        .entry(reference)
        .and_modify(|old| old.join(input))
        .or_insert_with(|| input.clone());
    }
    let mut variants: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
    for (key, snapshot) in analysis.derived.materialized(self) {
      variants
        .entry(key.site.clone())
        .or_default()
        .insert(snapshot.document);
    }
    let mut projected: BTreeMap<_, (Option<Snapshot>, BTreeSet<_>)> =
      BTreeMap::new();
    for (reference, input) in inputs {
      let Some(operand) = analysis.source_operand(reference) else {
        continue;
      };
      let Some(location) = self.navigation_location(operand) else {
        continue;
      };
      if location.snapshot.document != id {
        continue;
      }
      let target = match analysis.source_bindings().get(&reference) {
        Some(DependencyState::Resolved(target))
          if (matches!(input.operand, Value::Literal(_))
            || input.annotation.is_some())
            && self
              .document(target.document)
              .is_some_and(|doc| doc.snapshot() == *target) =>
        {
          Some(*target)
        }
        _ => None,
      };
      let site = self
        .derived
        .get(&reference.snapshot.document)
        .map(|unit| unit.key.site.clone());
      let entry = projected
        .entry((
          site,
          location.snapshot,
          location.bytes.start,
          location.bytes.end,
        ))
        .or_insert_with(|| (target, BTreeSet::new()));
      if entry.0 != target {
        entry.0 = None;
      }
      entry.1.insert(reference.snapshot.document);
    }
    let mut links: BTreeMap<_, Option<Snapshot>> = BTreeMap::new();
    for ((site, snapshot, start, end), (mut target, documents)) in projected {
      if site.is_some_and(|site| variants.get(&site) != Some(&documents)) {
        target = None;
      }
      links
        .entry((snapshot, start, end))
        .and_modify(|old| {
          if *old != target {
            *old = None;
          }
        })
        .or_insert(target);
    }
    links
      .into_iter()
      .filter_map(|((snapshot, start, end), target)| {
        target.map(|target| {
          (
            crate::interface::application::engine::Location {
              snapshot,
              bytes: start..end,
            },
            target,
          )
        })
      })
      .collect()
  }
  pub fn resolution_at(
    &self,
    snapshot: Snapshot,
    offset: usize,
  ) -> Result<Option<engine::Resolution>, Error> {
    let document = self.check_current(snapshot)?;
    if !document.source().is_char_boundary(offset) {
      return Err(Error::InvalidPosition);
    }
    if let Some(resolution) = self.projected_resolution_at(snapshot, offset) {
      return Ok(resolution.resolution);
    }
    let analysis = self.analysis();
    let mut selected: Option<((usize, u8), Option<engine::Resolution>)> = None;
    let mut consider =
      |width: usize, priority: u8, resolution: Option<engine::Resolution>| {
        let key = (width, priority);
        match &mut selected {
          Some((previous, _)) if *previous < key => {}
          Some((previous, result)) if *previous == key => {
            join_resolution(result, resolution.as_ref());
          }
          _ => selected = Some((key, resolution)),
        }
      };
    for site in document.occurrences() {
      if self
        .project_locations_at(&site.location, snapshot, offset)
        .is_empty()
      {
        continue;
      }
      let width = syntax_spelling(document, site)
        .iter()
        .flat_map(|location| {
          self.project_locations_at(location, snapshot, offset)
        })
        .map(|location| location.bytes.len())
        .min();
      if let Some(width) = width {
        consider(width, 0, syntax_resolution(document, site, analysis));
      }
    }
    if let Some(analysis) = analysis {
      for role in analysis.argument_summaries(snapshot).into_values() {
        let width = role
          .spelling
          .iter()
          .flat_map(|location| {
            self.project_locations_at(location, snapshot, offset)
          })
          .map(|location| location.bytes.len())
          .min();
        if let Some(width) = width {
          consider(width, 1, argument_resolution(analysis, &role));
        }
      }
    }
    Ok(selected.and_then(|(_, resolution)| resolution))
  }

  fn project_locations_at(
    &self,
    location: &Location,
    snapshot: Snapshot,
    offset: usize,
  ) -> Vec<Location> {
    let mut locations = self.project_locations(location).unwrap_or_default();
    locations.retain(|location| {
      location.snapshot == snapshot && location.bytes.contains(&offset)
    });
    locations
  }

  fn projections(
    &self,
    snapshot: Snapshot,
    analysis: Option<&Analysis>,
  ) -> impl Iterator<Item = ProjectedDocuments<'_>> {
    let mut groups: BTreeMap<_, ProjectedDocuments<'_>> = BTreeMap::new();
    if self.derived.is_empty() {
      return groups.into_values();
    }
    let analysis = analysis.filter(|analysis| {
      analysis.contexts().iter().any(|context| {
        self.source_snapshot(context.location.snapshot) == snapshot
      }) || analysis
        .derived
        .candidates
        .values()
        .any(|origin| origin.argument.snapshot == snapshot)
    });
    let derived = analysis.map(|analysis| &analysis.derived).or_else(|| {
      self
        .indexed
        .get(&snapshot.document)
        .map(|(_, derived)| derived.as_ref())
    });
    let Some(derived) = derived else {
      return groups.into_values();
    };
    let arguments: BTreeSet<_> = analysis
      .into_iter()
      .flat_map(Analysis::argument_definition_documents)
      .collect();
    let incomplete = derived.incomplete_bodies(analysis);
    let gaps: BTreeSet<_> = analysis
      .into_iter()
      .flat_map(Analysis::gaps)
      .map(|gap| {
        (
          gap.location.snapshot,
          gap.location.bytes.start,
          gap.location.bytes.end,
        )
      })
      .collect();
    for unit in derived
      .materialized(self)
      .filter_map(|(_, snapshot)| self.derived.get(&snapshot.document))
      .filter(|unit| {
        self.source_snapshot(unit.document.snapshot()) == snapshot
          && !arguments.contains(&unit.document.snapshot())
      })
    {
      let origin = &unit.origin.argument;
      let group = groups.entry(unit.key.site.clone()).or_insert_with(|| {
        ProjectedDocuments {
          documents: Vec::new(),
          complete: true,
        }
      });
      group.documents.push(unit);
      group.complete &= !incomplete.contains(&unit.document.snapshot())
        && !gaps.contains(&(
          origin.snapshot,
          origin.bytes.start,
          origin.bytes.end,
        ));
    }
    groups.into_values()
  }

  fn projected_resolution_at(
    &self,
    snapshot: Snapshot,
    offset: usize,
  ) -> Option<ProjectedResolution> {
    let mut selected: Option<(usize, ProjectedResolution)> = None;
    for ((start, end), projected) in
      self.projected_resolutions(snapshot, Some(offset))
    {
      let width = end - start;
      match &mut selected {
        Some((previous, _)) if *previous < width => {}
        Some((previous, resolution)) if *previous == width => {
          resolution.join(&projected);
        }
        _ => selected = Some((width, projected)),
      }
    }
    selected.map(|(_, resolution)| resolution)
  }

  pub(super) fn projected_resolutions(
    &self,
    snapshot: Snapshot,
    offset: Option<usize>,
  ) -> BTreeMap<(usize, usize), ProjectedResolution> {
    let analysis = self.analysis();
    let mut result: BTreeMap<_, ProjectedResolution> = BTreeMap::new();
    for group in self.projections(snapshot, analysis) {
      let mut projected: BTreeMap<_, ProjectedResolution> = BTreeMap::new();
      for unit in &group.documents {
        for site in unit.document.occurrences() {
          for location in self
            .project_locations(&site.location)
            .unwrap_or_default()
            .into_iter()
            .filter(|location| {
              location.snapshot == snapshot
                && offset.is_none_or(|offset| location.bytes.contains(&offset))
            })
          {
            let resolution = ProjectedResolution::new(
              site,
              syntax_resolution(&unit.document, site, analysis),
            );
            projected
              .entry((location.bytes.start, location.bytes.end))
              .and_modify(|old| old.join(&resolution))
              .or_insert(resolution);
          }
        }
      }
      for (range, mut projected) in projected {
        if !group.complete || projected.documents.len() != group.documents.len()
        {
          projected.resolution = None;
        }
        result
          .entry(range)
          .and_modify(|old| old.join(&projected))
          .or_insert(projected);
      }
    }
    result
  }
}

struct ProjectedDocuments<'a> {
  documents: Vec<&'a DerivedUnit>,
  complete: bool,
}

struct ProjectedSpan {
  span: engine::SemanticSpan,
  documents: BTreeSet<DocumentId>,
  compatible: bool,
}

impl ProjectedSpan {
  fn new(span: engine::SemanticSpan, document: DocumentId) -> Self {
    Self {
      span,
      documents: BTreeSet::from([document]),
      compatible: true,
    }
  }

  fn join(&mut self, other: &engine::SemanticSpan, document: DocumentId) {
    self.documents.insert(document);
    if self.span.kind != other.kind {
      if matches!(
        self.span.kind,
        engine::SemanticKind::Function | engine::SemanticKind::Command
      ) && matches!(
        other.kind,
        engine::SemanticKind::Function | engine::SemanticKind::Command
      ) {
        self.span.kind = engine::SemanticKind::Command;
      } else {
        self.compatible = false;
      }
    }
    self
      .span
      .modifiers
      .retain(|modifier| other.modifiers.contains(modifier));
    self.span.readonly = self.span.readonly.join(other.readonly);
  }
}

pub(super) struct ProjectedResolution {
  kind: engine::OccurrenceKind,
  access: engine::Access,
  name: Value,
  documents: BTreeSet<DocumentId>,
  pub(super) resolution: Option<engine::Resolution>,
}

impl ProjectedResolution {
  fn new(site: &Occurrence, resolution: Option<engine::Resolution>) -> Self {
    Self {
      kind: site.kind,
      access: site.access,
      name: site.name.clone(),
      documents: [site.id.snapshot.document].into(),
      resolution,
    }
  }

  fn join(&mut self, other: &Self) {
    self.documents.extend(other.documents.iter().copied());
    if self.kind != other.kind
      || self.access != other.access
      || self.name != other.name
    {
      self.resolution = None;
    } else {
      join_resolution(&mut self.resolution, other.resolution.as_ref());
    }
  }
}

fn join_resolution(
  left: &mut Option<engine::Resolution>,
  right: Option<&engine::Resolution>,
) {
  match (left.as_mut(), right) {
    (Some(left), Some(right)) => left.join(right),
    _ => *left = None,
  }
}

pub(super) fn syntax_spelling(
  document: &super::Document,
  site: &Occurrence,
) -> Vec<Location> {
  if site.kind != engine::OccurrenceKind::Command {
    return vec![site.location.clone()];
  }
  if !matches!(site.name, Value::Literal(_)) {
    return Vec::new();
  }
  let Some(node) = document.tree.root_node().named_descendant_for_byte_range(
    site.location.bytes.start,
    site.location.bytes.end,
  ) else {
    return Vec::new();
  };
  engine::word::literal(node, document.source().as_bytes())
    .fragments
    .into_iter()
    .filter_map(|fragment| fragment.source)
    .map(|bytes| Location {
      snapshot: site.location.snapshot,
      bytes,
    })
    .collect()
}

fn argument_resolution(
  analysis: &Analysis,
  role: &engine::ArgumentRole,
) -> Option<engine::Resolution> {
  let site = engine::UseSite::argument(&role.location);
  match &role.kind {
    Some(engine::ArgumentKind::Variable {
      access: engine::Access::Write,
      ..
    }) => {
      let candidates =
        analysis.written_definitions(site).collect::<BTreeSet<_>>();
      (!candidates.is_empty()).then_some(engine::Resolution {
        candidates,
        ..Default::default()
      })
    }
    Some(
      engine::ArgumentKind::Variable {
        access: engine::Access::Unknown,
        ..
      }
      | engine::ArgumentKind::Number,
    )
    | None => None,
    _ => analysis.summarize(site).resolution,
  }
}

pub(in crate::interface::application::engine) fn syntax_resolution(
  document: &super::Document,
  site: &Occurrence,
  analysis: Option<&Analysis>,
) -> Option<engine::Resolution> {
  if matches!(
    site.access,
    engine::Access::Definition | engine::Access::Write
  ) {
    document
      .symbols()
      .iter()
      .find(|symbol| symbol.definition == site.id)
      .map(|symbol| engine::Resolution {
        candidates: [symbol.id].into(),
        ..Default::default()
      })
  } else {
    (site.kind == engine::OccurrenceKind::Command)
      .then(|| engine::analysis::invariant_command(&site.name))
      .flatten()
      .or_else(|| {
        let use_site = engine::UseSite::Syntax(site.id);
        if let Some(analysis) = analysis
          && analysis.resolutions(use_site).next().is_some()
        {
          analysis.summarize(use_site).resolution
        } else {
          document.deferred_resolution(use_site)
        }
      })
  }
}

#[cfg(test)]
#[path = "../../../../../tests/implementation/derived.rs"]
mod tests;
