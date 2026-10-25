use super::super::analysis::{
  argument_resolution, join_resolution, syntax_attempted, syntax_resolution,
};
use super::derived::{DerivedKey, DerivedSite, DerivedUnit};
use super::names::syntax_spelling;
use super::{Document, Workspace};
use crate::interface::application::engine::{
  self, Analysis, DocumentId, Location, Occurrence, Snapshot, Value,
};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

impl Workspace {
  pub(super) fn projections(
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

  pub(super) fn projected_resolution_at(
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
    let mut result = BTreeMap::new();
    for group in self.projections(snapshot, analysis) {
      let Some(first) = group.documents.first() else {
        continue;
      };
      let site = first.key.site.clone();
      let mut projected: BTreeMap<_, ProjectedResolution> = BTreeMap::new();
      for unit in &group.documents {
        for (range, resolution) in
          self.project_document_resolutions(&unit.document, snapshot, offset)
        {
          projected
            .entry(range)
            .and_modify(|old| old.join(&resolution))
            .or_insert(resolution);
        }
      }
      for (range, mut projected) in projected {
        if !group.complete || projected.documents.len() != group.documents.len()
        {
          projected.resolution = None;
        }
        insert_nested(&mut result, range, &site, projected, |old, new| {
          old.join(&new);
        });
      }
    }
    result
      .into_iter()
      .map(|(range, (_, projected))| (range, projected))
      .collect()
  }

  fn project_document_resolutions(
    &self,
    document: &Document,
    snapshot: Snapshot,
    offset: Option<usize>,
  ) -> BTreeMap<(usize, usize), ProjectedResolution> {
    let analysis = self.analysis();
    let mut resolutions: BTreeMap<_, ProjectedResolution> = BTreeMap::new();
    for site in document.occurrences() {
      for location in self.project_locations_at(
        &syntax_spelling(document, site),
        snapshot,
        offset,
      ) {
        let resolution = ProjectedResolution::syntax(document, site, analysis);
        resolutions
          .entry((location.bytes.start, location.bytes.end))
          .and_modify(|old| old.join(&resolution))
          .or_insert(resolution);
      }
    }
    for role in document
      .argument_summaries(analysis)
      .into_values()
      .filter(|role| role.kind != Some(engine::ArgumentKind::ShellBody))
    {
      for location in
        self.project_locations_at(role.selection_locations(), snapshot, offset)
      {
        resolutions
          .entry((location.bytes.start, location.bytes.end))
          .or_insert_with(|| ProjectedResolution::argument(&role, analysis));
      }
    }
    resolutions
  }

  pub(super) fn project_locations_at<'l>(
    &self,
    locations: impl IntoIterator<Item = &'l Location>,
    snapshot: Snapshot,
    offset: Option<usize>,
  ) -> impl Iterator<Item = Location> {
    locations
      .into_iter()
      .flat_map(|location| self.project_locations(location).unwrap_or_default())
      .filter(move |location| {
        location.snapshot == snapshot
          && offset.is_none_or(|offset| location.bytes.contains(&offset))
      })
  }
}

pub(super) struct ProjectedDocuments<'a> {
  pub(super) documents: Vec<&'a DerivedUnit>,
  pub(super) complete: bool,
}

/// A unit derived from another unit's text supersedes the names it was
/// expanded from, as every unit supersedes the syntax of its source.
fn insert_nested<T>(
  entries: &mut BTreeMap<(usize, usize), (usize, T)>,
  range: (usize, usize),
  site: &DerivedSite,
  value: T,
  join: impl FnOnce(&mut T, T),
) {
  let width = site.argument.1 - site.argument.0;
  match entries.entry(range) {
    Entry::Occupied(mut entry) => {
      let (previous, old) = entry.get_mut();
      if width < *previous {
        *entry.get_mut() = (width, value);
      } else if width == *previous {
        join(old, value);
      }
    }
    Entry::Vacant(entry) => {
      entry.insert((width, value));
    }
  }
}

pub(super) fn projected_semantic_spans(
  document: &Document,
  workspaces: &[&Workspace],
) -> Vec<engine::SemanticSpan> {
  let mut spans = BTreeMap::new();
  for (site, units) in projection_groups(document, workspaces) {
    for span in projected_units(document, units) {
      let range = (span.location.bytes.start, span.location.bytes.end);
      insert_nested(&mut spans, range, &site, vec![span], Vec::extend);
    }
  }
  spans.into_values().flat_map(|(_, spans)| spans).collect()
}

type ProjectionUnits<'a> =
  BTreeMap<&'a DerivedKey, Vec<(&'a Workspace, &'a DerivedUnit)>>;

fn projection_groups<'a>(
  document: &Document,
  workspaces: &[&'a Workspace],
) -> Vec<(DerivedSite, ProjectionUnits<'a>)> {
  let mut output = Vec::new();
  let mut groups: BTreeMap<_, Vec<_>> = BTreeMap::new();
  for workspace in workspaces {
    for group in
      workspace.projections(document.snapshot(), workspace.analysis())
    {
      let Some(first) = group.documents.first() else {
        continue;
      };
      let attempted = workspace.analysis().is_some_and(|analysis| {
        analysis
          .argument_summary(engine::UseSite::argument(&first.origin.argument))
          .is_some()
          || group.documents.iter().any(|unit| {
            analysis.contexts().iter().any(|context| {
              context.location.snapshot == unit.document.snapshot()
                || matches!(
                  context.kind,
                  engine::ExecutionKind::Deferred(snapshot)
                    | engine::ExecutionKind::Arithmetic(snapshot)
                    if snapshot == unit.document.snapshot()
                )
            })
          })
      });
      groups
        .entry(first.key.site.clone())
        .or_default()
        .push((*workspace, group, attempted));
    }
  }
  let blocked: Vec<_> = groups
    .iter()
    .filter(|(site, groups)| {
      let attempted = groups.iter().any(|(_, _, attempted)| *attempted);
      conflicting_body_roles(site, workspaces)
        || groups.iter().any(|(_, group, reached)| {
          (!attempted || *reached) && !group.complete
        })
    })
    .map(|(site, _)| site)
    .cloned()
    .collect();
  for (site, groups) in groups {
    let attempted = groups.iter().any(|(_, _, attempted)| *attempted);
    if blocked.iter().any(|parent| {
      parent.root == site.root
        && parent.argument.0 <= site.argument.0
        && site.argument.1 <= parent.argument.1
    }) {
      continue;
    }
    let groups: Vec<_> = groups
      .into_iter()
      .filter(|(_, _, reached)| !attempted || *reached)
      .collect();
    let mut units: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (workspace, group, _) in groups {
      for unit in group.documents {
        units.entry(&unit.key).or_default().push((workspace, unit));
      }
    }
    output.push((site, units));
  }
  output
}

pub(super) fn blocked_projections(
  document: &Document,
  workspaces: &[&Workspace],
) -> Vec<Range<usize>> {
  let accepted: BTreeSet<_> = projection_groups(document, workspaces)
    .into_iter()
    .map(|(site, _)| site)
    .collect();
  workspaces
    .iter()
    .flat_map(|workspace| {
      workspace.projections(document.snapshot(), workspace.analysis())
    })
    .filter_map(|group| {
      let site = &group.documents.first()?.key.site;
      (!accepted.contains(site)).then_some(site.argument.0..site.argument.1)
    })
    .collect()
}

fn projected_units(
  document: &Document,
  units: ProjectionUnits<'_>,
) -> Vec<engine::SemanticSpan> {
  let mut spans = Vec::new();
  let count = units.len();
  let mut projected: BTreeMap<_, ProjectedSpan> = BTreeMap::new();
  for (index, units) in units.into_values().enumerate() {
    let (workspace, unit) = units[0];
    let inputs: Vec<_> = units
      .iter()
      .filter_map(|(workspace, unit)| {
        let analysis = workspace.analysis()?;
        analysis
          .snapshots()
          .contains(&unit.document.snapshot())
          .then_some((unit.document.as_ref(), analysis))
      })
      .collect();
    for mut span in unit.document.semantic_spans(&inputs) {
      let Some(locations) = workspace.project_locations(&span.location) else {
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
          .and_modify(|old| old.join(&span, index))
          .or_insert_with(|| ProjectedSpan::new(span.clone(), index));
      }
    }
  }
  for projected in projected.into_values() {
    if projected.compatible && projected.documents.len() == count {
      spans.push(projected.span);
    }
  }
  spans
}

fn conflicting_body_roles(
  site: &DerivedSite,
  workspaces: &[&Workspace],
) -> bool {
  let mut body = false;
  let mut incomplete = false;
  for workspace in workspaces {
    let Some(analysis) = workspace.analysis() else {
      continue;
    };
    for location in body_arguments(site, workspace) {
      let Some(role) =
        analysis.argument_summary(engine::UseSite::argument(&location))
      else {
        continue;
      };
      if role.kind != Some(engine::ArgumentKind::ShellBody) {
        incomplete = true;
        continue;
      }
      body = true;
      let document = workspace
        .document(location.snapshot.document)
        .expect("body document");
      let Some(command) = document.command_at(&location) else {
        incomplete = true;
        continue;
      };
      let state =
        analysis
          .derived
          .bodies
          .iter()
          .find_map(|(registration, body)| {
            let occurrence = workspace.occurrence(*registration)?;
            (registration.snapshot == location.snapshot
              && document.command_at(&occurrence.location) == Some(command))
            .then_some(body)
          });
      incomplete |= state.is_none_or(|state| {
        state.incomplete || state.conditional || state.documents.is_empty()
      });
    }
  }
  body && incomplete
}

fn body_arguments(site: &DerivedSite, workspace: &Workspace) -> Vec<Location> {
  let Some(program) = &site.program else {
    return vec![Location {
      snapshot: site.root,
      bytes: site.argument.0..site.argument.1,
    }];
  };
  workspace
    .derived
    .values()
    .filter(|unit| {
      unit.key.site.root == site.root
        && unit.key.body == *program
        && unit.origin.project(site.range.0..site.range.1).is_some_and(
          |locations| {
            locations
              .first()
              .is_some_and(|location| location.bytes.start == site.argument.0)
              && locations
                .last()
                .is_some_and(|location| location.bytes.end == site.argument.1)
          },
        )
    })
    .map(|unit| Location {
      snapshot: unit.document.snapshot(),
      bytes: site.range.0..site.range.1,
    })
    .collect()
}

struct ProjectedSpan {
  span: engine::SemanticSpan,
  documents: BTreeSet<usize>,
  compatible: bool,
}

impl ProjectedSpan {
  fn new(span: engine::SemanticSpan, document: usize) -> Self {
    Self {
      span,
      documents: BTreeSet::from([document]),
      compatible: true,
    }
  }

  fn join(&mut self, other: &engine::SemanticSpan, document: usize) {
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

#[derive(PartialEq)]
enum ProjectedKind {
  Syntax(engine::OccurrenceKind, engine::Access, Value),
  Variable(Vec<u8>, engine::Access),
  Function(Vec<u8>),
  Command(Value),
  Other,
}

pub(super) struct ProjectedResolution {
  kind: ProjectedKind,
  documents: BTreeSet<DocumentId>,
  pub(super) resolution: Option<engine::Resolution>,
  pub(super) attempted: bool,
}

impl ProjectedResolution {
  fn syntax(
    document: &Document,
    site: &Occurrence,
    analysis: Option<&Analysis>,
  ) -> Self {
    let resolution = syntax_resolution(document, site, analysis);
    let attempted = syntax_attempted(site, analysis, resolution.as_ref());
    Self {
      kind: ProjectedKind::Syntax(site.kind, site.access, site.name.clone()),
      documents: [site.id.snapshot.document].into(),
      resolution,
      attempted,
    }
  }

  fn argument(
    role: &engine::ArgumentRole,
    analysis: Option<&Analysis>,
  ) -> Self {
    let kind = match &role.kind {
      Some(engine::ArgumentKind::Variable { name, access, .. }) => {
        ProjectedKind::Variable(name.clone(), *access)
      }
      Some(engine::ArgumentKind::Function { name }) => {
        ProjectedKind::Function(name.clone())
      }
      Some(engine::ArgumentKind::Command { name }) => {
        ProjectedKind::Command(name.clone())
      }
      _ => ProjectedKind::Other,
    };
    Self {
      kind,
      documents: [role.location.snapshot.document].into(),
      resolution: argument_resolution(analysis, role),
      attempted: role.context.is_some(),
    }
  }

  pub(super) fn derived_from(&self, document: DocumentId) -> bool {
    self.documents.contains(&document)
  }

  fn join(&mut self, other: &Self) {
    self.documents.extend(other.documents.iter().copied());
    self.attempted |= other.attempted;
    if self.kind == other.kind {
      join_resolution(&mut self.resolution, other.resolution.as_ref());
    } else {
      self.resolution = None;
    }
  }
}
