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
    self.workspace.semantic_spans(id).map_err(Error::from)
  }
}
