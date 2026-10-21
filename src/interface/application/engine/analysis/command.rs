mod arguments;
mod builtins;
mod dispatch;
mod effects;
mod redirections;

pub(in crate::interface::application::engine) use arguments::invariant_arguments;
pub(super) use arguments::{operand_index, syntax_arguments};
pub(in crate::interface::application::engine) use effects::{
  UnattemptedCommand, unattempted_command,
};
pub(super) use redirections::{Redirections, index_heredocs};

pub(in crate::interface::application::engine) use dispatch::invariant_command;
pub use dispatch::{optional_reserved_word, reserved_word, special_builtin};
