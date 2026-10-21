use super::{Document, Workspace};
use crate::interface::application::engine::{
  self, Analysis, DocumentId, Error, Location, OccurrenceId, Revision,
  Snapshot, Value, check_cancelled,
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
  pub(super) root: Snapshot,
  pub(super) program: Option<Arc<[u8]>>,
  pub(super) range: (usize, usize),
  pub(super) argument: (usize, usize),
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

pub(in crate::interface::application::engine) const fn reserved(
  id: DocumentId,
) -> bool {
  id.0 & (1 << 63) != 0
}

impl Workspace {
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

  /// Keep unretained derived documents: an active analysis may still use them.
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
}

#[cfg(test)]
#[path = "../../../../../tests/implementation/derived.rs"]
mod tests;
