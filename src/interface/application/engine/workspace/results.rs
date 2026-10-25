use super::super::analysis::{
  argument_resolution, join_resolution, syntax_attempted, syntax_resolution,
};
use super::names::syntax_spelling;
use super::{Document, Workspace, projection};
use crate::interface::application::engine::{
  self, Alternative, DependencyState, DocumentId, Error, Location, ReferenceId,
  Resolution, ResolutionIssue, SemanticSpan, Snapshot, Value,
};
use std::collections::{BTreeMap, BTreeSet};

/// A view of independent entry analyses. IDs within an entry stay local to it.
pub struct AnalysisResults<'a> {
  pub(super) entries: Vec<&'a Workspace>,
}

#[derive(Clone, Debug, Default)]
pub struct DefinitionResolution {
  pub definitions: Vec<Option<Location>>,
  pub alternatives: BTreeSet<Alternative>,
  pub issues: BTreeSet<ResolutionIssue>,
  pub compatible: bool,
}

pub(super) struct ResolutionObservation {
  pub resolution: Option<Resolution>,
  pub attempted: bool,
  pub projected: bool,
}

#[derive(Clone, Debug)]
pub struct SourceResolution {
  pub location: Location,
  pub targets: BTreeSet<Snapshot>,
  pub complete: bool,
}

impl<'a> AnalysisResults<'a> {
  pub fn new(entries: impl IntoIterator<Item = &'a Workspace>) -> Self {
    Self {
      entries: entries.into_iter().collect(),
    }
  }

  pub(super) fn document(
    &self,
    snapshot: Snapshot,
  ) -> Result<&'a Document, Error> {
    self
      .entries
      .iter()
      .find_map(|workspace| {
        workspace
          .document(snapshot.document)
          .filter(|document| document.snapshot() == snapshot)
      })
      .ok_or(Error::UnknownDocument(snapshot.document))
  }

  pub(super) fn workspaces(&self, snapshot: Snapshot) -> Vec<&'a Workspace> {
    self
      .entries
      .iter()
      .copied()
      .filter(|workspace| {
        workspace
          .document(snapshot.document)
          .is_some_and(|document| document.snapshot() == snapshot)
      })
      .collect()
  }

  /// Dependency edges for original documents; derived IDs belong to each entry.
  #[must_use]
  pub fn dependencies(&self) -> BTreeMap<ReferenceId, DependencyState> {
    let mut states: BTreeMap<_, DependencyState> = BTreeMap::new();
    for workspace in &self.entries {
      let Some(analysis) = workspace.analysis() else {
        continue;
      };
      for (reference, _, _) in analysis.source_inputs() {
        if workspace
          .derived_origin(reference.snapshot.document)
          .is_some()
        {
          continue;
        }
        if let Some(state) = analysis.source_bindings().get(&reference) {
          states
            .entry(reference)
            .and_modify(|old| {
              if old != state {
                *old = DependencyState::Unresolved(
                  "Source target differs between entries".into(),
                );
              }
            })
            .or_insert_with(|| state.clone());
        }
      }
    }
    for reference in self
      .entries
      .iter()
      .filter_map(|workspace| workspace.analysis())
      .flat_map(|analysis| &analysis.non_sources)
    {
      if let Some(state) = states.get_mut(reference) {
        *state = DependencyState::Unresolved(
          "Source command differs between entries".into(),
        );
      }
    }
    states
  }

  pub fn semantic_spans(
    &self,
    snapshot: Snapshot,
  ) -> Result<Vec<SemanticSpan>, Error> {
    let document = self.document(snapshot)?;
    let workspaces = self.workspaces(snapshot);
    let inputs: Vec<_> = workspaces
      .iter()
      .filter_map(|workspace| {
        let analysis = workspace.analysis()?;
        analysis
          .snapshots()
          .contains(&snapshot)
          .then_some((document, analysis))
      })
      .collect();
    let mut spans = document.semantic_spans(&inputs);
    spans.extend(projection::projected_semantic_spans(document, &workspaces));
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

  pub fn resolution_at(
    &self,
    snapshot: Snapshot,
    offset: usize,
  ) -> Result<Option<DefinitionResolution>, Error> {
    let document = self.document(snapshot)?;
    if !document.source().is_char_boundary(offset) {
      return Err(Error::InvalidPosition);
    }
    let workspaces = self.workspaces(snapshot);
    let blocked = projection::blocked_projections(document, &workspaces);
    let mut observations = Vec::new();
    for workspace in &workspaces {
      if let Some(observation) =
        workspace.resolution_observation_at(snapshot, offset)?
      {
        observations.push((*workspace, observation));
      }
    }
    if observations.is_empty() {
      return Ok(None);
    }
    let attempted = observations
      .iter()
      .any(|(_, observation)| observation.attempted);
    let mut result = DefinitionResolution {
      compatible: true,
      ..Default::default()
    };
    for (workspace, observation) in observations {
      if attempted && !observation.attempted {
        continue;
      }
      if observation.projected
        && blocked.iter().any(|range| range.contains(&offset))
      {
        result.compatible = false;
      }
      let Some(resolution) = observation.resolution else {
        result.compatible = false;
        continue;
      };
      result.alternatives.extend(resolution.alternatives);
      result.issues.extend(resolution.issues);
      for symbol in resolution.candidates {
        let location = workspace
          .symbol(symbol)
          .and_then(|symbol| workspace.definition_location(symbol));
        result.definitions.push(location);
      }
    }
    result.definitions.sort_by_key(|location| {
      location.as_ref().map(|location| {
        (location.snapshot, location.bytes.start, location.bytes.end)
      })
    });
    result.definitions.dedup();
    Ok(Some(result))
  }

  pub fn sources(
    &self,
    snapshot: Snapshot,
  ) -> Result<Vec<SourceResolution>, Error> {
    let document = self.document(snapshot)?;
    let workspaces = self.workspaces(snapshot);
    let mut blocked = projection::blocked_projections(document, &workspaces);
    let mut sources: BTreeMap<_, SourceResolution> = BTreeMap::new();
    for workspace in workspaces {
      if let Some(analysis) = workspace.analysis() {
        for reference in analysis.non_sources.iter().copied().chain(
          analysis
            .source_inputs()
            .filter(|(reference, _, _)| {
              analysis.source_operand(*reference).is_none()
            })
            .map(|(reference, _, _)| reference),
        ) {
          if let Some(location) = workspace
            .document(reference.snapshot.document)
            .and_then(|document| document.reference(reference))
            .and_then(|reference| {
              workspace.navigation_location(&reference.command)
            })
            .filter(|location| location.snapshot == snapshot)
          {
            blocked.push(location.bytes);
          }
        }
      }
      for (location, target) in workspace.source_observations(snapshot.document)
      {
        let result = sources
          .entry((location.bytes.start, location.bytes.end))
          .or_insert_with(|| SourceResolution {
            location,
            targets: BTreeSet::new(),
            complete: true,
          });
        if let Some(target) = target {
          result.targets.insert(target);
        } else {
          result.complete = false;
        }
      }
    }
    for result in sources.values_mut() {
      result.complete &= !blocked.iter().any(|range| {
        range.start <= result.location.bytes.start
          && result.location.bytes.end <= range.end
      });
    }
    Ok(sources.into_values().collect())
  }
}

impl Workspace {
  pub fn semantic_spans(
    &self,
    id: DocumentId,
  ) -> Result<Vec<SemanticSpan>, Error> {
    self.semantic_spans_with(id, &[])
  }

  pub fn semantic_spans_with(
    &self,
    id: DocumentId,
    entries: &[&Self],
  ) -> Result<Vec<SemanticSpan>, Error> {
    let snapshot = self
      .document(id)
      .ok_or(Error::UnknownDocument(id))?
      .snapshot();
    AnalysisResults::new(std::iter::once(self).chain(entries.iter().copied()))
      .semantic_spans(snapshot)
  }

  /// Locations are projected to the requested document.
  #[must_use]
  pub fn source_links(&self, id: DocumentId) -> Vec<(Location, Snapshot)> {
    self
      .source_observations(id)
      .into_iter()
      .filter_map(|(location, target)| target.map(|target| (location, target)))
      .collect()
  }

  pub(super) fn source_observations(
    &self,
    id: DocumentId,
  ) -> Vec<(Location, Option<Snapshot>)> {
    let Some(analysis) = self.analysis() else {
      return Vec::new();
    };
    let mut inputs: BTreeMap<_, engine::SourceInput> = BTreeMap::new();
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
      .map(|((snapshot, start, end), target)| {
        (
          Location {
            snapshot,
            bytes: start..end,
          },
          target,
        )
      })
      .collect()
  }

  pub fn resolution_at(
    &self,
    snapshot: Snapshot,
    offset: usize,
  ) -> Result<Option<engine::Resolution>, Error> {
    Ok(
      self
        .resolution_observation_at(snapshot, offset)?
        .and_then(|observation| observation.resolution),
    )
  }

  pub(super) fn resolution_observation_at(
    &self,
    snapshot: Snapshot,
    offset: usize,
  ) -> Result<Option<ResolutionObservation>, Error> {
    let document = self.check_current(snapshot)?;
    if !document.source().is_char_boundary(offset) {
      return Err(Error::InvalidPosition);
    }
    if let Some(resolution) = self.projected_resolution_at(snapshot, offset) {
      return Ok(Some(ResolutionObservation {
        resolution: resolution.resolution,
        attempted: resolution.attempted,
        projected: true,
      }));
    }
    let analysis = self.analysis();
    let mut selected: Option<((usize, u8), ResolutionObservation)> = None;
    let mut consider = |width: usize,
                        priority: u8,
                        resolution: Option<engine::Resolution>,
                        attempted: bool| {
      let key = (width, priority);
      match &mut selected {
        Some((previous, _)) if *previous < key => {}
        Some((previous, result)) if *previous == key => {
          join_resolution(&mut result.resolution, resolution.as_ref());
          result.attempted |= attempted;
        }
        _ => {
          selected = Some((
            key,
            ResolutionObservation {
              resolution,
              attempted,
              projected: false,
            },
          ));
        }
      }
    };
    for site in document.occurrences() {
      if self
        .project_locations_at([&site.location], snapshot, Some(offset))
        .next()
        .is_none()
      {
        continue;
      }
      let width = self
        .project_locations_at(
          &syntax_spelling(document, site),
          snapshot,
          Some(offset),
        )
        .map(|location| location.bytes.len())
        .min();
      if let Some(width) = width {
        let resolution = syntax_resolution(document, site, analysis);
        let attempted = syntax_attempted(site, analysis, resolution.as_ref());
        consider(width, 0, resolution, attempted);
      }
    }
    for role in document.argument_summaries(analysis).into_values() {
      let width = self
        .project_locations_at(
          role.selection_locations(),
          snapshot,
          Some(offset),
        )
        .map(|location| location.bytes.len())
        .min();
      if let Some(width) = width {
        consider(
          width,
          1,
          argument_resolution(analysis, &role),
          role.context.is_some(),
        );
      }
    }
    Ok(selected.map(|(_, resolution)| resolution))
  }
}
