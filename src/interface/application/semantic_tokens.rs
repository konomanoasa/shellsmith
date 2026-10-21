use super::{Application, Error};
use crate::interface::application::engine::{self, DocumentId};
use std::sync::atomic::AtomicBool;

impl Application {
  pub fn classify(
    &mut self,
    id: DocumentId,
    cancelled: &AtomicBool,
  ) -> Result<Vec<engine::SemanticSpan>, Error> {
    self.analyze_document(id, cancelled)?;
    if let Some(analyses) = &self.analyses {
      if !self.analysis_inputs_current(analyses, cancelled)? {
        return Err(Error::Superseded);
      }
      let snapshot = self
        .workspace
        .document(id)
        .ok_or(engine::Error::UnknownDocument(id))?
        .snapshot();
      engine::AnalysisResults::new(
        std::iter::once(&self.workspace).chain(analyses.entries.values()),
      )
      .semantic_spans(snapshot)
      .map_err(Error::from)
    } else {
      self.workspace.semantic_spans(id).map_err(Error::from)
    }
  }
}
