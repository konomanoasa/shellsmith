use super::engine::{DocumentId, Location, check_cancelled};
use super::{Application, Error};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

impl Application {
  pub fn definitions(
    &mut self,
    id: DocumentId,
    offset: usize,
    cancelled: &AtomicBool,
  ) -> Result<Vec<Location>, Error> {
    let path = self
      .path(id)
      .ok_or(super::engine::Error::UnknownDocument(id))?
      .to_path_buf();
    let previous = self
      .workspace
      .shared_document(id)
      .ok_or(super::engine::Error::UnknownDocument(id))?;
    let analyses = self.prepare_analysis(&path, cancelled)?;
    let document = self
      .workspace
      .document(id)
      .ok_or(super::engine::Error::UnknownDocument(id))?;
    if document.snapshot() != previous.snapshot()
      && document.source() != previous.source()
    {
      return Err(Error::Superseded);
    }
    let snapshot = document.snapshot();
    let resolution =
      super::engine::AnalysisResults::new(analyses.entries.values())
        .resolution_at(snapshot, offset)?;
    check_cancelled(cancelled)?;
    let mut locations = BTreeMap::new();
    if let Some(resolution) = resolution.filter(|result| {
      result.compatible
        && result.issues.is_empty()
        && result.alternatives.is_empty()
    }) {
      for location in resolution.definitions {
        check_cancelled(cancelled)?;
        let Some(location) = location else {
          locations.clear();
          break;
        };
        let Some(path) = self.path(location.snapshot.document) else {
          locations.clear();
          break;
        };
        locations.insert(
          (path.to_path_buf(), location.bytes.start, location.bytes.end),
          location,
        );
      }
    }
    if !self.analysis_inputs_current(&analyses, cancelled)? {
      return Err(Error::Superseded);
    }
    Ok(locations.into_values().collect())
  }
}
