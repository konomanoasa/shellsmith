pub(in crate::interface::application::engine) mod derived;
mod documents;
mod names;
pub(in crate::interface::application::engine) mod parser;
mod symbols;

pub use derived::{DerivedOrigin, OriginMapping, OriginPart};
pub use documents::Workspace;
pub use names::*;
pub use parser::Document;
pub use symbols::SymbolDetails;
