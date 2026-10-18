mod command;
mod deferred;
mod execution;
mod expansion;
mod request;
mod resolver;
mod result;
pub(in crate::interface::application::engine) mod semantic;
mod sources;

pub(super) use command::invariant_command;
pub use command::special_builtin;
pub(super) use command::{
  UnattemptedCommand, invariant_arguments, unattempted_command,
};
use execution::Task;
pub(super) use request::validate;
pub use request::*;
pub(super) use resolver::analyze;
use resolver::{Frame, Resolver};
pub use result::*;
pub use semantic::*;
pub(super) use sources::deferred_source_input;
pub use sources::{SourceAnnotation, SourceDocument, SourceInput};
