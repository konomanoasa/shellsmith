use super::engine::{
  self, Access, Alternative, EffectTarget, ExecutionContextId, Location, Name,
  NameEnvironment, NameId, ResolutionIssue, SymbolId, SymbolKind,
  special_builtin, valid_name,
};
use super::support::{EntryNames, NameCollection};
use super::{Application, Error, check};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenameTarget {
  pub location: Location,
  pub placeholder: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenameEdit {
  pub location: Location,
  pub new_text: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SourceRange {
  path: PathBuf,
  start: usize,
  end: usize,
}

type Site = (usize, usize);
type Member = (usize, NameId);

struct Selection {
  target: RenameTarget,
  kind: SymbolKind,
  sites: BTreeSet<Site>,
  members: BTreeSet<Member>,
}

fn refused(message: &str) -> Error {
  Error::RenameRefused(message.into())
}

impl Application {
  pub fn prepare_rename(
    &mut self,
    path: &Path,
    offset: usize,
    cancelled: &AtomicBool,
  ) -> Result<Option<RenameTarget>, Error> {
    self.verify_collection(cancelled)?;
    let names = self.prepare_names(path, cancelled)?;
    self.check_name_inputs(&names, cancelled)?;
    self
      .rename_selection(&names, path, offset, cancelled)
      .map(|selection| selection.map(|selection| selection.target))
  }

  pub fn rename(
    &mut self,
    path: &Path,
    offset: usize,
    new_name: &str,
    cancelled: &AtomicBool,
  ) -> Result<Vec<RenameEdit>, Error> {
    self.verify_collection(cancelled)?;
    let names = self.prepare_names(path, cancelled)?;
    self.check_name_inputs(&names, cancelled)?;
    let selection = self
      .rename_selection(&names, path, offset, cancelled)?
      .ok_or_else(|| {
        refused("No editable variable or function at this position")
      })?;
    if !valid_name(new_name.as_bytes())
      || reserved(selection.kind, new_name.as_bytes())
    {
      return Err(Error::InvalidName("The new name must be a portable shell name without special shell meaning".into()));
    }
    if selection.target.placeholder == new_name {
      return Ok(Vec::new());
    }
    let mut edits = BTreeMap::new();
    for &(entry, site) in &selection.sites {
      check(cancelled)?;
      let location = names.entries[entry].relations.occurrences[site]
        .spelling
        .editable()
        .ok_or_else(|| {
          refused("A related name has no editable source spelling")
        })?;
      edits
        .entry(source_range(&names, location)?)
        .or_insert_with(|| RenameEdit {
          location: location.clone(),
          new_text: new_name.into(),
        });
    }
    let mapping = SourceMap::new(&edits)?;
    let mut changed = BTreeMap::new();
    for (range, edit) in edits.iter().rev() {
      check(cancelled)?;
      let text = changed.entry(range.path.clone()).or_insert_with(|| {
        names.documents[&edit.location.snapshot].1.to_string()
      });
      text.replace_range(range.start..range.end, new_name);
    }
    let after = self.reanalyze_names(&names, &changed, cancelled)?;
    verify(&names, &after, &selection, &mapping, new_name, cancelled)?;
    self.check_name_inputs(&names, cancelled)?;
    Ok(edits.into_values().collect())
  }

  fn rename_selection(
    &self,
    names: &NameCollection,
    path: &Path,
    offset: usize,
    cancelled: &AtomicBool,
  ) -> Result<Option<Selection>, Error> {
    let path = self.absolute(path);
    let id = self
      .document_id(&path)
      .ok_or_else(|| refused("The requested document is unavailable"))?;
    let document = self
      .workspace
      .document(id)
      .ok_or(engine::Error::UnknownDocument(id))?;
    if offset > document.source().len()
      || !document.source().is_char_boundary(offset)
    {
      return Err(engine::Error::InvalidPosition.into());
    }
    let mut candidates = Vec::new();
    let mut width = usize::MAX;
    for (entry, result) in names.entries.iter().enumerate() {
      for (index, site) in result.relations.at(document.snapshot(), offset) {
        if site.names.is_empty() {
          continue;
        }
        let location = site.spelling.editable().expect("selected spelling");
        if location.bytes.len() < width {
          candidates.clear();
          width = location.bytes.len();
        }
        if location.bytes.len() == width {
          candidates.push((entry, index));
        }
      }
    }
    let Some(&(entry, index)) = candidates.first() else {
      return Ok(None);
    };
    let first = &names.entries[entry].relations.occurrences[index];
    let name = &names.entries[entry].relations.names
      [first.names.first().expect("selected name").0];
    if !valid_name(&name.text) || reserved(name.kind, &name.text) {
      return Ok(None);
    }
    let target = RenameTarget {
      location: first
        .spelling
        .editable()
        .expect("selected spelling")
        .clone(),
      placeholder: String::from_utf8(name.text.clone()).expect("portable name"),
    };
    let mut selection = Selection {
      target,
      kind: name.kind,
      sites: BTreeSet::new(),
      members: BTreeSet::new(),
    };
    selection.expand(names, candidates, cancelled)?;
    if !selection.members.iter().any(|(entry, id)| {
      !names.entries[*entry].relations.names[id.0]
        .definitions
        .is_empty()
    }) {
      return Ok(None);
    }
    selection.validate(names)?;
    Ok(Some(selection))
  }
}

impl Selection {
  fn expand(
    &mut self,
    names: &NameCollection,
    candidates: Vec<Site>,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let mut by_name: BTreeMap<Member, Vec<Site>> = BTreeMap::new();
    let mut by_source: BTreeMap<SourceRange, Vec<Site>> = BTreeMap::new();
    for (entry, result) in names.entries.iter().enumerate() {
      for (index, site) in result.relations.occurrences.iter().enumerate() {
        check(cancelled)?;
        for id in &site.names {
          by_name
            .entry((entry, *id))
            .or_default()
            .push((entry, index));
        }
        for location in
          std::iter::once(&site.location).chain(site.spelling.editable())
        {
          if let Ok(range) = source_range(names, location) {
            by_source.entry(range).or_default().push((entry, index));
          }
        }
      }
    }
    let mut expanded_sources = BTreeSet::new();
    let mut pending: VecDeque<_> = candidates.into();
    while let Some((entry, index)) = pending.pop_front() {
      check(cancelled)?;
      if !self.sites.insert((entry, index)) {
        continue;
      }
      let site = &names.entries[entry].relations.occurrences[index];
      for id in &site.names {
        if self.members.insert((entry, *id)) {
          pending
            .extend(by_name.get(&(entry, *id)).into_iter().flatten().copied());
        }
      }
      for location in
        std::iter::once(&site.location).chain(site.spelling.editable())
      {
        if let Ok(range) = source_range(names, location)
          && expanded_sources.insert(range.clone())
        {
          pending.extend(by_source.get(&range).into_iter().flatten().copied());
        }
      }
    }
    Ok(())
  }

  fn validate(&self, collection: &NameCollection) -> Result<(), Error> {
    for &(entry, index) in &self.sites {
      let result = &collection.entries[entry];
      let site = &result.relations.occurrences[index];
      if result.damaged {
        return Err(refused("Related input contains incomplete syntax"));
      }
      if !site.compatible
        || site.names.is_empty()
        || site.access == Access::Unknown
      {
        return Err(refused("A related occurrence has inconsistent roles"));
      }
      if site.spelling.editable().is_none() {
        return Err(refused(
          "A related name is generated or has no safe source replacement",
        ));
      }
      let resolution = site
        .resolution
        .as_ref()
        .ok_or_else(|| refused("A related name has not been resolved"))?;
      if !resolution.issues.is_empty()
        || resolution
          .alternatives
          .iter()
          .any(|alternative| *alternative != Alternative::Unset)
      {
        return Err(refused(
          "A related reference has unknown or external bindings",
        ));
      }
    }
    for &(entry, id) in &self.members {
      let result = &collection.entries[entry];
      let name = &result.relations.names[id.0];
      if matches!(name.environment, NameEnvironment::Caller(_)) {
        return Err(refused(
          "A related name depends on an unknown caller environment",
        ));
      }
      if name.kind != self.kind
        || name.text != self.target.placeholder.as_bytes()
      {
        return Err(refused(
          "The same source spelling denotes different names",
        ));
      }
      if uncertain_name(result, name) {
        return Err(refused(
          "An unknown effect may access or change this name",
        ));
      }
      if result.relations.gaps.iter().any(|gap| {
        result.relations.may_inherit(
          NameEnvironment::Execution(gap.context),
          name.environment,
        ) && matches!(
          gap.issue,
          ResolutionIssue::Recovery
            | ResolutionIssue::UndecodedSource
            | ResolutionIssue::Recursion
            | ResolutionIssue::UnsupportedControl
        )
      }) {
        return Err(refused(
          "Analysis of the related execution environment is incomplete",
        ));
      }
    }
    Ok(())
  }
}

fn uncertain_name(entry: &EntryNames, name: &Name) -> bool {
  entry.relations.effects.iter().any(|effect| {
    entry
      .relations
      .may_inherit(effect.environment, name.environment)
      && effect.targets.iter().any(|target| affects(target, name))
  }) || entry.relations.dependencies.iter().any(|dependency| {
    entry
      .relations
      .may_depend(dependency.environment, name.environment)
      && dependency
        .targets
        .iter()
        .any(|target| affects(target, name))
  })
}

fn affects(target: &EffectTarget, name: &Name) -> bool {
  match target {
    EffectTarget::Variable(text) => {
      name.kind == SymbolKind::Variable && name.text == *text
    }
    EffectTarget::Function(text) => {
      name.kind == SymbolKind::Function && name.text == *text
    }
    EffectTarget::VariablePattern(value) => {
      name.kind == SymbolKind::Variable && value.can_equal(&name.text)
    }
    EffectTarget::FunctionPattern(value) => {
      name.kind == SymbolKind::Function && value.can_equal(&name.text)
    }
    EffectTarget::Variables => name.kind == SymbolKind::Variable,
    EffectTarget::Functions => name.kind == SymbolKind::Function,
    _ => false,
  }
}

fn source_range(
  collection: &NameCollection,
  location: &Location,
) -> Result<SourceRange, Error> {
  let (path, source) = collection
    .documents
    .get(&location.snapshot)
    .ok_or_else(|| refused("A name has no current source document"))?;
  source
    .get(location.bytes.clone())
    .ok_or_else(|| refused("Invalid source spelling"))?;
  Ok(SourceRange {
    path: path.clone(),
    start: location.bytes.start,
    end: location.bytes.end,
  })
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Identity {
  kind: SymbolKind,
  text: Vec<u8>,
  environment: EnvironmentIdentity,
  temporary: Option<SourceRange>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum EnvironmentIdentity {
  Execution(ExecutionContextId),
  Caller(SourceRange),
  Isolated(SourceRange),
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Relationship {
  location: SourceRange,
  names: BTreeSet<Arc<Identity>>,
  candidates: BTreeSet<Option<SourceRange>>,
  alternatives: BTreeSet<Alternative>,
  issues: BTreeSet<ResolutionIssue>,
  attempted: bool,
  compatible: bool,
  access: Access,
  readonly: engine::AttributeState,
}

#[derive(Debug, Eq, PartialEq)]
struct Relationships {
  names: BTreeMap<Arc<Identity>, BTreeSet<Option<SourceRange>>>,
  uncertain: BTreeSet<Arc<Identity>>,
  sites: BTreeMap<Relationship, usize>,
}

struct Shift {
  old: std::ops::Range<usize>,
  new: std::ops::Range<usize>,
}

struct SourceMap(BTreeMap<PathBuf, Vec<Shift>>);

impl SourceMap {
  fn new(edits: &BTreeMap<SourceRange, RenameEdit>) -> Result<Self, Error> {
    let mut files: BTreeMap<PathBuf, Vec<Shift>> = BTreeMap::new();
    for (range, edit) in edits {
      let changes = files.entry(range.path.clone()).or_default();
      let start = if let Some(previous) = changes.last() {
        if previous.old.end > range.start {
          return Err(refused("Related edits overlap"));
        }
        range.start - previous.old.end + previous.new.end
      } else {
        range.start
      };
      changes.push(Shift {
        old: range.start..range.end,
        new: start..start + edit.new_text.len(),
      });
    }
    Ok(Self(files))
  }

  fn translate(&self, mut range: SourceRange) -> SourceRange {
    if let Some(changes) = self.0.get(&range.path) {
      let point = |offset: usize, end: bool| {
        let index = changes.partition_point(|change| change.old.end <= offset);
        if let Some(change) = changes.get(index)
          && change.old.start < offset
        {
          return if end {
            change.new.end
          } else {
            change.new.start
          };
        }
        if index == 0 {
          offset
        } else {
          let previous = &changes[index - 1];
          offset - previous.old.end + previous.new.end
        }
      };
      range.start = point(range.start, false);
      range.end = point(range.end, true);
    }
    range
  }
}

fn verify(
  before: &NameCollection,
  after: &NameCollection,
  selection: &Selection,
  mapping: &SourceMap,
  new_name: &str,
  cancelled: &AtomicBool,
) -> Result<(), Error> {
  if before.entries.len() != after.entries.len() {
    return Err(refused("The analysis entries changed after the edit"));
  }
  for (index, (old, new)) in
    before.entries.iter().zip(&after.entries).enumerate()
  {
    check(cancelled)?;
    if old.path != new.path {
      return Err(refused("The analysis entries changed after the edit"));
    }
    if new.damaged && !old.damaged || old.dependencies != new.dependencies {
      return Err(refused("The edit changes syntax or source dependencies"));
    }
    let previous = relationships(
      before,
      old,
      |id| selection.members.contains(&(index, id)).then_some(new_name),
      |range| mapping.translate(range),
      cancelled,
    )?;
    let current =
      relationships(after, new, |_| None, |range| range, cancelled)?;
    if previous != current {
      return Err(refused(
        "The new name would change bindings or capture another reference",
      ));
    }
  }
  Ok(())
}

fn relationships<'a>(
  collection: &NameCollection,
  entry: &EntryNames,
  renamed: impl Fn(NameId) -> Option<&'a str>,
  map: impl Fn(SourceRange) -> SourceRange,
  cancelled: &AtomicBool,
) -> Result<Relationships, Error> {
  let definition = |id: &SymbolId| -> Option<SourceRange> {
    entry
      .definitions
      .get(id)
      .and_then(Option::as_ref)
      .and_then(|location| source_range(collection, location).ok())
      .map(&map)
  };
  let mut result = Relationships {
    names: BTreeMap::new(),
    uncertain: BTreeSet::new(),
    sites: BTreeMap::new(),
  };
  let mut identities = Vec::new();
  for name in &entry.relations.names {
    check(cancelled)?;
    let identity = Arc::new(Identity {
      kind: name.kind,
      text: renamed(name.id)
        .map_or_else(|| name.text.clone(), |text| text.as_bytes().to_vec()),
      environment: match name.environment {
        NameEnvironment::Execution(id) => EnvironmentIdentity::Execution(id),
        NameEnvironment::Caller(id) | NameEnvironment::Isolated(id) => {
          let location = entry
            .relations
            .scope_location(id)
            .ok_or_else(|| refused("A name has no source environment"))?;
          let range = map(source_range(collection, location)?);
          if matches!(name.environment, NameEnvironment::Caller(_)) {
            EnvironmentIdentity::Caller(range)
          } else {
            EnvironmentIdentity::Isolated(range)
          }
        }
      },
      temporary: name
        .temporary
        .and_then(|_| name.definitions.iter().find_map(&definition)),
    });
    if result
      .names
      .insert(
        identity.clone(),
        name.definitions.iter().map(&definition).collect(),
      )
      .is_some()
    {
      return Err(refused(
        "The new name collides with an existing name in the same environment",
      ));
    }
    if uncertain_name(entry, name) {
      result.uncertain.insert(identity.clone());
    }
    identities.push(identity);
  }
  for site in &entry.relations.occurrences {
    check(cancelled)?;
    let location = map(source_range(
      collection,
      site.spelling.editable().unwrap_or(&site.location),
    )?);
    let resolution = site.resolution.clone().unwrap_or_default();
    let names = site
      .names
      .iter()
      .map(|id| identities[id.0].clone())
      .collect();
    let relationship = Relationship {
      location,
      names,
      candidates: resolution.candidates.iter().map(&definition).collect(),
      alternatives: resolution.alternatives,
      issues: resolution.issues,
      attempted: site.context.is_some(),
      compatible: site.compatible,
      access: site.access,
      readonly: site.readonly,
    };
    *result.sites.entry(relationship).or_default() += 1;
  }
  Ok(result)
}

fn reserved(kind: SymbolKind, name: &[u8]) -> bool {
  match kind {
    SymbolKind::Function => {
      special_builtin(name)
        || matches!(
          name,
          b"if"
            | b"then"
            | b"else"
            | b"elif"
            | b"fi"
            | b"do"
            | b"done"
            | b"case"
            | b"esac"
            | b"while"
            | b"until"
            | b"for"
            | b"in"
        )
    }
    SymbolKind::Variable => matches!(
      name,
      b"ENV"
        | b"HOME"
        | b"IFS"
        | b"LANG"
        | b"LC_ALL"
        | b"LC_COLLATE"
        | b"LC_CTYPE"
        | b"LC_MESSAGES"
        | b"LINENO"
        | b"NLSPATH"
        | b"PATH"
        | b"PPID"
        | b"PS1"
        | b"PS2"
        | b"PS4"
        | b"PWD"
        | b"OLDPWD"
        | b"CDPATH"
        | b"OPTIND"
        | b"OPTARG"
        | b"MAIL"
        | b"MAILCHECK"
        | b"MAILPATH"
        | b"HISTFILE"
        | b"HISTSIZE"
        | b"FCEDIT"
    ),
  }
}
