use crate::interface::application::engine::workspace::derived::DerivedDocuments;
use crate::interface::application::engine::{
  self, Access, AnalysisRequest, DependencyState, Location, OccurrenceId,
  ReferenceId, Snapshot, SymbolId, SymbolKind, UseSite, Workspace,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Valid only within the analysis result that issued it.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExecutionContextId(
  pub(in crate::interface::application::engine) usize,
);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ExecutionKind {
  Entry,
  /// Analysis under inputs invariant after registration; no signal delivery is assumed.
  Deferred(Snapshot),
  Function(SymbolId),
  Source(ReferenceId),
  Subshell,
  Substitution,
  Arithmetic(Snapshot),
  Redirection,
  Asynchronous,
  Pipeline,
  TemporaryAssignments,
}

impl ExecutionKind {
  pub(super) const fn separate(&self) -> bool {
    matches!(
      self,
      Self::Deferred(_)
        | Self::Subshell
        | Self::Substitution
        | Self::Redirection
        | Self::Asynchronous
        | Self::Pipeline
    )
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionContext {
  pub id: ExecutionContextId,
  pub parent: Option<ExecutionContextId>,
  pub environment: ExecutionContextId,
  pub kind: ExecutionKind,
  pub location: Location,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Alternative {
  Environment,
  External,
  Unset,
  Builtin,
  ExternalCommand,
  SpecialParameter,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ResolutionIssue {
  InitialState,
  DynamicName,
  UnknownEffect,
  ExcludedEffect,
  Recovery,
  UndecodedSource,
  EvaluationOrder,
  PipelineEnvironment,
  UnresolvedSource,
  Recursion,
  ValueLimit,
  UnsupportedControl,
}

#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Resolution {
  pub candidates: BTreeSet<SymbolId>,
  pub alternatives: BTreeSet<Alternative>,
  pub issues: BTreeSet<ResolutionIssue>,
  pub origins: imbl::OrdSet<BindingOrigin>,
}

/// Name owners, independent of the value or definition that reaches a read.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum BindingOrigin {
  Context(ExecutionContextId),
  Definition(SymbolId),
  Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionStatus {
  NotAttempted,
  Resolved(SymbolId),
  Candidates,
  Unresolved,
}

impl Resolution {
  pub(in crate::interface::application::engine) fn name_definitions(
    &self,
  ) -> impl Iterator<Item = SymbolId> + '_ {
    self
      .candidates
      .iter()
      .copied()
      .chain(self.origins.iter().filter_map(|origin| {
        if let BindingOrigin::Definition(id) = origin {
          Some(*id)
        } else {
          None
        }
      }))
  }

  pub(in crate::interface::application::engine) fn join(
    &mut self,
    other: &Self,
  ) {
    self.candidates.extend(&other.candidates);
    self.alternatives.extend(&other.alternatives);
    self.issues.extend(&other.issues);
    if !other.origins.is_subset(&self.origins) {
      self.origins.extend(other.origins.iter().copied());
    }
  }

  pub(in crate::interface::application::engine) fn in_context(
    mut self,
    context: ExecutionContextId,
  ) -> Self {
    self.origins.remove(&BindingOrigin::Unknown);
    self.origins.insert(BindingOrigin::Context(context));
    self
  }

  pub(in crate::interface::application::engine) fn is_environment(
    &self,
  ) -> bool {
    self.candidates.is_empty()
      && self.issues.is_empty()
      && self.alternatives == BTreeSet::from([Alternative::Environment])
  }

  #[must_use]
  pub fn status(&self) -> ResolutionStatus {
    if self.candidates.len() == 1
      && self.alternatives.is_empty()
      && self.issues.is_empty()
      && let Some(id) = self.candidates.first()
    {
      ResolutionStatus::Resolved(*id)
    } else if self.candidates.is_empty() {
      ResolutionStatus::Unresolved
    } else {
      ResolutionStatus::Candidates
    }
  }

  #[must_use]
  pub fn is_complete(&self) -> bool {
    self.issues.is_empty()
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedUse {
  pub site: UseSite,
  pub context: ExecutionContextId,
  pub resolution: Resolution,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UseResolution {
  pub site: UseSite,
  pub contexts: Vec<ExecutionContextId>,
  pub resolution: Option<Resolution>,
}

impl UseResolution {
  #[must_use]
  pub fn status(&self) -> ResolutionStatus {
    self
      .resolution
      .as_ref()
      .map_or(ResolutionStatus::NotAttempted, Resolution::status)
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalysisGap {
  pub context: ExecutionContextId,
  pub location: Location,
  pub issue: ResolutionIssue,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EffectTarget {
  Variable(Vec<u8>),
  Function(Vec<u8>),
  VariablePattern(engine::Value),
  FunctionPattern(engine::Value),
  Attribute(Vec<u8>),
  AttributePattern(engine::Value),
  Variables,
  Functions,
  Attributes,
  /// May read source text whose names and dependencies are unknown.
  Sources,
  /// Includes the PWD and OLDPWD values set by `cd`.
  Directory,
  Positionals,
  Options,
  SpecialParameters,
}

impl EffectTarget {
  /// Whether the target may be the variable or function `name`.
  #[must_use]
  pub fn covers(&self, kind: SymbolKind, name: &[u8]) -> bool {
    match (self, kind) {
      (Self::Variable(text), SymbolKind::Variable)
      | (Self::Function(text), SymbolKind::Function) => text == name,
      (Self::VariablePattern(value), SymbolKind::Variable)
      | (Self::FunctionPattern(value), SymbolKind::Function) => {
        value.can_equal(name)
      }
      (Self::Variables, SymbolKind::Variable)
      | (Self::Functions, SymbolKind::Function) => true,
      (Self::Directory, SymbolKind::Variable) => {
        matches!(name, b"PWD" | b"OLDPWD")
      }
      _ => false,
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownEffect {
  pub context: ExecutionContextId,
  pub location: Location,
  pub issue: ResolutionIssue,
  pub targets: BTreeSet<EffectTarget>,
}

pub(super) type EffectKey =
  (ExecutionContextId, Snapshot, usize, usize, ResolutionIssue);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AttributeState {
  Known(bool),
  Conditional,
  Unknown,
}

impl AttributeState {
  pub(in crate::interface::application::engine) fn join(
    self,
    other: Self,
  ) -> Self {
    if self == other {
      self
    } else if self == Self::Unknown || other == Self::Unknown {
      Self::Unknown
    } else {
      Self::Conditional
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgumentKind {
  Number,
  ShellBody,
  Variable {
    name: Vec<u8>,
    access: Access,
    declaration: bool,
    readonly: AttributeState,
  },
  Function {
    name: Vec<u8>,
  },
  Command {
    name: engine::Value,
  },
}

impl ArgumentKind {
  pub(in crate::interface::application::engine) fn join(
    left: &mut Option<Self>,
    right: Option<&Self>,
  ) {
    match (&mut *left, right) {
      (
        Some(Self::Variable {
          name,
          access,
          declaration,
          readonly,
        }),
        Some(Self::Variable {
          name: next_name,
          access: next_access,
          declaration: next_declaration,
          readonly: next_readonly,
        }),
      ) if name == next_name => {
        if access != next_access {
          *access = Access::Unknown;
        }
        *declaration &= next_declaration;
        *readonly = readonly.join(*next_readonly);
      }
      (Some(Self::Command { name }), Some(Self::Command { name: next })) => {
        name.join(next);
      }
      (Some(Self::Function { name }), Some(Self::Function { name: next }))
        if name == next => {}
      (Some(Self::Number), Some(Self::Number))
      | (Some(Self::ShellBody), Some(Self::ShellBody)) => {}
      _ => *left = None,
    }
  }
}

#[derive(Clone, Debug)]
pub struct ArgumentRole {
  /// `None` denotes a role that does not depend on an execution context.
  pub context: Option<ExecutionContextId>,
  pub location: Location,
  pub spelling: Vec<Location>,
  pub kind: Option<ArgumentKind>,
}

impl ArgumentRole {
  pub(in crate::interface::application::engine) fn selection_locations(
    &self,
  ) -> impl Iterator<Item = &Location> {
    self.spelling.iter().chain(
      (self.context.is_some() && self.spelling.is_empty())
        .then_some(&self.location),
    )
  }

  pub(in crate::interface::application::engine) fn join(
    &mut self,
    other: &Self,
  ) {
    ArgumentKind::join(&mut self.kind, other.kind.as_ref());
    if self.spelling != other.spelling {
      self.kind = None;
    }
  }

  fn is_reference(&self) -> bool {
    matches!(
      self.kind,
      Some(
        ArgumentKind::Function { .. }
          | ArgumentKind::Command { .. }
          | ArgumentKind::Variable {
            access: Access::NameReference
              | Access::Read
              | Access::Write
              | Access::ReadWrite
              | Access::ReadConditionalWrite,
            ..
          }
      )
    )
  }
}

#[derive(Debug)]
pub struct Analysis {
  pub(in crate::interface::application::engine) writes:
    BTreeSet<(SymbolId, ExecutionContextId)>,
  pub(in crate::interface::application::engine) temporary:
    BTreeMap<ExecutionContextId, BTreeMap<Vec<u8>, UseSite>>,
  pub(in crate::interface::application::engine) argument_roles:
    BTreeMap<(UseSite, ExecutionContextId), ArgumentRole>,
  pub(in crate::interface::application::engine) argument_definitions:
    BTreeMap<(UseSite, ExecutionContextId), BTreeSet<SymbolId>>,
  pub(in crate::interface::application::engine) effects:
    BTreeMap<EffectKey, UnknownEffect>,
  pub(in crate::interface::application::engine) derived: DerivedDocuments,
  pub(in crate::interface::application::engine) attributes:
    BTreeMap<(OccurrenceId, ExecutionContextId), AttributeState>,
  pub(in crate::interface::application::engine) generation: Arc<()>,
  pub(in crate::interface::application::engine) request: AnalysisRequest,
  pub(in crate::interface::application::engine) contexts: Vec<ExecutionContext>,
  pub(in crate::interface::application::engine) uses:
    BTreeMap<(UseSite, ExecutionContextId), ResolvedUse>,
  pub(in crate::interface::application::engine) gaps: Vec<AnalysisGap>,
  pub(in crate::interface::application::engine) snapshots: Vec<Snapshot>,
  pub(in crate::interface::application::engine) source_bindings:
    BTreeMap<ReferenceId, DependencyState>,
  pub(in crate::interface::application::engine) readable:
    BTreeSet<OccurrenceId>,
  pub(in crate::interface::application::engine) references:
    BTreeMap<SymbolId, BTreeSet<(UseSite, ExecutionContextId)>>,
  pub(in crate::interface::application::engine) non_sources:
    BTreeSet<ReferenceId>,
  pub(in crate::interface::application::engine) source_operands:
    BTreeMap<ReferenceId, Option<Location>>,
  pub(in crate::interface::application::engine) source_inputs:
    BTreeMap<(ReferenceId, Option<ExecutionContextId>), engine::SourceInput>,
  pub(in crate::interface::application::engine) values:
    BTreeMap<(OccurrenceId, ExecutionContextId), Arc<engine::Value>>,
}

impl Analysis {
  pub(super) fn new(
    request: AnalysisRequest,
    entry: ExecutionContext,
    readable: BTreeSet<OccurrenceId>,
  ) -> Self {
    Self {
      argument_roles: BTreeMap::new(),
      argument_definitions: BTreeMap::new(),
      effects: BTreeMap::new(),
      derived: DerivedDocuments::default(),
      attributes: BTreeMap::new(),
      generation: Arc::default(),
      request,
      contexts: vec![entry],
      uses: BTreeMap::new(),
      writes: BTreeSet::new(),
      temporary: BTreeMap::new(),
      gaps: Vec::new(),
      snapshots: Vec::new(),
      source_bindings: BTreeMap::new(),
      readable,
      references: BTreeMap::new(),
      source_inputs: BTreeMap::new(),
      source_operands: BTreeMap::new(),
      non_sources: BTreeSet::new(),
      values: BTreeMap::new(),
    }
  }

  /// Argument uses without a reference role imply unknown effects.
  pub(super) fn index_uses(
    &mut self,
    cancelled: &AtomicBool,
  ) -> Result<(), engine::Error> {
    for (key, site) in &mut self.uses {
      if matches!(site.site, UseSite::Argument { .. })
        && self
          .argument_roles
          .get(&(site.site, site.context))
          .is_none_or(|role| !role.is_reference())
      {
        site
          .resolution
          .issues
          .insert(ResolutionIssue::UnknownEffect);
      }
      engine::check_cancelled(cancelled)?;
      for candidate in &site.resolution.candidates {
        self.references.entry(*candidate).or_default().insert(*key);
      }
    }
    engine::check_cancelled(cancelled)
  }

  pub(in crate::interface::application::engine) fn name_environment(
    &self,
    context: ExecutionContextId,
  ) -> ExecutionContextId {
    let mut environment = self.contexts[context.0].environment;
    while matches!(
      self.contexts[environment.0].kind,
      ExecutionKind::Deferred(_)
    ) {
      let parent = self.contexts[environment.0]
        .parent
        .expect("deferred parent");
      environment = self.contexts[parent.0].environment;
    }
    environment
  }

  pub(in crate::interface::application::engine) fn binding_context(
    &self,
    context: ExecutionContextId,
  ) -> ExecutionContextId {
    self
      .context_ancestors(context)
      .find(|context| {
        context.id == context.environment
          || self.temporary.contains_key(&context.id)
      })
      .expect("execution environment")
      .id
  }

  pub(in crate::interface::application::engine) fn synchronize(
    &mut self,
    workspace: &Workspace,
  ) {
    self.generation = workspace.generation.clone();
    self.snapshots = workspace
      .documents()
      .map(engine::Document::snapshot)
      .collect();
    self.source_bindings = workspace
      .documents()
      .flat_map(|document| {
        workspace
          .dependencies(document.snapshot().document)
          .map(|(reference, state)| (reference.id, state.clone()))
      })
      .collect();
    self
      .readable
      .retain(|id| workspace.occurrence(*id).is_some());
  }

  pub fn argument_roles(&self) -> impl Iterator<Item = &ArgumentRole> {
    self.argument_roles.values()
  }

  pub fn unknown_effects(&self) -> impl Iterator<Item = &UnknownEffect> {
    self.effects.values()
  }

  pub fn source_operand(&self, reference: ReferenceId) -> Option<&Location> {
    self
      .source_operands
      .get(&reference)
      .and_then(Option::as_ref)
  }

  /// A missing execution context identifies a syntax-only reference.
  pub fn source_inputs(
    &self,
  ) -> impl Iterator<
    Item = (
      ReferenceId,
      Option<ExecutionContextId>,
      &engine::SourceInput,
    ),
  > {
    self
      .source_inputs
      .iter()
      .map(|((id, context), input)| (*id, *context, input))
  }

  #[must_use]
  pub fn value(
    &self,
    occurrence: OccurrenceId,
    context: ExecutionContextId,
  ) -> Option<&engine::Value> {
    self.values.get(&(occurrence, context)).map(AsRef::as_ref)
  }

  pub fn values(
    &self,
    occurrence: OccurrenceId,
  ) -> impl Iterator<Item = (ExecutionContextId, &engine::Value)> {
    self
      .values
      .range(
        (occurrence, ExecutionContextId(0))
          ..=(occurrence, ExecutionContextId(usize::MAX)),
      )
      .map(|((_, context), value)| (*context, value.as_ref()))
  }

  #[must_use]
  pub fn has_recursive_calls(&self) -> bool {
    self
      .gaps
      .iter()
      .any(|gap| gap.issue == ResolutionIssue::Recursion)
  }

  pub fn readonly_contexts(
    &self,
    id: OccurrenceId,
  ) -> impl Iterator<Item = (ExecutionContextId, AttributeState)> {
    self
      .attributes
      .range((id, ExecutionContextId(0))..=(id, ExecutionContextId(usize::MAX)))
      .map(|((_, context), state)| (*context, *state))
  }

  #[must_use]
  pub const fn request(&self) -> &AnalysisRequest {
    &self.request
  }

  #[must_use]
  pub fn snapshots(&self) -> &[Snapshot] {
    &self.snapshots
  }

  #[must_use]
  pub const fn source_bindings(
    &self,
  ) -> &BTreeMap<ReferenceId, DependencyState> {
    &self.source_bindings
  }

  #[must_use]
  pub fn summarize(&self, site: UseSite) -> UseResolution {
    let mut result = UseResolution {
      site,
      contexts: Vec::new(),
      resolution: None,
    };
    for site in self.resolutions(site) {
      result.contexts.push(site.context);
      result
        .resolution
        .get_or_insert_with(Resolution::default)
        .join(&site.resolution);
    }
    if matches!(site, UseSite::Argument { .. })
      && self
        .argument_summary(site)
        .is_none_or(|role| !role.is_reference())
      && let Some(resolution) = &mut result.resolution
    {
      resolution.issues.insert(ResolutionIssue::UnknownEffect);
    }
    result
  }

  pub fn document_resolutions(
    &self,
    snapshot: Snapshot,
  ) -> impl Iterator<Item = UseResolution> {
    let sites: BTreeSet<_> = self
      .readable
      .range(
        OccurrenceId { snapshot, index: 0 }..=OccurrenceId {
          snapshot,
          index: usize::MAX,
        },
      )
      .copied()
      .map(UseSite::Syntax)
      .chain(
        self
          .document_argument_roles(snapshot)
          .map(|role| UseSite::argument(&role.location)),
      )
      .collect();
    sites.into_iter().map(|site| self.summarize(site))
  }

  pub(in crate::interface::application::engine) fn written_definitions(
    &self,
    site: UseSite,
  ) -> impl Iterator<Item = SymbolId> {
    self
      .argument_definitions
      .range(
        (site, ExecutionContextId(0))..=(site, ExecutionContextId(usize::MAX)),
      )
      .flat_map(|(_, definitions)| definitions.iter().copied())
  }

  pub(in crate::interface::application::engine) fn argument_definition_documents(
    &self,
  ) -> impl Iterator<Item = Snapshot> {
    self
      .argument_definitions
      .values()
      .flatten()
      .map(|id| id.snapshot)
  }

  pub(in crate::interface::application::engine) fn argument_summary(
    &self,
    site: UseSite,
  ) -> Option<ArgumentRole> {
    let mut roles = self
      .argument_roles
      .range(
        (site, ExecutionContextId(0))..=(site, ExecutionContextId(usize::MAX)),
      )
      .map(|(_, role)| role);
    let mut summary = roles.next()?.clone();
    for role in roles {
      summary.join(role);
    }
    Some(summary)
  }

  pub(in crate::interface::application::engine) fn argument_summaries(
    &self,
    snapshot: Snapshot,
  ) -> BTreeMap<(usize, usize), ArgumentRole> {
    let mut roles: BTreeMap<(usize, usize), ArgumentRole> = BTreeMap::new();
    for role in self.document_argument_roles(snapshot) {
      roles
        .entry((role.location.bytes.start, role.location.bytes.end))
        .and_modify(|old| old.join(role))
        .or_insert_with(|| role.clone());
    }
    roles
  }

  fn document_argument_roles(
    &self,
    snapshot: Snapshot,
  ) -> impl Iterator<Item = &ArgumentRole> {
    let first = UseSite::Argument {
      snapshot,
      start: 0,
      end: 0,
    };
    let last = UseSite::Argument {
      snapshot,
      start: usize::MAX,
      end: usize::MAX,
    };
    self
      .argument_roles
      .range(
        (first, ExecutionContextId(0))..=(last, ExecutionContextId(usize::MAX)),
      )
      .map(|(_, role)| role)
  }

  #[must_use]
  pub fn contexts(&self) -> &[ExecutionContext] {
    &self.contexts
  }

  #[must_use]
  pub fn context(&self, id: ExecutionContextId) -> Option<&ExecutionContext> {
    self.contexts.get(id.0)
  }

  /// The context and its ancestors, innermost first.
  pub(in crate::interface::application::engine) fn context_ancestors(
    &self,
    id: ExecutionContextId,
  ) -> impl Iterator<Item = &ExecutionContext> {
    std::iter::successors(self.context(id), |context| {
      context.parent.and_then(|parent| self.context(parent))
    })
  }

  pub fn uses(&self) -> impl Iterator<Item = &ResolvedUse> {
    self.uses.values()
  }

  #[must_use]
  pub fn gaps(&self) -> &[AnalysisGap] {
    &self.gaps
  }

  #[must_use]
  pub fn resolution(
    &self,
    site: UseSite,
    context: ExecutionContextId,
  ) -> Option<&Resolution> {
    self.uses.get(&(site, context)).map(|site| &site.resolution)
  }

  #[must_use]
  pub fn status(
    &self,
    site: UseSite,
    context: ExecutionContextId,
  ) -> ResolutionStatus {
    self
      .resolution(site, context)
      .map_or(ResolutionStatus::NotAttempted, Resolution::status)
  }

  pub fn resolutions(
    &self,
    site: UseSite,
  ) -> impl Iterator<Item = &ResolvedUse> {
    self
      .uses
      .range(
        (site, ExecutionContextId(0))..=(site, ExecutionContextId(usize::MAX)),
      )
      .map(|(_, site)| site)
  }

  pub fn references(
    &self,
    definition: SymbolId,
  ) -> impl Iterator<Item = &ResolvedUse> {
    self
      .references
      .get(&definition)
      .into_iter()
      .flatten()
      .filter_map(|key| self.uses.get(key))
  }
}

pub(in crate::interface::application::engine) fn argument_resolution(
  analysis: Option<&Analysis>,
  role: &ArgumentRole,
) -> Option<Resolution> {
  if role.spelling.is_empty() {
    return None;
  }
  let site = UseSite::argument(&role.location);
  role.context?;
  let analysis = analysis?;
  match &role.kind {
    Some(ArgumentKind::Variable {
      access: Access::Write,
      ..
    }) => {
      let candidates =
        analysis.written_definitions(site).collect::<BTreeSet<_>>();
      (!candidates.is_empty()).then_some(Resolution {
        candidates,
        ..Default::default()
      })
    }
    Some(
      ArgumentKind::Variable {
        access: Access::Unknown,
        ..
      }
      | ArgumentKind::Number,
    )
    | None => None,
    _ => analysis.summarize(site).resolution,
  }
}

pub(in crate::interface::application::engine) fn syntax_attempted(
  site: &engine::Occurrence,
  analysis: Option<&Analysis>,
  resolution: Option<&Resolution>,
) -> bool {
  analysis.is_some_and(|analysis| {
    analysis
      .resolutions(UseSite::Syntax(site.id))
      .next()
      .is_some()
      || resolution.is_some_and(|resolution| {
        resolution.candidates.iter().any(|symbol| {
          analysis
            .writes
            .range(
              (*symbol, ExecutionContextId(0))
                ..=(*symbol, ExecutionContextId(usize::MAX)),
            )
            .next()
            .is_some()
        })
      })
  })
}

pub(in crate::interface::application::engine) fn syntax_resolution(
  document: &engine::Document,
  site: &engine::Occurrence,
  analysis: Option<&Analysis>,
) -> Option<Resolution> {
  if matches!(site.access, Access::Definition | Access::Write) {
    document
      .symbols()
      .iter()
      .find(|symbol| symbol.definition == site.id)
      .map(|symbol| Resolution {
        candidates: [symbol.id].into(),
        ..Default::default()
      })
  } else {
    (site.kind == engine::OccurrenceKind::Command)
      .then(|| engine::analysis::invariant_command(&site.name))
      .flatten()
      .or_else(|| {
        let use_site = UseSite::Syntax(site.id);
        analysis.and_then(|analysis| analysis.summarize(use_site).resolution)
      })
  }
}

pub(in crate::interface::application::engine) fn join_resolution(
  left: &mut Option<Resolution>,
  right: Option<&Resolution>,
) {
  match (left.as_mut(), right) {
    (Some(left), Some(right)) => left.join(right),
    _ => *left = None,
  }
}
