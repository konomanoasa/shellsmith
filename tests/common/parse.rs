use shellsmith::interface::application::engine::{
  Analysis, AnalysisInputs, AnalysisRequest, Document, DocumentId, Error,
  ExecutionContextId, ExecutionKind, InitialEnvironment, InputQuery,
  InputResult, Revision, Snapshot, SourceDocument, SourceInput, Value,
  ValueIssue, Workspace,
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

/// Analyzes a source that must contain syntax errors.
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
