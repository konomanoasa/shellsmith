use shellsmith::interface::application::engine::{
  Analysis, AnalysisInputs, AnalysisRequest, Document, DocumentId, Error,
  ExecutionContextId, ExecutionKind, InitialEnvironment, InputQuery,
  InputResult, NameOccurrence, NameRelations, Revision, Snapshot,
  SourceDocument, SourceInput, Value, ValueIssue, Workspace,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Default)]
pub struct Inputs {
  pub sources: BTreeMap<SourceInput, SourceDocument>,
  pub evaluated: BTreeMap<InputQuery, InputResult>,
  pub source_queries: Vec<SourceInput>,
  pub queries: Vec<InputQuery>,
}

impl Inputs {
  pub fn sources<'a>(
    documents: impl IntoIterator<Item = (&'a str, Document)>,
  ) -> Self {
    Self {
      sources: documents
        .into_iter()
        .map(|(path, document)| {
          assert!(path.starts_with('/'), "fixture paths must be absolute");
          (
            SourceInput {
              operand: Value::Literal(path.as_bytes().to_vec()),
              cwd: None,
              search_path: None,
              annotation: None,
            },
            SourceDocument::Resolved(Arc::new(document)),
          )
        })
        .collect(),
      ..Self::default()
    }
  }
}

impl AnalysisInputs for Inputs {
  fn source(&mut self, input: &SourceInput) -> Result<SourceDocument, Error> {
    self.source_queries.push(input.clone());
    Ok(match self.sources.get(input) {
      Some(SourceDocument::Resolved(document)) => {
        SourceDocument::Resolved(document.clone())
      }
      Some(SourceDocument::Unresolved(reason)) => {
        SourceDocument::Unresolved(reason.clone())
      }
      Some(SourceDocument::Failed(reason)) => {
        SourceDocument::Failed(reason.clone())
      }
      None => SourceDocument::Unresolved("No matching source input".into()),
    })
  }

  fn evaluate(&mut self, query: &InputQuery) -> Result<InputResult, Error> {
    self.queries.push(query.clone());
    Ok(
      self
        .evaluated
        .get(query)
        .cloned()
        .unwrap_or(InputResult::Unknown(ValueIssue::Dynamic)),
    )
  }
}

pub fn parse(id: u64, revision: u64, source: &str) -> Document {
  Document::parse(
    Snapshot {
      document: DocumentId(id),
      revision: Revision(revision),
    },
    source.as_bytes(),
    &AtomicBool::new(false),
  )
  .unwrap()
}

pub fn deferred(analysis: &Analysis, context: ExecutionContextId) -> bool {
  std::iter::successors(analysis.context(context), |context| {
    context.parent.and_then(|parent| analysis.context(parent))
  })
  .any(|context| matches!(context.kind, ExecutionKind::Deferred(_)))
}

pub fn analyze(source: &str) -> (Workspace, Analysis) {
  analyze_with(source, InitialEnvironment::empty())
}

pub fn analyze_with(
  source: &str,
  initial: InitialEnvironment,
) -> (Workspace, Analysis) {
  let document = parse(1, 1, source);
  assert!(!document.has_syntax_errors(), "{source}");
  analyze_document(document, initial)
}

pub fn analyze_damaged(source: &str) -> (Workspace, Analysis) {
  let document = parse(1, 1, source);
  assert!(document.has_syntax_errors(), "{source}");
  analyze_document(document, InitialEnvironment::empty())
}

fn analyze_document(
  document: Document,
  initial: InitialEnvironment,
) -> (Workspace, Analysis) {
  let mut request = AnalysisRequest::new(document.snapshot());
  request.initial = initial;
  let mut workspace = Workspace::default();
  workspace.insert(document).unwrap();
  let analysis = workspace
    .analyze(request, &mut Inputs::default(), &AtomicBool::new(false))
    .unwrap();
  (workspace, analysis)
}

pub fn name_sites_at(
  names: &NameRelations,
  snapshot: Snapshot,
  offset: usize,
) -> Vec<&NameOccurrence> {
  let mut sites = Vec::new();
  let mut width = usize::MAX;
  for site in &names.occurrences {
    let Some(location) = site.spelling.editable().filter(|location| {
      location.snapshot == snapshot && location.bytes.contains(&offset)
    }) else {
      continue;
    };
    if location.bytes.len() < width {
      sites.clear();
      width = location.bytes.len();
    }
    if location.bytes.len() == width {
      sites.push(site);
    }
  }
  sites
}

pub fn sources(
  workspace: &Workspace,
  id: DocumentId,
) -> Vec<shellsmith::interface::application::engine::SourceResolution> {
  shellsmith::interface::application::engine::AnalysisResults::new([workspace])
    .sources(workspace.document(id).unwrap().snapshot())
    .unwrap()
}

pub fn values(
  analysis: &Analysis,
  occurrence: shellsmith::interface::application::engine::OccurrenceId,
) -> impl Iterator<Item = (ExecutionContextId, &Value)> {
  analysis.contexts().iter().filter_map(move |context| {
    analysis
      .value(occurrence, context.id)
      .map(|value| (context.id, value))
  })
}

pub fn references(
  analysis: &Analysis,
  definition: shellsmith::interface::application::engine::SymbolId,
) -> impl Iterator<Item = &shellsmith::interface::application::engine::ResolvedUse>
{
  analysis
    .uses()
    .filter(move |site| site.resolution.candidates.contains(&definition))
}
