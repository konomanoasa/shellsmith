use super::{Application, Error};
use crate::interface::application::engine::{self, DocumentId};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

impl Application {
  pub fn document_links(
    &mut self,
    id: DocumentId,
    cancelled: &AtomicBool,
  ) -> Result<Vec<(engine::Location, PathBuf)>, Error> {
    self.analyze_document(id, cancelled)?;
    Ok(
      self
        .workspace
        .source_links(id)
        .into_iter()
        .filter_map(|(location, target)| {
          self
            .path(target.document)
            .map(|path| (location, path.to_path_buf()))
        })
        .collect(),
    )
  }
}
