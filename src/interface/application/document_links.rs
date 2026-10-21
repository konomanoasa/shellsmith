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
    let path = self
      .path(id)
      .ok_or(engine::Error::UnknownDocument(id))?
      .to_path_buf();
    let analyses = self.prepare_analysis(&path, cancelled)?;
    let snapshot = self
      .workspace
      .document(id)
      .ok_or(engine::Error::UnknownDocument(id))?
      .snapshot();
    let links = engine::AnalysisResults::new(analyses.entries.values())
      .sources(snapshot)?;
    let links = links
      .into_iter()
      .filter_map(|link| {
        if !link.complete || link.targets.len() != 1 {
          return None;
        }
        let target = link.targets.first()?;
        Some((link.location, self.path(target.document)?.to_path_buf()))
      })
      .collect();
    if !self.analysis_inputs_current(&analyses, cancelled)? {
      return Err(Error::Superseded);
    }
    Ok(links)
  }
}
