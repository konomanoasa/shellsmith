use super::{Application, Error};
use crate::interface::application::engine::{self, DocumentId};
use std::sync::atomic::AtomicBool;

impl Application {
  pub(in crate::interface::application) fn prepare_semantics(
    &mut self,
    id: DocumentId,
    cancelled: &AtomicBool,
  ) -> Result<(), Error> {
    match self.analyze_document(id, cancelled) {
      Err(Error::Engine(engine::Error::AnalysisLimit)) => Ok(()),
      result => result,
    }
  }

  pub fn classify(
    &mut self,
    id: DocumentId,
    cancelled: &AtomicBool,
  ) -> Result<Vec<engine::SemanticSpan>, Error> {
    if self.request_prepared {
      self.check_inputs(cancelled)?;
    } else if !self.inputs_current(cancelled)? {
      let path = self
        .path(id)
        .ok_or(engine::Error::UnknownDocument(id))?
        .to_path_buf();
      self.refresh_for(Some(&path), cancelled)?;
      self.discard_results();
    }
    self.prepare_semantics(id, cancelled)?;
    let entries: Vec<_> = self
      .analyses
      .iter()
      .flat_map(|analyses| analyses.entries.values())
      .collect();
    let spans = self.workspace.semantic_spans_with(id, &entries)?;
    self.check_inputs(cancelled)?;
    Ok(spans)
  }
}
