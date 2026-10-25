use super::engine::{Location, check_cancelled};
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
    let names = self.prepare_names(path, cancelled)?;
    self.check_name_inputs(&names, cancelled)?;
    let selected = self
      .name_sites(&names, path, offset, false, cancelled)?
      .ok_or_else(|| {
        Error::Message("The requested document is unavailable".into())
      })?;
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
