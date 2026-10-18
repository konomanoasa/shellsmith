use super::Application;
use super::engine::SymbolDetails;

impl Application {
  pub fn workspace_symbols<'a>(
    &'a self,
    query: &'a str,
  ) -> impl Iterator<Item = SymbolDetails<'a>> {
    self.workspace.search_symbol_details(query.as_bytes())
  }
}
