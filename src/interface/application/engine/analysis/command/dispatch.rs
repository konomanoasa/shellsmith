use super::redirections::Redirections;
use std::collections::BTreeSet;
use tree_sitter::Node;

use super::super::execution::{Environment, Flow, Transfer, alternative};
use super::super::execution::{children, damage_outside};
use super::super::expansion::{Argument, Sharing};
use super::super::{Frame, Resolver, Task};
use crate::interface::application::engine::word;
use crate::interface::application::engine::workspace::parser;
use crate::interface::application::engine::{
  Alternative, ArgumentKind, ExecutionKind, OccurrenceId, Resolution,
  ResolutionIssue, SymbolId, UseSite, Value, ValueIssue,
};

#[derive(Clone, Copy)]
enum ArgumentUse {
  Effects,
  Values,
  Fields,
  Command,
}

fn argument_use(
  words: &[Argument<'_>],
  arguments: &[Node<'_>],
  source: &[u8],
  env: &Environment,
) -> ArgumentUse {
  let mut words = words.iter();
  let mut suppress = false;
  loop {
    let Some(command) = words.next() else {
      return ArgumentUse::Command;
    };
    let resolution =
      Resolver::command_resolution(&command.value, env, suppress);
    if !resolution.candidates.is_empty() {
      return if resolution.alternatives.contains(&Alternative::Builtin) {
        ArgumentUse::Values
      } else {
        ArgumentUse::Fields
      };
    }
    if !resolution.alternatives.contains(&Alternative::Builtin) {
      return ArgumentUse::Effects;
    }
    let Value::Literal(name) = &command.value else {
      return ArgumentUse::Effects;
    };
    if name != b"command" {
      if super::arguments::numeric_argument(name) {
        let fields = words
          .clone()
          .filter(|arg| arg.count.boundary().is_ok())
          .count()
          + arguments
            .iter()
            .filter(|node| word::has_guaranteed_field(**node, source))
            .count();
        if fields > 1 {
          return ArgumentUse::Effects;
        }
      }
      let first = words
        .as_slice()
        .first()
        .map(|arg| arg.value.clone())
        .or_else(|| {
          arguments
            .first()
            .map(|node| word::literal_value(*node, source))
        });
      if (name == b"set" && first == Some(Value::Literal(b"--".to_vec())))
        || (name == b"echo" && env.io.output.is_some())
      {
        return ArgumentUse::Fields;
      }
      return if super::builtins::uses_arguments(name) || env.io.output.is_some()
      {
        ArgumentUse::Values
      } else {
        ArgumentUse::Effects
      };
    }
    match command_options(words.as_slice()) {
      Ok(options) if !options.query => {
        words = words.as_slice()[options.operands..].iter();
      }
      Ok(_) => return ArgumentUse::Values,
      Err(_) => return ArgumentUse::Effects,
    }
    suppress = true;
  }
}

enum ForwardOption {
  Name,
  End,
  Search,
  Query,
  Invalid,
  Unknown,
}

fn forward_option(argument: &Argument<'_>) -> ForwardOption {
  if argument.count.boundary().is_err() {
    return ForwardOption::Unknown;
  }
  let Value::Literal(value) = &argument.value else {
    return if argument.value.may_start_with(b"-") {
      ForwardOption::Unknown
    } else {
      ForwardOption::Name
    };
  };
  if value == b"--" {
    ForwardOption::End
  } else if !value.starts_with(b"-") || value == b"-" {
    ForwardOption::Name
  } else if value[1..].iter().all(|byte| b"pvV".contains(byte)) {
    if value.contains(&b'v') || value.contains(&b'V') {
      ForwardOption::Query
    } else {
      ForwardOption::Search
    }
  } else {
    ForwardOption::Invalid
  }
}

pub(super) struct CommandOptions {
  /// The index of the first operand.
  pub operands: usize,
  pub query: bool,
  pub verbose: bool,
  pub standard: bool,
}

pub(super) fn command_options(
  args: &[Argument<'_>],
) -> Result<CommandOptions, ResolutionIssue> {
  let mut options = CommandOptions {
    operands: args.len(),
    query: false,
    verbose: false,
    standard: false,
  };
  for (index, argument) in args.iter().enumerate() {
    match forward_option(argument) {
      ForwardOption::Name => {
        options.operands = index;
        break;
      }
      ForwardOption::End => {
        if args
          .get(index + 1)
          .is_some_and(|operand| operand.count.boundary().is_err())
        {
          return Err(ResolutionIssue::DynamicName);
        }
        options.operands = index + 1;
        break;
      }
      ForwardOption::Search => options.standard = true,
      ForwardOption::Query => {
        options.query = true;
        options.verbose |= matches!(
          &argument.value,
          Value::Literal(value) if value.contains(&b'V')
        );
      }
      ForwardOption::Invalid => return Err(ResolutionIssue::UnknownEffect),
      ForwardOption::Unknown => return Err(ResolutionIssue::DynamicName),
    }
  }
  Ok(options)
}

type Invocation<'a> =
  Box<dyn FnOnce(&mut Resolver<'a>, Environment) -> Task<'a> + 'a>;

#[derive(Clone)]
struct CallArguments<'a> {
  node: Node<'a>,
  command: Node<'a>,
  arguments: Vec<Node<'a>>,
  words: Vec<Argument<'a>>,
  frame: Frame,
}

impl<'a> Resolver<'a> {
  pub(in crate::interface::application::engine::analysis) fn command(
    &mut self,
    node: Node<'a>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    env.substitution_status = None;
    let prefix = node.child_by_field_name("prefix");
    let suffix = node
      .child_by_field_name("suffix")
      .map_or_else(Vec::new, children);
    let redirects = redirects(node);
    let Some(command) = parser::command_word(node) else {
      return self.without_command(prefix, redirects, env, frame);
    };
    let arguments = suffix
      .into_iter()
      .filter(|word| word.kind() == "word")
      .collect();
    let call = CallArguments {
      node,
      command,
      arguments,
      words: Vec::new(),
      frame,
    };
    self.expand_call(command, call, env)
  }

  fn expand_call(
    &mut self,
    node: Node<'a>,
    mut call: CallArguments<'a>,
    env: Environment,
  ) -> Task<'a> {
    let words = std::mem::take(&mut call.words);
    self.arguments(
      vec![node].into_iter(),
      words,
      env,
      call.frame,
      Sharing::None,
      Box::new(move |resolver, words, env| {
        resolver.command_arguments(CallArguments { words, ..call }, env)
      }),
    )
  }

  fn command_arguments(
    &mut self,
    mut call: CallArguments<'a>,
    env: Environment,
  ) -> Task<'a> {
    // Later state changes must not reintroduce choices captured by an argument.
    let shared = !env.alternatives.is_empty()
      && call.node.child_by_field_name("prefix").is_none()
      && redirects(call.node).is_empty()
      && self
        .expansion_targets(
          call.arguments.iter().copied().filter_map(|node| {
            if node.child_by_field_name("name").is_some() {
              node.child_by_field_name("value")
            } else {
              Some(node)
            }
          }),
          call.frame,
        )
        .is_empty();
    let source = self.workspace.source(call.frame.snapshot.document);
    let usage = argument_use(&call.words, &call.arguments, source, &env);
    let shared = if !shared {
      Sharing::None
    } else if matches!(usage, ArgumentUse::Fields) {
      Sharing::Fields
    } else {
      Sharing::Assignments
    };
    if !env.alternatives.is_empty() {
      let needed = call.arguments.iter().copied().enumerate().filter_map(
        |(index, argument)| {
          (matches!(usage, ArgumentUse::Values | ArgumentUse::Fields)
            || (matches!(usage, ArgumentUse::Command) && index == 0)
            || (!matches!(usage, ArgumentUse::Command)
              && !self.expansion_targets([argument], call.frame).is_empty()))
          .then_some(argument)
        },
      );
      let needed: Vec<_> = needed.collect();
      let mut names = BTreeSet::new();
      for argument in needed {
        if shared == Sharing::None {
          names.extend(self.value_dependencies([argument], call.frame));
          continue;
        }
        match self.conditional_value(
          argument,
          &env,
          call.frame,
          shared == Sharing::Fields,
        ) {
          Ok(Some(_)) => {}
          Ok(None) => {
            names.extend(self.value_dependencies([argument], call.frame));
          }
          Err(error) => {
            self.error = Some(error);
            return Task::Done(Flow::default());
          }
        }
      }
      if env.alternatives.depends(&names) {
        return self.each_alternative(env, &names, move |resolver, env| {
          resolver.command_arguments(call.clone(), env)
        });
      }
      if matches!(usage, ArgumentUse::Command) && !call.arguments.is_empty() {
        let first = call.arguments.remove(0);
        return self.expand_call(first, call, env);
      }
    }
    let CallArguments {
      node,
      command,
      arguments,
      words,
      frame,
    } = call;
    self.arguments(
      arguments.into_iter(),
      words,
      env,
      frame,
      shared,
      Box::new(move |resolver, words, env| {
        resolver.expanded_command(node, command, words, env, frame)
      }),
    )
  }

  fn expanded_command(
    &mut self,
    node: Node<'a>,
    command: Node<'a>,
    mut words: Vec<Argument<'a>>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let prefix = node.child_by_field_name("prefix");
    let redirects = redirects(node);
    if words.is_empty() {
      return self.without_command(prefix, redirects, env, frame);
    }
    let name = Argument::take_first(&mut words);
    let args = words;
    let assignments = assignments(prefix);
    let source = self.workspace.source(frame.snapshot.document);
    let search = Self::name_search(command, source, &name, &env);
    let io = redirection_persistence(&name, &args, &env)
      .save(&redirects, source, &mut env);
    Task::sequence(redirects, env, frame).then(move |resolver, mut flow| {
      let Some(env) = flow.take(Transfer::Next) else {
        return Task::Done(flow);
      };
      let resolution = Self::search_resolution(&name, &env, search);
      let scope = prefix_scope(&name);
      let frame = resolver.temporary_prefix(node, &assignments, scope, frame);
      resolver
        .prefix_assignments(
          assignments.into_iter(),
          scope,
          env,
          frame,
          Box::new(move |resolver, env| {
            if let Some(id) = resolver.site(command, frame) {
              resolver.record_use(
                UseSite::Syntax(id),
                frame.context,
                resolution,
                None,
                &env,
              );
            }
            resolver.invoke(node, &name, &args, env, frame, search)
          }),
        )
        .then(move |_, mut called| {
          if let Some(io) = io {
            io.restore(&mut called);
          }
          flow.join(called);
          Task::Done(flow)
        })
    })
  }

  fn prefix_assignments(
    &mut self,
    mut assignments: std::vec::IntoIter<Node<'a>>,
    scope: PrefixScope,
    env: Environment,
    frame: Frame,
    invoke: Invocation<'a>,
  ) -> Task<'a> {
    let Some(assignment) = assignments.next() else {
      return invoke(self, env);
    };
    Self::assignment_value(assignment, env, frame).then_word(
      move |resolver, mut expanded| {
        let before =
          (scope != PrefixScope::Persistent).then(|| expanded.env.clone());
        let name = assignment.child_by_field_name("name").map(|name| {
          word::lexical(
            name,
            resolver.workspace.source(frame.snapshot.document),
          )
        });
        if scope != PrefixScope::Persistent
          && let Some(name) = &name
          && expanded.env.readonly(name) != Some(true)
        {
          let certain = scope == PrefixScope::Temporary
            && expanded.env.readonly(name).is_none();
          expanded.env.exports.mark(name, true, certain);
        }
        resolver.assign_value(assignment, expanded, frame).then(
          move |resolver, mut flow| {
            let Some(env) = flow.take(Transfer::Next) else {
              return Task::Done(flow);
            };
            resolver
              .prefix_assignments(assignments, scope, env, frame, invoke)
              .then(move |_, mut called| {
                if let (Some(name), Some(before)) = (name, before) {
                  for env in called.environments_mut() {
                    env.restore_temporary(
                      &name,
                      &before,
                      scope == PrefixScope::Uncertain,
                    );
                  }
                }
                flow.join(called);
                Task::Done(flow)
              })
          },
        )
      },
    )
  }

  fn without_command(
    &mut self,
    prefix: Option<Node<'a>>,
    redirects: Vec<Node<'a>>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let source = self.workspace.source(frame.snapshot.document);
    let io = Redirections::new(&redirects, source, &mut env, false);
    Task::sequence(redirects, env, frame).then(move |_, mut flow| {
      let Some(env) = flow.take(Transfer::Next) else {
        return Task::Done(flow);
      };
      Task::sequence(assignments(prefix), env, frame).then(
        move |_, mut assigned| {
          for env in assigned.environments_mut() {
            let status = env
              .substitution_status
              .take()
              .unwrap_or(Value::Literal(b"0".to_vec()));
            env.set_status(status);
          }
          io.restore(&mut assigned);
          flow.join(assigned);
          Task::Done(flow)
        },
      )
    })
  }

  fn temporary_prefix(
    &mut self,
    node: Node<'_>,
    assignments: &[Node<'_>],
    scope: PrefixScope,
    frame: Frame,
  ) -> Frame {
    if assignments.is_empty() || scope != PrefixScope::Temporary {
      return frame;
    }
    let inner = self.execution(
      node,
      frame,
      ExecutionKind::TemporaryAssignments,
      frame.snapshot,
    );
    for (name, id) in self.assigned_names(assignments, frame) {
      self
        .result
        .temporary
        .entry(inner.context)
        .or_default()
        .insert(name, UseSite::Syntax(id));
    }
    inner
  }

  /// Literal names of prefix assignments, with their name occurrences.
  fn assigned_names(
    &self,
    assignments: &[Node<'_>],
    frame: Frame,
  ) -> Vec<(Vec<u8>, OccurrenceId)> {
    assignments
      .iter()
      .filter_map(|assignment| {
        let id = self.site(assignment.child_by_field_name("name")?, frame)?;
        match &self.workspace.occurrence(id)?.name {
          Value::Literal(name) => Some((name.clone(), id)),
          _ => None,
        }
      })
      .collect()
  }

  pub(super) fn command_resolution(
    name: &Value,
    env: &Environment,
    suppress: bool,
  ) -> Resolution {
    let Value::Literal(bytes) = name else {
      let mut result = Resolution::default();
      if matches!(name, Value::Unknown(_)) {
        result.alternatives.insert(Alternative::ExternalCommand);
        result.issues.insert(ResolutionIssue::DynamicName);
        return result;
      }
      let constraints = name.constraints();
      if constraints.iter().any(|choice| !choice.exact) {
        result.alternatives.insert(Alternative::ExternalCommand);
      }
      for candidate in command_names(name, env) {
        result.join(&Self::command_resolution(
          &Value::Literal(candidate),
          env,
          suppress,
        ));
      }
      if !suppress
        && constraints
          .iter()
          .any(|choice| !choice.exact && !choice.required.contains(&b'/'))
      {
        result.join(&env.functions.fallback.resolution);
        if env
          .functions
          .patterns
          .iter()
          .any(|pattern| name.intersects(pattern))
          && let Some(unknown) = &env.functions.uncertain
        {
          result.join(&unknown.resolution);
        }
        result.alternatives.remove(&Alternative::Unset);
      }
      return result;
    };
    let name = bytes;
    if name.is_empty() || name.contains(&b'/') {
      alternative(Alternative::ExternalCommand)
    } else if special_builtin(name) {
      alternative(Alternative::Builtin)
    } else if unspecified_command(name) {
      unspecified_resolution()
    } else if suppress {
      utility_resolution(name)
    } else {
      let mut result = env.functions.get(name).resolution.clone();
      if result.alternatives.remove(&Alternative::Unset) {
        result.join(&utility_resolution(name));
      }
      result
    }
  }

  /// POSIX leaves optional reserved words unspecified in command position.
  pub(super) fn name_search(
    command: Node<'_>,
    source: &[u8],
    name: &Value,
    env: &Environment,
  ) -> Search {
    let Some(word) = command
      .named_child(0)
      .filter(|_| command.kind() == "cmd_name")
    else {
      return Search::Normal;
    };
    let word = word::lexical(word, source);
    let reserved = if word == b"time" {
      // `time` runs the time utility wherever that utility would run.
      let resolution = Self::command_resolution(name, env, false);
      !resolution.candidates.is_empty()
        || !resolution.issues.is_empty()
        || resolution.alternatives != [Alternative::ExternalCommand].into()
    } else {
      optional_reserved_word(&word)
    };
    if reserved {
      Search::Reserved
    } else {
      Search::Normal
    }
  }

  pub(super) fn search_resolution(
    name: &Value,
    env: &Environment,
    search: Search,
  ) -> Resolution {
    match search {
      Search::Normal => Self::command_resolution(name, env, false),
      Search::Reserved => unspecified_resolution(),
      Search::SuppressFunctions | Search::Standard => {
        Self::command_resolution(name, env, true)
      }
    }
  }

  fn invoke(
    &mut self,
    node: Node<'a>,
    name: &Value,
    args: &[Argument<'a>],
    env: Environment,
    frame: Frame,
    search: Search,
  ) -> Task<'a> {
    let resolution = Self::search_resolution(name, &env, search);
    if !only_builtin(&resolution) {
      for arg in args {
        self.argument_role(arg, None, frame);
      }
      self.non_source(node, frame);
    }
    let candidates = resolution
      .candidates
      .iter()
      .copied()
      .collect::<Vec<_>>()
      .into_iter();
    let name = name.clone();
    let args = args.to_vec();
    let mut call = env.clone();
    call.set_positionals(&args);
    self
      .invoke_candidates(node, candidates, &call, Flow::default(), frame)
      .then(move |resolver, mut flow| {
        for state in flow.environments_mut() {
          state.positional.clone_from(&env.positional);
          state.option_cursor.clone_from(&env.option_cursor);
          state.options_reset = env.options_reset;
          state.alternatives.restore_positionals(&env.alternatives);
        }
        let task = if resolution.alternatives.contains(&Alternative::Builtin) {
          let names = builtin_names(&name, &env, search != Search::Normal);
          resolver.invoke_builtins(
            node,
            names.into_iter(),
            args.clone(),
            env.clone(),
            Flow::default(),
            frame,
          )
        } else {
          Task::Done(Flow::default())
        };
        task.then(move |resolver, builtin| {
          flow.join(builtin);
          if resolution
            .alternatives
            .contains(&Alternative::ExternalCommand)
          {
            let external = resolver.external_command(
              node,
              &name,
              &args,
              env.clone(),
              search,
              frame,
            );
            flow.add(Transfer::Next, external);
          }
          if unknown_utility(&resolution) {
            let issue = if resolution.issues
              == [ResolutionIssue::UnresolvedSource].into()
            {
              ResolutionIssue::UnresolvedSource
            } else {
              ResolutionIssue::UnknownEffect
            };
            flow.join(resolver.opaque(node, env, frame, issue, None));
          }
          Task::Done(flow)
        })
      })
  }

  /// Output is known only for standard utilities that `command -p` selects.
  fn external_command(
    &mut self,
    node: Node<'_>,
    name: &Value,
    args: &[Argument<'_>],
    mut env: Environment,
    search: Search,
    frame: Frame,
  ) -> Environment {
    let standard = match name {
      Value::Literal(name) if search == Search::Standard => Some(name),
      _ => None,
    };
    let (output, trace, expression) = match standard {
      Some(name) => self.utility_output(node, name, args, &env, frame),
      None if search == Search::Standard => {
        (Value::Unknown(ValueIssue::Dynamic), None, None)
      }
      None => (Value::Unknown(ValueIssue::UnsupportedExpansion), None, None),
    };
    self.emit(&mut env, output, trace, expression);
    if !standard.is_some_and(|name| super::builtins::preserves_input(name)) {
      env.io.consume_input(None);
    }
    env.io.may_write_stderr();
    env.set_status(Value::Unknown(ValueIssue::Dynamic));
    env
  }

  fn invoke_builtins(
    &mut self,
    node: Node<'a>,
    mut names: std::vec::IntoIter<Vec<u8>>,
    args: Vec<Argument<'a>>,
    env: Environment,
    mut flow: Flow,
    frame: Frame,
  ) -> Task<'a> {
    let Some(name) = names.next() else {
      return Task::Done(flow);
    };
    self
      .utility(node, &name, args.clone(), env.clone(), frame)
      .then(move |resolver, result| {
        flow.join(result);
        resolver.invoke_builtins(node, names, args, env, flow, frame)
      })
  }

  fn invoke_candidates(
    &mut self,
    node: Node<'a>,
    mut candidates: std::vec::IntoIter<SymbolId>,
    env: &Environment,
    mut flow: Flow,
    frame: Frame,
  ) -> Task<'a> {
    let next = candidates
      .find_map(|id| self.index.function_body(id).map(|body| (id, body)));
    let Some((id, body)) = next else {
      return Task::Done(flow);
    };
    let task = if self
      .ancestors(frame)
      .take_while(|context| !matches!(context.kind, ExecutionKind::Deferred(_)))
      .any(|context| context.kind == ExecutionKind::Function(id))
    {
      let targets = self.potential_effects(body, id.snapshot, env, frame);
      let mut env = env.clone();
      env.io.consume_input(None);
      if let Some(output) = &mut env.io.output {
        output.fail(ValueIssue::UnsupportedExpansion);
      }
      env.set_status(Value::Unknown(ValueIssue::Dynamic));
      self.gap(node, frame, ResolutionIssue::Recursion);
      self.invalidate(
        node,
        &mut env,
        frame,
        ResolutionIssue::Recursion,
        ValueIssue::Dynamic,
        targets,
      );
      Task::Done(Flow::next(env))
    } else {
      let inner =
        self.execution(node, frame, ExecutionKind::Function(id), id.snapshot);
      Task::eval(body, env.clone(), inner)
        .then(|_, returned| Task::Done(returned.returned()))
    };
    let env = env.clone();
    task.then(move |resolver, returned| {
      flow.join(returned);
      resolver.invoke_candidates(node, candidates, &env, flow, frame)
    })
  }

  pub(super) fn forward(
    &mut self,
    node: Node<'a>,
    mut args: std::vec::IntoIter<Argument<'a>>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let options = match command_options(args.as_slice()) {
      Ok(options) => options,
      Err(issue) => {
        let value_issue = super::arguments::argument_issue(args.as_slice());
        for arg in args {
          self.argument_role(&arg, None, frame);
        }
        self.non_source(node, frame);
        return Task::Done(self.opaque(node, env, frame, issue, value_issue));
      }
    };
    for arg in args.by_ref().take(options.operands) {
      self.argument_role(&arg, None, frame);
    }
    if options.query {
      return self.command_query(node, args, options.verbose, env, frame);
    }

    let Some(command) = args.next() else {
      self.non_source(node, frame);
      return Task::Done(Flow::next(env));
    };
    let name = command.value.clone();
    self.record_argument_use(
      &command,
      Self::command_resolution(&name, &env, true),
      &env,
      frame,
    );
    self.argument_role(
      &command,
      Some(ArgumentKind::Command {
        name: command.value.clone(),
      }),
      frame,
    );
    if name == Value::Literal(b"command".to_vec()) {
      return Task::Done(Flow::next(env)).then(move |resolver, mut flow| {
        resolver.forward(
          node,
          args,
          flow.take(Transfer::Next).expect("forward input"),
          frame,
        )
      });
    }
    self.invoke(
      node,
      &name,
      args.as_slice(),
      env,
      frame,
      if options.standard {
        Search::Standard
      } else {
        Search::SuppressFunctions
      },
    )
  }

  fn command_query(
    &mut self,
    node: Node<'a>,
    mut args: std::vec::IntoIter<Argument<'a>>,
    verbose: bool,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let mut output = Value::Unknown(
      super::arguments::argument_issue(args.as_slice())
        .unwrap_or(ValueIssue::UnsupportedExpansion),
    );
    if args.len() == 1 {
      let arg = args.next().expect("query operand");
      if arg.count.boundary().is_ok() {
        let resolution = Self::command_resolution(&arg.value, &env, false);
        if !verbose {
          output = query_output(&arg.value, &resolution);
        }
        self.record_argument_use(&arg, resolution, &env, frame);
        self.argument_role(
          &arg,
          Some(ArgumentKind::Command {
            name: arg.value.clone(),
          }),
          frame,
        );
      } else {
        self.argument_role(&arg, None, frame);
      }
    } else {
      for arg in args {
        self.argument_role(&arg, None, frame);
      }
    }
    self.emit(&mut env, output, None, None);
    self.non_source(node, frame);
    env.set_status(Value::Unknown(ValueIssue::Dynamic));
    Task::Done(Flow::next(env))
  }
}

fn assignments(prefix: Option<Node<'_>>) -> Vec<Node<'_>> {
  prefix
    .into_iter()
    .flat_map(children)
    .filter(|node| node.kind() == "assignment_word")
    .collect()
}

/// Redirections and damaged syntax, which run in place before the command.
fn redirects(node: Node<'_>) -> Vec<Node<'_>> {
  let mut redirects: Vec<_> = ["prefix", "suffix"]
    .into_iter()
    .filter_map(|field| node.child_by_field_name(field))
    .flat_map(children)
    .filter(|node| node.kind() != "word" && node.kind() != "assignment_word")
    .chain(damage_outside(node, &["name", "prefix", "suffix", "word"]))
    .collect();
  redirects.sort_by_key(Node::start_byte);
  redirects
}

const UNSPECIFIED: &[&[u8]] = &[
  b"alloc",
  b"autoload",
  b"bind",
  b"bindkey",
  b"builtin",
  b"bye",
  b"caller",
  b"cap",
  b"chdir",
  b"clone",
  b"comparguments",
  b"compcall",
  b"compctl",
  b"compdescribe",
  b"compfiles",
  b"compgen",
  b"compgroups",
  b"complete",
  b"compound",
  b"compquote",
  b"comptags",
  b"comptry",
  b"compvalues",
  b"declare",
  b"dirs",
  b"disable",
  b"disown",
  b"dosh",
  b"echotc",
  b"echoti",
  b"enum",
  b"float",
  b"help",
  b"history",
  b"hist",
  b"integer",
  b"let",
  b"local",
  b"login",
  b"logout",
  b"map",
  b"mapfile",
  b"nameref",
  b"popd",
  b"print",
  b"pushd",
  b"readarray",
  b"repeat",
  b"savehistory",
  b"source",
  b"shopt",
  b"stop",
  b"suspend",
  b"typeset",
  b"whence",
];

#[must_use]
pub fn unspecified_command(name: &[u8]) -> bool {
  UNSPECIFIED.contains(&name)
}

fn unspecified_resolution() -> Resolution {
  let mut result = alternative(Alternative::External);
  result.issues.insert(ResolutionIssue::UnknownEffect);
  result
}

const RESERVED: &[&[u8]] = &[
  b"!", b"{", b"}", b"case", b"do", b"done", b"elif", b"else", b"esac", b"fi",
  b"for", b"if", b"in", b"then", b"until", b"while",
];

const OPTIONAL_RESERVED: &[&[u8]] =
  &[b"[[", b"]]", b"function", b"namespace", b"select", b"time"];

#[must_use]
pub fn reserved_word(word: &[u8]) -> bool {
  RESERVED.contains(&word)
}

#[must_use]
pub fn optional_reserved_word(word: &[u8]) -> bool {
  OPTIONAL_RESERVED.contains(&word)
}

/// How long prefix assignments last (XCU 2.9.1.2).
#[derive(Clone, Copy, Eq, PartialEq)]
enum PrefixScope {
  /// Special built-ins keep the assignments.
  Persistent,
  /// An unknown name may select a special builtin.
  Uncertain,
  Temporary,
}

fn prefix_scope(name: &Value) -> PrefixScope {
  if special_name(name) {
    PrefixScope::Persistent
  } else if SPECIAL.iter().any(|special| name.can_equal(special)) {
    PrefixScope::Uncertain
  } else {
    PrefixScope::Temporary
  }
}

const SPECIAL: &[&[u8]] = &[
  b":",
  b".",
  b"break",
  b"continue",
  b"eval",
  b"exec",
  b"exit",
  b"export",
  b"readonly",
  b"return",
  b"set",
  b"shift",
  b"times",
  b"trap",
  b"unset",
];

#[must_use]
pub fn special_builtin(name: &[u8]) -> bool {
  SPECIAL.contains(&name)
}

/// Intrinsic utilities (XCU 1.7), which a PATH search never replaces.
const INTRINSIC: &[&[u8]] = &[
  b"alias", b"bg", b"cd", b"command", b"fc", b"fg", b"getopts", b"hash",
  b"jobs", b"kill", b"read", b"type", b"ulimit", b"umask", b"unalias", b"wait",
];

/// PATH may select external implementations of these built-ins.
const REGULAR: &[&[u8]] = &[
  b"[", b"echo", b"false", b"newgrp", b"printf", b"pwd", b"test", b"true",
];

pub(super) fn builtin_candidates(
  value: &Value,
) -> impl Iterator<Item = &'static [u8]> + '_ {
  SPECIAL
    .iter()
    .chain(INTRINSIC)
    .chain(REGULAR)
    .copied()
    .filter(|name| value.can_equal(name))
}

fn utility_resolution(name: &[u8]) -> Resolution {
  let regular = REGULAR.contains(&name);
  let mut result = alternative(
    if regular || special_builtin(name) || INTRINSIC.contains(&name) {
      Alternative::Builtin
    } else {
      Alternative::ExternalCommand
    },
  );
  if regular {
    result.alternatives.insert(Alternative::ExternalCommand);
  }
  result
}

fn command_names(value: &Value, env: &Environment) -> BTreeSet<Vec<u8>> {
  let mut names: BTreeSet<_> = SPECIAL
    .iter()
    .chain(INTRINSIC)
    .chain(REGULAR)
    .chain(UNSPECIFIED)
    .copied()
    .chain(env.functions.values.keys().map(Vec::as_slice))
    .filter(|name| value.can_equal(name))
    .map(<[u8]>::to_vec)
    .collect();
  names.extend(
    value
      .constraints()
      .into_iter()
      .filter(|choice| choice.exact)
      .map(|choice| choice.prefix),
  );
  names
}

pub(super) fn builtin_names(
  value: &Value,
  env: &Environment,
  suppress: bool,
) -> Vec<Vec<u8>> {
  if let Value::Literal(name) = value {
    return vec![name.clone()];
  }
  command_names(value, env)
    .into_iter()
    .filter(|name| {
      Resolver::command_resolution(&Value::Literal(name.clone()), env, suppress)
        .alternatives
        .contains(&Alternative::Builtin)
    })
    .collect()
}

fn query_output(name: &Value, resolution: &Resolution) -> Value {
  let issue = name.issue().unwrap_or(ValueIssue::Dynamic);
  let mut line = name.clone();
  line.concat(&Value::Literal(vec![b'\n']), usize::MAX);
  if let Value::Literal(bytes) = name
    && (special_builtin(bytes) || reserved_word(bytes))
  {
    return line;
  }
  let mut outputs = Vec::new();
  let unknown = unknown_utility(resolution);
  let bare_builtin = resolution.alternatives.contains(&Alternative::Builtin)
    && (!matches!(name, Value::Literal(_))
      || !resolution
        .alternatives
        .contains(&Alternative::ExternalCommand));
  if !resolution.candidates.is_empty() || unknown || bare_builtin {
    outputs.push(line.clone());
  }
  for reserved in RESERVED
    .iter()
    .chain(OPTIONAL_RESERVED)
    .filter(|reserved| name.can_equal(reserved))
  {
    let mut output = reserved.to_vec();
    output.push(b'\n');
    outputs.push(Value::Literal(output));
  }
  if unknown
    || resolution
      .alternatives
      .contains(&Alternative::ExternalCommand)
  {
    let mut path = Value::partial(b"/".to_vec(), b"\n".to_vec(), 2, issue);
    path.join(&Value::Literal(Vec::new()));
    outputs.push(path);
  }
  Value::join_all(outputs, issue)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Persistence {
  Temporary,
  Persistent,
  Unknown,
}

impl Persistence {
  fn save(
    self,
    redirects: &[Node<'_>],
    source: &[u8],
    env: &mut Environment,
  ) -> Option<Redirections> {
    (self != Self::Persistent)
      .then(|| Redirections::new(redirects, source, env, self == Self::Unknown))
  }
}

/// Redirections persist only for `exec`, reached directly or through the
/// `command` builtin; a name that may be either, or a `command` that may be a
/// function, makes them unknown.
fn redirection_persistence<'a>(
  mut name: &'a Value,
  mut args: &'a [Argument<'_>],
  env: &Environment,
) -> Persistence {
  let mut suppress = false;
  let mut builtin = true;
  while name == &Value::Literal(b"command".to_vec()) {
    let resolution = Resolver::command_resolution(name, env, suppress);
    if !resolution.alternatives.contains(&Alternative::Builtin) {
      return Persistence::Temporary;
    }
    builtin &= only_builtin(&resolution);
    match command_options(args) {
      Ok(options) if !options.query => {
        let Some((command, rest)) = args[options.operands..].split_first()
        else {
          return Persistence::Temporary;
        };
        name = &command.value;
        args = rest;
        suppress = true;
      }
      Err(ResolutionIssue::DynamicName) => break,
      Ok(_) | Err(_) => return Persistence::Temporary,
    }
  }
  if !name.can_equal(b"exec") && !name.can_equal(b"command") {
    Persistence::Temporary
  } else if builtin && *name == Value::Literal(b"exec".to_vec()) {
    Persistence::Persistent
  } else {
    Persistence::Unknown
  }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Search {
  Normal,
  /// The name may be a reserved word with unspecified results.
  Reserved,
  SuppressFunctions,
  Standard,
}

pub(super) fn only_builtin(resolution: &Resolution) -> bool {
  resolution.candidates.is_empty()
    && resolution.issues.is_empty()
    && resolution.alternatives == [Alternative::Builtin].into()
}

pub(super) fn unknown_utility(resolution: &Resolution) -> bool {
  !resolution.issues.is_empty()
    || resolution.alternatives.contains(&Alternative::External)
}

pub(super) fn special_name(name: &Value) -> bool {
  matches!(name, Value::Literal(name) if special_builtin(name))
}

pub(in crate::interface::application::engine) fn invariant_command(
  name: &Value,
) -> Option<Resolution> {
  special_name(name).then(|| alternative(Alternative::Builtin))
}
