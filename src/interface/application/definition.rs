use super::engine::{DocumentId, Location};
use super::{Application, Error, check};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

impl Application {
  pub fn definitions(
    &mut self,
    id: DocumentId,
    offset: usize,
    cancelled: &AtomicBool,
  ) -> Result<Vec<Location>, Error> {
    self.analyze_document(id, cancelled)?;
    let snapshot = self
      .workspace
      .document(id)
      .ok_or(super::engine::Error::UnknownDocument(id))?
      .snapshot();
    let resolution = self.workspace.resolution_at(snapshot, offset)?;
    check(cancelled)?;
    let Some(resolution) = resolution else {
      return Ok(Vec::new());
    };
    if !resolution.issues.is_empty() || !resolution.alternatives.is_empty() {
      return Ok(Vec::new());
    }
    let mut locations = BTreeMap::new();
    for candidate in resolution.candidates {
      check(cancelled)?;
      let Some(location) = self
        .workspace
        .symbol(candidate)
        .and_then(|symbol| self.workspace.occurrence(symbol.definition))
        .and_then(|site| self.workspace.navigation_location(&site.location))
      else {
        return Ok(Vec::new());
      };
      let Some(path) = self.path(location.snapshot.document) else {
        return Ok(Vec::new());
      };
      locations.insert(
        (path.to_path_buf(), location.bytes.start, location.bytes.end),
        location,
      );
    }
    Ok(locations.into_values().collect())
  }
}
