use super::{Document, Workspace, derived::syntax_spelling};
use crate::interface::application::engine::{
  self, Access, Analysis, AnalysisGap, ArgumentKind, AttributeState, ContextId,
  EffectTarget, Error, ExecutionContext, ExecutionContextId, Location,
  OccurrenceKind, Resolution, Snapshot, SymbolId, SymbolKind, UseSite, Value,
  check_cancelled,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

/// Identifies a name within one relationship result, independently of its definitions.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct NameId(pub usize);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum NameEnvironment {
  Execution(ExecutionContextId),
  Caller(ContextId),
  Isolated(ContextId),
}

#[derive(Clone, Debug)]
pub struct Name {
  pub id: NameId,
  pub kind: SymbolKind,
  pub text: Vec<u8>,
  pub environment: NameEnvironment,
  pub temporary: Option<UseSite>,
  pub definitions: BTreeSet<SymbolId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NameSpelling {
  Exact(Location),
  LiteralWord(Location),
  Fragments(Vec<Location>),
  Generated,
}

impl NameSpelling {
  #[must_use]
  pub fn editable(&self) -> Option<&Location> {
    match self {
      Self::Exact(location) | Self::LiteralWord(location) => Some(location),
      Self::Fragments(_) | Self::Generated => None,
    }
  }
}

#[derive(Clone, Debug)]
pub struct NameOccurrence {
  pub site: UseSite,
  pub location: Location,
  pub spelling: NameSpelling,
  pub context: Option<ExecutionContextId>,
  pub names: BTreeSet<NameId>,
  pub access: Access,
  pub definitions: BTreeSet<SymbolId>,
  pub resolution: Option<Resolution>,
  pub compatible: bool,
  pub readonly: AttributeState,
}

#[derive(Clone, Debug, Default)]
pub struct NameRelations {
  pub names: Vec<Name>,
  pub occurrences: Vec<NameOccurrence>,
  pub dependencies: Vec<NameDependency>,
  pub effects: Vec<NameEffect>,
  pub gaps: Vec<AnalysisGap>,
  contexts: Vec<ExecutionContext>,
  scopes: BTreeMap<ContextId, NameScope>,
}

#[derive(Clone, Debug)]
struct NameScope {
  location: Location,
  parent: Option<NameEnvironment>,
}

#[derive(Clone, Debug)]
pub struct NameDependency {
  pub environment: NameEnvironment,
  pub location: Location,
  pub targets: BTreeSet<EffectTarget>,
}

#[derive(Clone, Debug)]
pub struct NameEffect {
  pub environment: NameEnvironment,
  pub location: Location,
  pub targets: BTreeSet<EffectTarget>,
}

impl NameRelations {
  #[must_use]
  pub fn context(&self, id: ExecutionContextId) -> Option<&ExecutionContext> {
    self.contexts.get(id.0)
  }

  #[must_use]
  pub fn scope_location(&self, id: ContextId) -> Option<&Location> {
    self.scopes.get(&id).map(|scope| &scope.location)
  }

  fn isolated_parent(
    &self,
    environment: NameEnvironment,
  ) -> Option<NameEnvironment> {
    if let NameEnvironment::Isolated(id) = environment {
      self.scopes.get(&id).and_then(|scope| scope.parent)
    } else {
      None
    }
  }

  #[must_use]
  pub fn may_inherit(
    &self,
    from: NameEnvironment,
    target: NameEnvironment,
  ) -> bool {
    self.inherits(from, target, false)
  }

  #[must_use]
  pub fn may_depend(
    &self,
    from: NameEnvironment,
    target: NameEnvironment,
  ) -> bool {
    self.inherits(from, target, true)
  }

  fn inherits(
    &self,
    from: NameEnvironment,
    target: NameEnvironment,
    unknown_caller: bool,
  ) -> bool {
    let mut current = Some(from);
    while let Some(environment) = current {
      if environment == target {
        return true;
      }
      current = match environment {
        NameEnvironment::Execution(id) => {
          self.context(id).and_then(|context| {
            if NameEnvironment::Execution(context.environment) == target {
              Some(target)
            } else {
              context.parent.map(NameEnvironment::Execution)
            }
          })
        }
        NameEnvironment::Caller(_) => return unknown_caller,
        NameEnvironment::Isolated(_) => self.isolated_parent(environment),
      };
    }
    false
  }

  fn connect_definitions(
    &mut self,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let mut definitions: BTreeMap<SymbolId, BTreeSet<NameId>> = BTreeMap::new();
    for name in &self.names {
      for symbol in &name.definitions {
        definitions.entry(*symbol).or_default().insert(name.id);
      }
    }
    for site in &mut self.occurrences {
      check_cancelled(cancelled)?;
      if let Some(resolution) = &site.resolution {
        for symbol in &resolution.candidates {
          if let Some(names) = definitions.get(symbol) {
            site.names.extend(names);
          } else {
            site.compatible = false;
          }
        }
      }
    }
    Ok(())
  }

  #[must_use]
  pub fn at(
    &self,
    snapshot: Snapshot,
    offset: usize,
  ) -> Vec<(usize, &NameOccurrence)> {
    let mut matches = Vec::new();
    let mut width = usize::MAX;
    for (index, site) in self.occurrences.iter().enumerate() {
      let Some(location) = site.spelling.editable().filter(|location| {
        location.snapshot == snapshot && location.bytes.contains(&offset)
      }) else {
        continue;
      };
      if location.bytes.len() < width {
        matches.clear();
        width = location.bytes.len();
      }
      if location.bytes.len() == width {
        matches.push((index, site));
      }
    }
    matches
  }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct NameKey {
  environment: NameEnvironment,
  kind: SymbolKind,
  text: Vec<u8>,
  temporary: Option<UseSite>,
}

struct Description {
  kind: Option<SymbolKind>,
  text: Option<Vec<u8>>,
  access: Access,
  fragments: Vec<Location>,
  compatible: bool,
  readonly: AttributeState,
}

struct Relationships<'a> {
  workspace: &'a Workspace,
  analysis: &'a Analysis,
  result: NameRelations,
  keys: BTreeMap<NameKey, NameId>,
  unattempted: BTreeMap<UseSite, engine::ArgumentRole>,
  deferred: BTreeMap<UseSite, DeferredName>,
  scope_nodes: BTreeMap<(Snapshot, usize, usize), NameEnvironment>,
}

#[derive(Clone, Debug)]
struct DeferredName {
  environment: NameEnvironment,
  resolution: Option<Resolution>,
  definition: Option<SymbolId>,
}

#[derive(Clone, Copy)]
struct DeferredContext {
  environment: NameEnvironment,
  sequential: bool,
}

#[derive(Debug, Default)]
pub(super) struct DeferredNames {
  names: BTreeMap<UseSite, DeferredName>,
  scopes: BTreeMap<ContextId, NameScope>,
  scope_nodes: BTreeMap<(Snapshot, usize, usize), NameEnvironment>,
  dependencies: Vec<NameDependency>,
  effects: Vec<NameEffect>,
  pub commands:
    BTreeMap<engine::OccurrenceId, engine::analysis::UnattemptedCommand>,
}

impl DeferredNames {
  pub fn collect(
    document: &Document,
    cancelled: &AtomicBool,
  ) -> Result<Self, Error> {
    let mut result = Self::default();
    result.deferred_names(document, cancelled)?;
    Ok(result)
  }

  pub fn resolution(&self, site: UseSite) -> Option<Resolution> {
    self
      .names
      .get(&site)
      .and_then(|name| name.resolution.clone())
  }

  fn isolated_parent(
    &self,
    environment: NameEnvironment,
  ) -> Option<NameEnvironment> {
    if let NameEnvironment::Isolated(id) = environment {
      self.scopes.get(&id).and_then(|scope| scope.parent)
    } else {
      None
    }
  }

  fn deferred_scopes(
    &mut self,
    document: &Document,
    cancelled: &AtomicBool,
  ) -> Result<Vec<DeferredContext>, Error> {
    let mut contexts: Vec<DeferredContext> = Vec::new();
    for context in document.contexts() {
      check_cancelled(cancelled)?;
      let node = document
        .tree
        .root_node()
        .named_descendant_for_byte_range(
          context.location.bytes.start,
          context.location.bytes.end,
        )
        .and_then(|node| {
          std::iter::successors(Some(node), tree_sitter::Node::parent)
            .find(|node| node.kind() == context.kind)
        });
      let parent = context.parent.map(|id| contexts[id.index]);
      let outer = parent.map(|context| context.environment);
      let separate = matches!(
        context.kind.as_str(),
        "subshell" | "command_substitution" | "backquote_substitution"
      ) || node.is_some_and(super::parser::asynchronous);
      let environment =
        if context.parent.is_none() || context.kind == "function_body" {
          NameEnvironment::Caller(context.id)
        } else if separate {
          NameEnvironment::Isolated(context.id)
        } else {
          outer.expect("parent scope")
        };
      if outer != Some(environment) {
        self.scopes.insert(
          context.id,
          NameScope {
            location: context.location.clone(),
            parent: separate.then_some(outer).flatten(),
          },
        );
        self.scope_nodes.insert(
          (
            document.snapshot(),
            context.location.bytes.start,
            context.location.bytes.end,
          ),
          environment,
        );
      }
      contexts.push(DeferredContext {
        environment,
        sequential: parent.is_none_or(|parent| {
          parent.environment != environment || parent.sequential
        }) && node.is_some_and(|node| !uncertain_order(node)),
      });
    }
    Ok(contexts)
  }

  fn deferred_names(
    &mut self,
    document: &Document,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let contexts = self.deferred_scopes(document, cancelled)?;
    let definitions: BTreeMap<_, _> = document
      .symbols()
      .iter()
      .map(|symbol| (symbol.definition, symbol.id))
      .collect();
    let mut functions = BTreeMap::new();
    let mut function_effects = BTreeMap::new();
    let mut recovery = recovery_positions(document, cancelled)?;
    for occurrence in document.occurrences() {
      check_cancelled(cancelled)?;
      while recovery
        .first()
        .is_some_and(|position| *position <= occurrence.location.bytes.start)
      {
        recovery.pop_first();
        functions.clear();
      }
      let site = UseSite::Syntax(occurrence.id);
      let DeferredContext {
        environment,
        sequential,
      } = contexts[occurrence.context.index];
      if !functions.contains_key(&environment) {
        let inherited = self.inherited_functions(&functions, environment);
        functions.insert(environment, inherited);
      }
      let available = functions.get_mut(&environment).expect("name scope");
      if !sequential {
        available.clear();
      }
      let mut resolution = None;
      let own_definition = definitions.get(&occurrence.id).copied();
      if let Value::Literal(name) = &occurrence.name {
        if occurrence.kind == OccurrenceKind::FunctionDefinition {
          resolution = Some(Resolution::default());
          if sequential && let Some(symbol) = own_definition {
            available.insert(name.clone(), symbol);
          }
        } else if occurrence.kind == OccurrenceKind::Command {
          if engine::special_builtin(name) {
            resolution = Some(Resolution {
              alternatives: [engine::Alternative::Builtin].into(),
              ..Resolution::default()
            });
          } else if let Some(symbol) = available.get(name).copied() {
            resolution = Some(Resolution {
              candidates: [symbol].into(),
              ..Resolution::default()
            });
            if let std::collections::btree_map::Entry::Vacant(entry) =
              function_effects.entry(symbol)
            {
              entry.insert(function_targets(document, symbol, cancelled)?);
            }
            let targets = &function_effects[&symbol];
            forget_functions(available, targets);
            if !targets.is_empty() {
              self.effects.push(NameEffect {
                environment,
                location: occurrence.location.clone(),
                targets: targets.clone(),
              });
            }
          }
        }
      }
      if occurrence.kind == OccurrenceKind::Command {
        let targets = self.deferred_arguments(
          document,
          occurrence,
          environment,
          resolution.as_ref(),
        );
        forget_functions(available, &targets);
      }
      self.names.insert(
        site,
        DeferredName {
          environment,
          resolution,
          definition: own_definition,
        },
      );
    }
    Ok(())
  }

  fn inherited_functions(
    &self,
    functions: &BTreeMap<NameEnvironment, FunctionBindings>,
    environment: NameEnvironment,
  ) -> FunctionBindings {
    std::iter::successors(Some(environment), |environment| {
      self.isolated_parent(*environment)
    })
    .find_map(|environment| functions.get(&environment))
    .cloned()
    .unwrap_or_default()
  }

  fn deferred_arguments(
    &mut self,
    document: &Document,
    occurrence: &engine::Occurrence,
    environment: NameEnvironment,
    resolution: Option<&Resolution>,
  ) -> BTreeSet<EffectTarget> {
    if resolution.is_some_and(|resolution| !resolution.candidates.is_empty()) {
      return BTreeSet::new();
    }
    let Some(command) = document.command_at(&occurrence.location) else {
      return BTreeSet::new();
    };
    let command = engine::analysis::unattempted_command(
      document.snapshot(),
      command,
      document.source().as_bytes(),
    );
    if !command.dependencies.is_empty() {
      self.dependencies.push(NameDependency {
        environment,
        location: occurrence.location.clone(),
        targets: command.dependencies.clone(),
      });
    }
    if !command.targets.is_empty() {
      self.effects.push(NameEffect {
        environment,
        location: occurrence.location.clone(),
        targets: command.targets.clone(),
      });
    }
    let targets = command.targets.clone();
    self.commands.insert(occurrence.id, command);
    targets
  }
}

type FunctionBindings = imbl::OrdMap<Vec<u8>, SymbolId>;

impl Workspace {
  pub fn name_relations(
    &self,
    cancelled: &AtomicBool,
  ) -> Result<NameRelations, Error> {
    let analysis = self
      .analysis()
      .ok_or_else(|| Error::InvalidAnalysis("No current analysis".into()))?;
    let mut builder = Relationships::new(self, analysis);
    let mut sites: BTreeMap<(UseSite, ExecutionContextId), BTreeSet<SymbolId>> =
      BTreeMap::new();
    let arguments: BTreeMap<_, _> = analysis
      .argument_definitions
      .iter()
      .flat_map(|((site, context), definitions)| {
        definitions.iter().map(move |id| ((*id, *context), *site))
      })
      .collect();
    for (symbol, context) in &analysis.writes {
      if let Some(definition) = self.symbol(*symbol) {
        let site = arguments
          .get(&(*symbol, *context))
          .copied()
          .unwrap_or(UseSite::Syntax(definition.definition));
        sites.entry((site, *context)).or_default().insert(*symbol);
      }
    }
    let uses = analysis.uses().map(|site| (site.site, site.context));
    for key in uses.chain(analysis.argument_roles.keys().copied()) {
      sites.entry(key).or_default();
    }
    let mut visited = BTreeSet::from([analysis.request().entry.document]);
    for (reference, context, _) in analysis.source_inputs() {
      if context.is_none()
        && let Some(engine::DependencyState::Resolved(target)) =
          analysis.source_bindings().get(&reference)
      {
        visited.insert(target.document);
      }
    }
    let mut attempted = BTreeSet::new();
    for ((site, context), definitions) in sites {
      check_cancelled(cancelled)?;
      attempted.insert(site);
      if let Some(location) = builder.location(site) {
        visited.insert(self.source_snapshot(location.snapshot).document);
      }
      builder.add(site, Some(context), definitions);
    }
    let mut by_document: BTreeMap<Snapshot, Vec<usize>> = BTreeMap::new();
    for (index, site) in builder.result.occurrences.iter().enumerate() {
      by_document
        .entry(site.location.snapshot)
        .or_default()
        .push(index);
    }
    for id in &visited {
      let document = self.document(*id).ok_or(Error::UnknownDocument(*id))?;
      builder.add_document(
        document,
        by_document
          .get(&document.snapshot())
          .map_or(&[], Vec::as_slice),
        &attempted,
        cancelled,
      )?;
    }
    let inactive = builder.add_bodies(&mut visited, &attempted, cancelled)?;
    builder.unattempted_expansions(&visited, cancelled)?;
    builder.incomplete_bodies(&inactive);
    for site in builder.unattempted.keys().copied().collect::<Vec<_>>() {
      if !attempted.contains(&site) {
        builder.add(site, None, BTreeSet::new());
      }
    }
    builder.result.connect_definitions(cancelled)?;
    Ok(builder.result)
  }
}

impl<'a> Relationships<'a> {
  fn source_location(&self, location: &Location) -> Location {
    self
      .workspace
      .navigation_location(location)
      .or_else(|| {
        self
          .workspace
          .derived_origin(location.snapshot.document)
          .map(|origin| origin.argument.clone())
      })
      .unwrap_or_else(|| location.clone())
  }

  fn add_bodies(
    &mut self,
    visited: &mut BTreeSet<engine::DocumentId>,
    attempted: &BTreeSet<UseSite>,
    cancelled: &AtomicBool,
  ) -> Result<BTreeSet<engine::DocumentId>, Error> {
    let mut children: BTreeMap<_, Vec<_>> = BTreeMap::new();
    let mut documents = BTreeSet::new();
    for (site, body) in &self.analysis.derived.bodies {
      for snapshot in body.documents.keys() {
        children
          .entry(site.snapshot)
          .or_default()
          .push((*snapshot, *site));
        documents.insert(*snapshot);
      }
    }
    let mut pending: Vec<_> = children
      .keys()
      .filter(|snapshot| !documents.contains(snapshot))
      .copied()
      .collect();
    let mut inactive = BTreeSet::new();
    while let Some(parent) = pending.pop() {
      check_cancelled(cancelled)?;
      for (snapshot, registration) in
        children.remove(&parent).unwrap_or_default()
      {
        pending.push(snapshot);
        if !self.body_applicable(registration, &inactive) {
          inactive.insert(snapshot.document);
          continue;
        }
        let document = self
          .workspace
          .document(snapshot.document)
          .ok_or(Error::UnknownDocument(snapshot.document))?;
        self.add_document(document, &[], attempted, cancelled)?;
        visited.insert(snapshot.document);
      }
    }
    Ok(inactive)
  }

  fn body_applicable(
    &self,
    registration: engine::OccurrenceId,
    inactive: &BTreeSet<engine::DocumentId>,
  ) -> bool {
    if inactive.contains(&registration.snapshot.document) {
      return false;
    }
    !(self.analysis.derived.bodies[&registration].conditional
      && self
        .deferred
        .get(&UseSite::Syntax(registration))
        .and_then(|site| site.resolution.as_ref())
        .is_some_and(|resolution| !resolution.candidates.is_empty()))
  }

  fn incomplete_bodies(&mut self, inactive: &BTreeSet<engine::DocumentId>) {
    for (registration, body) in &self.analysis.derived.bodies {
      if !self.body_applicable(*registration, inactive) {
        continue;
      }
      let site = self
        .workspace
        .occurrence(*registration)
        .expect("body registration");
      let locations =
        body.incomplete.then_some(&site.location).into_iter().chain(
          body.documents.keys().filter_map(|snapshot| {
            let document = self.workspace.document(snapshot.document)?;
            document
              .has_syntax_errors()
              .then_some(&document.contexts()[0].location)
          }),
        );
      for location in locations {
        let document = self
          .workspace
          .document(location.snapshot.document)
          .expect("body source");
        self.result.dependencies.push(NameDependency {
          environment: self.deferred_environment(document, location),
          location: self.source_location(location),
          targets: [EffectTarget::Variables, EffectTarget::Functions].into(),
        });
      }
    }
  }

  fn new(workspace: &'a Workspace, analysis: &'a Analysis) -> Self {
    Self {
      workspace,
      analysis,
      result: NameRelations {
        effects: analysis
          .unknown_effects()
          .map(|effect| NameEffect {
            environment: NameEnvironment::Execution(effect.context),
            location: effect.location.clone(),
            targets: effect.targets.clone(),
          })
          .collect(),
        gaps: analysis.gaps().to_vec(),
        contexts: analysis.contexts().to_vec(),
        ..NameRelations::default()
      },
      keys: BTreeMap::new(),
      unattempted: BTreeMap::new(),
      deferred: BTreeMap::new(),
      scope_nodes: BTreeMap::new(),
    }
  }

  fn add_document(
    &mut self,
    document: &Document,
    analyzed: &[usize],
    attempted: &BTreeSet<UseSite>,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let projected = self
      .workspace
      .projected_resolutions(document.snapshot(), None);
    for index in analyzed {
      check_cancelled(cancelled)?;
      let site = &mut self.result.occurrences[*index];
      if site
        .site
        .syntax()
        .is_some_and(|id| id.snapshot != site.location.snapshot)
      {
        site.compatible &= projected
          .get(&(site.location.bytes.start, site.location.bytes.end))
          .is_some_and(|result| result.resolution.is_some());
      }
    }
    let mut pending = Vec::new();
    for occurrence in document.occurrences() {
      check_cancelled(cancelled)?;
      if !attempted.contains(&UseSite::Syntax(occurrence.id))
        && projected
          .get(&(
            occurrence.location.bytes.start,
            occurrence.location.bytes.end,
          ))
          .is_none_or(|result| result.resolution.is_none())
      {
        pending.push(occurrence);
      }
    }
    let deferred = document.deferred(cancelled)?;
    for (id, scope) in &deferred.scopes {
      self.result.scopes.insert(
        *id,
        NameScope {
          location: self.source_location(&scope.location),
          parent: scope.parent,
        },
      );
    }
    self.scope_nodes.extend(
      deferred
        .scope_nodes
        .iter()
        .map(|(key, value)| (*key, *value)),
    );
    let locations: BTreeSet<_> = pending
      .iter()
      .map(|site| (site.location.bytes.start, site.location.bytes.end))
      .collect();
    self.result.dependencies.extend(
      deferred
        .dependencies
        .iter()
        .filter(|dependency| {
          locations.contains(&(
            dependency.location.bytes.start,
            dependency.location.bytes.end,
          ))
        })
        .cloned(),
    );
    self.result.effects.extend(
      deferred
        .effects
        .iter()
        .filter(|effect| {
          locations
            .contains(&(effect.location.bytes.start, effect.location.bytes.end))
        })
        .cloned(),
    );
    for occurrence in &pending {
      let site = UseSite::Syntax(occurrence.id);
      if let Some(name) = deferred.names.get(&site) {
        self.deferred.insert(site, name.clone());
      }
      if let Some(command) = deferred.commands.get(&occurrence.id) {
        for role in &command.arguments {
          self
            .unattempted
            .insert(UseSite::argument(&role.location), role.clone());
        }
      }
    }
    for occurrence in pending {
      check_cancelled(cancelled)?;
      self.add(UseSite::Syntax(occurrence.id), None, BTreeSet::new());
    }
    for role in document.argument_roles() {
      let site = UseSite::argument(&role.location);
      if !attempted.contains(&site) {
        self.unattempted.entry(site).or_insert_with(|| role.clone());
      }
    }
    Ok(())
  }

  fn deferred_environment(
    &self,
    document: &Document,
    location: &Location,
  ) -> NameEnvironment {
    let mut node = document.tree.root_node().named_descendant_for_byte_range(
      location.bytes.start,
      location.bytes.end,
    );
    while let Some(current) = node {
      if let Some(environment) = self.scope_nodes.get(&(
        document.snapshot(),
        current.start_byte(),
        current.end_byte(),
      )) {
        return *environment;
      }
      node = current.parent();
    }
    NameEnvironment::Caller(document.contexts()[0].id)
  }

  fn unattempted_expansions(
    &mut self,
    documents: &BTreeSet<engine::DocumentId>,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    let mut covered: BTreeSet<_> = self
      .analysis
      .contexts()
      .iter()
      .filter(|context| {
        matches!(context.kind, engine::ExecutionKind::Arithmetic(_))
      })
      .map(|context| &context.location)
      .chain(
        self
          .analysis
          .unknown_effects()
          .map(|effect| &effect.location),
      )
      .filter_map(|location| self.workspace.navigation_location(location))
      .map(|location| {
        (location.snapshot, location.bytes.start, location.bytes.end)
      })
      .collect();
    for id in documents {
      let document = self
        .workspace
        .document(*id)
        .ok_or(Error::UnknownDocument(*id))?;
      let mut arithmetic: Vec<Option<&Location>> = Vec::new();
      for context in document.contexts() {
        check_cancelled(cancelled)?;
        let outer = context.parent.and_then(|parent| arithmetic[parent.index]);
        arithmetic.push(if context.kind == "arithmetic_expansion" {
          Some(&context.location)
        } else {
          outer
        });
        if matches!(
          context.kind.as_str(),
          "parameter_expansion"
            | "command_substitution"
            | "backquote_substitution"
        ) && let Some(location) = outer
          && covered.insert((
            location.snapshot,
            location.bytes.start,
            location.bytes.end,
          ))
        {
          self.result.dependencies.push(NameDependency {
            environment: self.deferred_environment(document, location),
            location: location.clone(),
            targets: [engine::EffectTarget::Variables].into(),
          });
        }
      }
    }
    Ok(())
  }

  fn location(&self, site: UseSite) -> Option<Location> {
    match site {
      UseSite::Syntax(id) => {
        Some(self.workspace.occurrence(id)?.location.clone())
      }
      UseSite::Argument {
        snapshot,
        start,
        end,
      } => Some(Location {
        snapshot,
        bytes: start..end,
      }),
    }
  }

  fn add(
    &mut self,
    site: UseSite,
    context: Option<ExecutionContextId>,
    mut definitions: BTreeSet<SymbolId>,
  ) {
    let Some(location) = self.location(site) else {
      return;
    };
    let Some(document) = self.workspace.document(location.snapshot.document)
    else {
      return;
    };
    let deferred = context
      .is_none()
      .then(|| self.deferred.get(&site))
      .flatten();
    if let Some(deferred) = deferred {
      definitions.extend(deferred.definition);
    }
    let Some(Description {
      kind,
      text,
      access,
      fragments: spelling,
      compatible,
      readonly,
    }) = self.describe(document, site, context)
    else {
      return;
    };
    let mut names = BTreeSet::new();
    if let (Some(kind), Some(text)) = (kind, &text) {
      let environment = context
        .map(|context| {
          NameEnvironment::Execution(self.analysis.name_environment(context))
        })
        .or_else(|| deferred.map(|site| site.environment))
        .unwrap_or_else(|| self.deferred_environment(document, &location));
      let temporary = context.and_then(|context| {
        self.temporary_scope(site, context, text, &definitions)
      });
      let key = NameKey {
        environment,
        kind,
        text: text.clone(),
        temporary,
      };
      let id = *self.keys.entry(key).or_insert_with(|| {
        let id = NameId(self.result.names.len());
        self.result.names.push(Name {
          id,
          kind,
          text: text.clone(),
          environment,
          temporary,
          definitions: BTreeSet::new(),
        });
        id
      });
      self.result.names[id.0].definitions.extend(&definitions);
      names.insert(id);
    }
    let resolution = context
      .map(|context| {
        self
          .analysis
          .resolution(site, context)
          .cloned()
          .unwrap_or_default()
      })
      .or_else(|| deferred.and_then(|site| site.resolution.clone()));
    let spelling = text.as_ref().map_or(NameSpelling::Generated, |name| {
      self.spelling(document, &location, &spelling, name)
    });
    let source = self.source_location(&location);
    let occurrence = NameOccurrence {
      site,
      location: source,
      spelling,
      context,
      names,
      access,
      definitions,
      resolution,
      compatible,
      readonly,
    };
    self.deferred_dependency(&occurrence);
    self.result.occurrences.push(occurrence);
  }

  fn deferred_dependency(&mut self, occurrence: &NameOccurrence) {
    if occurrence.context.is_some() {
      return;
    }
    for id in &occurrence.names {
      let name = &self.result.names[id.0];
      if (occurrence.resolution.is_none()
        || matches!(name.environment, NameEnvironment::Caller(_)))
        && !(name.kind == SymbolKind::Function
          && engine::special_builtin(&name.text))
      {
        self.result.dependencies.push(NameDependency {
          environment: name.environment,
          location: occurrence.location.clone(),
          targets: [match name.kind {
            SymbolKind::Variable => EffectTarget::Variable(name.text.clone()),
            SymbolKind::Function => EffectTarget::Function(name.text.clone()),
          }]
          .into(),
        });
      }
    }
  }

  fn temporary_scope(
    &self,
    site: UseSite,
    context: ExecutionContextId,
    text: &[u8],
    definitions: &BTreeSet<SymbolId>,
  ) -> Option<UseSite> {
    if self.analysis.temporary.is_empty() {
      return None;
    }
    for ancestor in std::iter::successors(Some(context), |id| {
      self
        .analysis
        .context(*id)
        .and_then(|context| context.parent)
    }) {
      let Some(scope) = self
        .analysis
        .temporary
        .get(&ancestor)
        .and_then(|names| names.get(text))
      else {
        continue;
      };
      if !definitions.is_empty() {
        return Some(*scope);
      }
      let local =
        self
          .analysis
          .resolution(site, context)
          .is_some_and(|resolution| {
            resolution.candidates.iter().any(|symbol| {
              self
                .analysis
                .writes
                .range(
                  (*symbol, ExecutionContextId(0))
                    ..=(*symbol, ExecutionContextId(usize::MAX)),
                )
                .any(|(_, written)| {
                  std::iter::successors(Some(*written), |id| {
                    self
                      .analysis
                      .context(*id)
                      .and_then(|context| context.parent)
                  })
                  .any(|id| id == ancestor)
                })
            })
          });
      return local.then_some(*scope);
    }
    None
  }

  fn describe(
    &self,
    document: &Document,
    site: UseSite,
    context: Option<ExecutionContextId>,
  ) -> Option<Description> {
    match site {
      UseSite::Syntax(id) => {
        let occurrence = document.occurrence(id)?;
        if matches!(
          occurrence.kind,
          OccurrenceKind::SpecialParameter
            | OccurrenceKind::PositionalParameter
        ) {
          return None;
        }
        let kind = if matches!(
          occurrence.kind,
          OccurrenceKind::Command | OccurrenceKind::FunctionDefinition
        ) {
          SymbolKind::Function
        } else {
          SymbolKind::Variable
        };
        Some(Description {
          kind: Some(kind),
          text: literal(&occurrence.name),
          access: occurrence.access,
          fragments: syntax_spelling(document, occurrence),
          compatible: true,
          readonly: context
            .and_then(|context| self.analysis.attributes.get(&(id, context)))
            .copied()
            .unwrap_or(AttributeState::Unknown),
        })
      }
      UseSite::Argument { .. } => {
        let analyzed = context.and_then(|context| {
          self.analysis.argument_roles.get(&(site, context))
        });
        let role =
          analyzed
            .or_else(|| self.unattempted.get(&site))
            .or_else(|| {
              document
                .argument_roles()
                .iter()
                .find(|role| UseSite::argument(&role.location) == site)
            });
        let role = role?;
        let mut readonly = AttributeState::Unknown;
        let (kind, text, access) = match &role.kind {
          Some(ArgumentKind::Variable {
            name,
            access,
            readonly: state,
            ..
          }) => {
            if analyzed.is_some() {
              readonly = *state;
            }
            (Some(SymbolKind::Variable), Some(name.clone()), *access)
          }
          Some(ArgumentKind::Function { name }) => (
            Some(SymbolKind::Function),
            Some(name.clone()),
            Access::NameReference,
          ),
          Some(ArgumentKind::Command { name }) => {
            (Some(SymbolKind::Function), literal(name), Access::Read)
          }
          Some(ArgumentKind::Number | ArgumentKind::ShellBody) | None => {
            (None, None, Access::Unknown)
          }
        };
        Some(Description {
          kind,
          text,
          access,
          fragments: role.spelling.clone(),
          compatible: kind.is_some(),
          readonly,
        })
      }
    }
  }

  fn spelling(
    &self,
    document: &Document,
    location: &Location,
    fragments: &[Location],
    name: &[u8],
  ) -> NameSpelling {
    let projected: Option<Vec<_>> = fragments
      .iter()
      .map(|part| self.workspace.project_locations(part))
      .collect();
    let Some(projected) = projected else {
      return NameSpelling::Generated;
    };
    let projected: Vec<_> = projected.into_iter().flatten().collect();
    if projected.is_empty() {
      return NameSpelling::Generated;
    }
    if let (Some(first), Some(last)) = (projected.first(), projected.last())
      && projected.windows(2).all(|parts| {
        parts[0].snapshot == parts[1].snapshot
          && parts[0].bytes.end == parts[1].bytes.start
      })
    {
      let exact = Location {
        snapshot: first.snapshot,
        bytes: first.bytes.start..last.bytes.end,
      };
      if self
        .workspace
        .document(exact.snapshot.document)
        .and_then(|doc| doc.text(&exact))
        .is_some_and(|text| text.as_bytes() == name)
      {
        return NameSpelling::Exact(exact);
      }
    }
    if self
      .workspace
      .derived_origin(location.snapshot.document)
      .is_none()
      && let Some(node) =
        document.tree.root_node().named_descendant_for_byte_range(
          location.bytes.start,
          location.bytes.end,
        )
      && node.byte_range() == location.bytes
      && engine::word::literal_value(node, document.source().as_bytes())
        == Value::Literal(name.to_vec())
    {
      return NameSpelling::LiteralWord(location.clone());
    }
    NameSpelling::Fragments(projected)
  }
}

fn literal(value: &Value) -> Option<Vec<u8>> {
  match value {
    Value::Literal(text) => Some(text.clone()),
    Value::Unknown(_) | Value::Partial(_) => None,
  }
}

fn recovery_positions(
  document: &Document,
  cancelled: &AtomicBool,
) -> Result<BTreeSet<usize>, Error> {
  let mut positions = BTreeSet::new();
  if document.has_syntax_errors() {
    let mut pending = vec![document.tree.root_node()];
    while let Some(node) = pending.pop() {
      check_cancelled(cancelled)?;
      if node.is_error() || node.is_missing() {
        positions.insert(node.start_byte());
      } else if node.has_error() {
        pending.extend(node.children(&mut node.walk()));
      }
    }
  }
  Ok(positions)
}

fn forget_functions(
  available: &mut FunctionBindings,
  targets: &BTreeSet<EffectTarget>,
) {
  for target in targets {
    match target {
      EffectTarget::Function(name) => {
        available.remove(name);
      }
      EffectTarget::FunctionPattern(value) => {
        let names: Vec<_> = available
          .keys()
          .filter(|name| value.can_equal(name))
          .cloned()
          .collect();
        for name in names {
          available.remove(&name);
        }
      }
      EffectTarget::Functions => available.clear(),
      _ => {}
    }
  }
}

fn uncertain_order(node: tree_sitter::Node<'_>) -> bool {
  match node.kind() {
    "if_clause"
    | "else_part"
    | "case_clause"
    | "for_clause"
    | "while_clause"
    | "until_clause"
    | "backquote_substitution"
    | "here_document_body" => true,
    "and_or" => {
      node
        .named_children(&mut node.walk())
        .filter(|child| child.kind() == "pipeline")
        .count()
        > 1
    }
    "pipe_sequence" => {
      node
        .children_by_field_name("command", &mut node.walk())
        .count()
        > 1
    }
    _ => false,
  }
}

fn function_targets(
  document: &Document,
  symbol: SymbolId,
  cancelled: &AtomicBool,
) -> Result<BTreeSet<EffectTarget>, Error> {
  let definition = document
    .symbol(symbol)
    .and_then(|symbol| document.occurrence(symbol.definition))
    .expect("function definition");
  let node = document
    .tree
    .root_node()
    .named_descendant_for_byte_range(
      definition.location.bytes.start,
      definition.location.bytes.end,
    )
    .expect("function name");
  let body = std::iter::successors(Some(node), tree_sitter::Node::parent)
    .find(|node| node.kind() == "function_definition")
    .and_then(|node| node.child_by_field_name("body"))
    .expect("function body");
  let mut pending = vec![body];
  let mut targets = BTreeSet::new();
  while let Some(node) = pending.pop() {
    check_cancelled(cancelled)?;
    if super::parser::damaged(node) {
      return Ok([EffectTarget::Functions].into());
    }
    if super::parser::asynchronous(node) {
      continue;
    }
    match node.kind() {
      "function_definition" => {
        let Some(name) = node.child_by_field_name("name") else {
          return Ok([EffectTarget::Functions].into());
        };
        let name = engine::word::lexical(name, document.source().as_bytes());
        targets.insert(EffectTarget::Function(name));
        continue;
      }
      "subshell" | "command_substitution" | "backquote_substitution" => {
        continue;
      }
      "simple_command" => {
        let command = engine::analysis::unattempted_command(
          document.snapshot(),
          node,
          document.source().as_bytes(),
        );
        targets.extend(command.targets.into_iter().filter(|target| {
          matches!(
            target,
            EffectTarget::Function(_)
              | EffectTarget::FunctionPattern(_)
              | EffectTarget::Functions
          )
        }));
      }
      _ => {}
    }
    pending.extend(node.named_children(&mut node.walk()));
  }
  Ok(targets)
}
