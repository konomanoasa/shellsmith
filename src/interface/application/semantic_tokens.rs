use super::{Application, Error};
use crate::interface::application::engine::{self, DocumentId};
use std::sync::atomic::AtomicBool;

impl Application {
  pub fn classify(
    &mut self,
    id: DocumentId,
    cancelled: &AtomicBool,
  ) -> Result<Vec<engine::SemanticSpan>, Error> {
    if !self.inputs_current(cancelled)? {
      if self.request_prepared {
        self.discard_results();
        return Err(Error::Superseded);
      }
      let path = self
        .path(id)
        .ok_or(engine::Error::UnknownDocument(id))?
        .to_path_buf();
      self.refresh_for(Some(&path), cancelled)?;
      self.discard_results();
    }
    self.analyze_document(id, cancelled)?;
    let spans = if let Some(analyses) = &self.analyses {
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
    }?;
    self.check_inputs(cancelled)?;
    Ok(spans)
  }
}
