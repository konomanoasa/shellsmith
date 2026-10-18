use shellsmith::interface::application::engine::{
  Analysis, AnalysisRequest, Document, DocumentId, ExecutionContextId,
  ExecutionKind, InitialEnvironment, Revision, Snapshot, Workspace,
};
use std::sync::atomic::AtomicBool;

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
  let mut request = AnalysisRequest::new(document.snapshot());
  request.initial = initial;
  let mut workspace = Workspace::default();
  workspace.insert(document).unwrap();
  workspace.set_analysis_request(request).unwrap();
  let analysis = workspace.analyze(&AtomicBool::new(false)).unwrap();
  (workspace, analysis)
}
