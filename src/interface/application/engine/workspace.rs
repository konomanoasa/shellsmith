pub(in crate::interface::application::engine) mod derived;
mod documents;
mod name_results;
mod names;
pub(in crate::interface::application::engine) mod parser;
mod projection;
mod results;
mod symbols;

pub use derived::{DerivedOrigin, OriginMapping, OriginPart};
pub use documents::Workspace;
pub use name_results::*;
pub use names::*;
pub use parser::Document;
pub use results::*;
pub use symbols::SymbolDetails;
