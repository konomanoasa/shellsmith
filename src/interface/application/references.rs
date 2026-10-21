use super::engine::{self, Location, check_cancelled};
use super::{Application, Error};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;

impl Application {
  pub fn references(
    &mut self,
    path: &Path,
    offset: usize,
    include_declaration: bool,
    cancelled: &AtomicBool,
  ) -> Result<Vec<Location>, Error> {
    self.verify_discovery(cancelled)?;
    let names = self.prepare_names(path, cancelled)?;
    self.check_name_inputs(&names, cancelled)?;
    let id = self.document_id(path).ok_or_else(|| {
      Error::Message("The requested document is unavailable".into())
    })?;
    let document = self
      .workspace
      .document(id)
      .ok_or(engine::Error::UnknownDocument(id))?;
    if !document.source().is_char_boundary(offset) {
      return Err(engine::Error::InvalidPosition.into());
    }
    let selected =
      names
        .results
        .at(document.snapshot(), offset, false, cancelled)?;
    let targets = names.results.identities(&selected);
    let mut locations = BTreeMap::new();
    if let Some(selected) = targets
      && let Some(targets) =
        names.results.corresponding_names(&selected, cancelled)?
    {
      for sites in names.results.groups() {
        check_cancelled(cancelled)?;
        let Some((location, declaration)) =
          names.results.navigation(sites, &targets)
        else {
          continue;
        };
        if declaration && !include_declaration {
          continue;
        }
        if let Some((path, _)) = names.documents.get(&location.snapshot) {
          locations.insert(
            (path.clone(), location.bytes.start, location.bytes.end),
            location,
          );
        }
      }
    }
    self.check_name_inputs(&names, cancelled)?;
    Ok(locations.into_values().collect())
  }
}
