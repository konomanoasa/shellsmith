mod analysis;
mod application;
mod discovery;
mod files;
mod inputs;
mod names;
mod session;

pub use application::{Application, Error, Failure, Settings};
pub(super) use names::NameCollection;
pub use session::{RequestOutcome, RequestScope, Session};

#[cfg(test)]
use crate::interface::fixtures;
