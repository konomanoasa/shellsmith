mod arguments;
mod builtins;
mod dispatch;
mod effects;
mod redirections;

pub(super) use arguments::syntax_arguments;
pub(in crate::interface::application::engine) use arguments::{
  UnattemptedCommand, invariant_arguments, unattempted_command,
};
pub(super) use redirections::{Redirections, index_heredocs};

pub(in crate::interface::application::engine) use dispatch::invariant_command;
pub use dispatch::special_builtin;
