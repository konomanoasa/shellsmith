use super::redirections::Redirections;
use std::collections::BTreeSet;
use tree_sitter::Node;

use super::super::execution::children;
use super::super::execution::{Environment, Flow, Transfer, alternative};
use super::super::expansion::Argument;
use super::super::{Frame, Resolver, Task};
use crate::interface::application::engine::word;
use crate::interface::application::engine::workspace::parser;
use crate::interface::application::engine::{
  Alternative, ArgumentKind, ExecutionKind, Resolution, ResolutionIssue,
  SymbolId, UseSite, Value, ValueIssue,
};

#[derive(Clone, Copy)]
enum ArgumentUse {
  Effects,
  Values,
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
      return ArgumentUse::Values;
    }
    if !resolution.alternatives.contains(&Alternative::Builtin) {
      return ArgumentUse::Effects;
    }
    let Value::Literal(name) = &command.value else {
      return ArgumentUse::Effects;
    };
    if name != b"command" {
      if super::arguments::numeric_argument(name) {
        let fields = words.filter(|arg| arg.boundary.is_ok()).count()
          + arguments
            .iter()
            .filter(|node| word::has_guaranteed_field(**node, source))
            .count();
        if fields > 1 {
          return ArgumentUse::Effects;
        }
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
  if argument.boundary.is_err() {
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

/// Parses every option of `command`, or returns why they are unknown.
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
    let redirects = redirects(prefix, &suffix);
    let Some(command) = parser::command_word(node) else {
      return Self::without_command(node, prefix, redirects, env, frame);
    };
    let arguments = suffix
      .into_iter()
      .filter(|word| word.kind() == "word")
      .collect();
    self.arguments(
      vec![command].into_iter(),
      Vec::new(),
      env,
      frame,
      Box::new(move |resolver, words, env| {
        resolver.command_arguments(
          CallArguments {
            node,
            command,
            arguments,
            words,
            frame,
          },
          env,
        )
      }),
    )
  }

  fn command_arguments(
    &mut self,
    call: CallArguments<'a>,
    env: Environment,
  ) -> Task<'a> {
    if !env.alternatives.is_empty() {
      let source = self.workspace.source(call.frame.snapshot.document);
      let usage = argument_use(&call.words, &call.arguments, source, &env);
      let needed = call.arguments.iter().copied().enumerate().filter_map(
        |(index, argument)| {
          (matches!(usage, ArgumentUse::Values)
            || (matches!(usage, ArgumentUse::Command) && index == 0)
            || (!matches!(usage, ArgumentUse::Command)
              && !self.expansion_targets([argument], call.frame).is_empty()))
          .then_some(argument)
        },
      );
      let names = self.value_dependencies(needed, call.frame);
      if let Some(states) = env.alternatives.select(&env, &names) {
        return self.each_alternative(
          states,
          Flow::default(),
          move |resolver, env| resolver.command_arguments(call.clone(), env),
        );
      }
      if matches!(usage, ArgumentUse::Command) && !call.arguments.is_empty() {
        let CallArguments {
          node,
          command,
          mut arguments,
          words,
          frame,
        } = call;
        let first = arguments.remove(0);
        return self.arguments(
          vec![first].into_iter(),
          words,
          env,
          frame,
          Box::new(move |resolver, words, env| {
            resolver.command_arguments(
              CallArguments {
                node,
                command,
                arguments,
                words,
                frame,
              },
              env,
            )
          }),
        );
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
    let suffix = node
      .child_by_field_name("suffix")
      .map_or_else(Vec::new, children);
    let redirects = redirects(prefix, &suffix);
    if words.is_empty() {
      return Self::without_command(node, prefix, redirects, env, frame);
    }
    let name = words.remove(0).value;
    let args = words;
    let mut flow = Flow::default();
    let assignments = assignments(prefix);
    let uncertain_order = !assignments.is_empty()
      && !redirects.is_empty()
      && (special_name(&name) || !matches!(name, Value::Literal(_)));
    let order = (env.order.clone(), env.io.input_order_unknown);
    let affected = if uncertain_order {
      self.gap(node, frame, ResolutionIssue::EvaluationOrder);
      self
        .expansion_targets(assignments.iter().chain(&redirects).copied(), frame)
    } else {
      BTreeSet::new()
    };
    env.order.extend(affected.iter().cloned());
    let source = self.workspace.source(frame.snapshot.document);
    let search = Self::name_search(command, source, &name, &env);
    let io = temporary_redirections(&name, &args)
      .then(|| Redirections::new(&redirects, source, &mut env));
    env.io.input_order_unknown |= uncertain_order;
    Task::sequence(redirects, env, frame).then(
      move |resolver, mut redirected| {
        let before = redirected.take(Transfer::Next);
        flow.join(redirected);
        let Some(mut before) = before else {
          return Task::Done(flow);
        };
        let input_order = before.io.input_order_unknown && order.1;
        before.io.input_order_unknown |= uncertain_order;
        let frame =
          resolver.temporary_prefix(node, &name, &before, frame, search);
        resolver
          .prefix_assignments(node, assignments, &name, before.clone(), frame)
          .then(move |resolver, mut prepared| {
            let env = prepared.take(Transfer::Next);
            flow.join(prepared);
            let Some(mut env) = env else {
              return Task::Done(flow);
            };
            env.order = order.0;
            env.io.input_order_unknown = input_order;
            if uncertain_order {
              resolver.invalidate(
                node,
                &mut env,
                frame,
                ResolutionIssue::EvaluationOrder,
                ValueIssue::Dynamic,
                affected,
              );
            }
            let resolution = Self::search_resolution(&name, &env, search);
            if let Some(id) = resolver.site(command, frame) {
              resolver.record_use(
                UseSite::Syntax(id),
                frame.context,
                resolution.clone(),
                None,
                &env,
              );
            }
            if let Some(prefix) = prefix
              && !special_name(&name)
            {
              resolver.prefix_order(node, prefix, &mut env, frame);
            }
            resolver
              .invoke(node, &name, &args, env, frame, search)
              .then(move |resolver, mut called| {
                if let Some(prefix) = prefix {
                  resolver.restore_prefix(
                    prefix,
                    &before,
                    &name,
                    &resolution,
                    &mut called,
                    frame,
                  );
                }
                if let Some(io) = io {
                  io.restore(&mut called);
                }
                flow.join(called);
                Task::Done(flow)
              })
          })
      },
    )
  }

  fn prefix_assignments(
    &mut self,
    node: Node<'a>,
    assignments: Vec<Node<'a>>,
    name: &Value,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    if assignments.len() > 1 && !special_name(name) {
      let targets = self.expansion_targets(assignments.iter().copied(), frame);
      self.uncertain_order(node, assignments, targets, env, frame)
    } else {
      Task::sequence(assignments, env, frame)
    }
  }

  fn without_command(
    node: Node<'a>,
    prefix: Option<Node<'a>>,
    redirects: Vec<Node<'a>>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let assignments = assignments(prefix);
    let possible_writes = assignments.clone();
    let uncertain_order = !assignments.is_empty();
    let previous_input_order = env.io.input_order_unknown;
    env.io.input_order_unknown |= uncertain_order && !redirects.is_empty();
    Task::sequence(assignments, env, frame).then(move |resolver, mut flow| {
      if !redirects.is_empty()
        && let Some(env) = flow.get_mut(Transfer::Next)
      {
        let assigned_status = env.substitution_status.take();
        let saved_input = env.io.save_input();
        let inner = resolver.execution(
          node,
          frame,
          ExecutionKind::Redirection,
          frame.snapshot,
        );
        let task = if uncertain_order {
          let targets = resolver.expansion_targets(
            possible_writes.into_iter().chain(redirects.iter().copied()),
            frame,
          );
          resolver.uncertain_order(node, redirects, targets, env.clone(), inner)
        } else {
          Task::sequence(redirects, env.clone(), inner)
        };
        return task.then(move |_, redirects| {
          if let Some(state) = flow.get_mut(Transfer::Next) {
            let returned = redirects.merged();
            let mut status = assigned_status;
            if let Some(returned) = returned {
              state.failure.then(returned.failure);
              if let Some(redirected_status) = returned.substitution_status {
                if let Some(status) = &mut status {
                  status.join(&redirected_status);
                } else {
                  status = Some(redirected_status);
                }
              }
              state.io.inherit_input(&returned.io, saved_input);
              state.io.output = returned.io.output;
            } else {
              state.io.restore_input(saved_input);
            }
            state.set_status(
              status.unwrap_or(Value::Unknown(ValueIssue::Dynamic)),
            );
            state.io.input_order_unknown = previous_input_order;
          }
          Task::Done(flow)
        });
      }
      if let Some(state) = flow.get_mut(Transfer::Next) {
        state.io.input_order_unknown = previous_input_order;
        let status = state
          .substitution_status
          .take()
          .unwrap_or(Value::Literal(b"0".to_vec()));
        state.set_status(status);
      }
      Task::Done(flow)
    })
  }

  fn prefix_order(
    &mut self,
    node: Node<'_>,
    prefix: Node<'a>,
    env: &mut Environment,
    frame: Frame,
  ) {
    let targets = self.expansion_targets(
      children(prefix)
        .into_iter()
        .filter_map(|child| child.child_by_field_name("value")),
      frame,
    );
    self.invalidate(
      node,
      env,
      frame,
      ResolutionIssue::EvaluationOrder,
      ValueIssue::Dynamic,
      targets,
    );
  }

  fn temporary_prefix(
    &mut self,
    node: Node<'_>,
    name: &Value,
    env: &Environment,
    frame: Frame,
    search: Search,
  ) -> Frame {
    let assignments = assignments(node.child_by_field_name("prefix"));
    if assignments.is_empty()
      || special_name(name)
      || prefix_may_persist(name, &Self::search_resolution(name, env, search))
    {
      return frame;
    }
    let inner = self.execution(
      node,
      frame,
      ExecutionKind::TemporaryAssignments,
      frame.snapshot,
    );
    for assignment in assignments {
      if let Some(id) = assignment
        .child_by_field_name("name")
        .and_then(|name| self.site(name, frame))
        && let Some(site) = self.workspace.occurrence(id)
        && let Value::Literal(name) = &site.name
      {
        self
          .result
          .temporary
          .entry(inner.context)
          .or_default()
          .insert(name.clone(), UseSite::Syntax(id));
      }
    }
    inner
  }

  fn restore_prefix(
    &self,
    prefix: Node<'_>,
    before: &Environment,
    name: &Value,
    resolution: &Resolution,
    flow: &mut Flow,
    frame: Frame,
  ) {
    if special_name(name) {
      return;
    }
    let uncertain = prefix_may_persist(name, resolution);
    for assignment in children(prefix) {
      let Some(id) = assignment
        .child_by_field_name("name")
        .and_then(|node| self.site(node, frame))
      else {
        continue;
      };
      let Some(site) = self.workspace.occurrence(id) else {
        continue;
      };
      let Value::Literal(name) = &site.name else {
        continue;
      };
      if before.readonly(name) == Some(true) {
        continue;
      }
      for env in flow.environments_mut() {
        env.variable_changed(name, None);
        env.alternatives.forget(name);
        let old = before.variables.get(name);
        if uncertain {
          let mut current = env.variables.get(name).clone();
          current.join(old);
          current
            .resolution
            .issues
            .insert(ResolutionIssue::EvaluationOrder);
          env.variables.values.insert(name.clone(), current);
        } else {
          env.variables.values.insert(name.clone(), old.clone());
          env.alternatives.restore(name, &before.alternatives);
        }
      }
    }
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
    } else if unspecified(name) {
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

  /// Search for a simple command name, which POSIX leaves unspecified where
  /// an implementation may recognize the name as a reserved word.
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
      Self::command_resolution(name, env, false)
        != alternative(Alternative::ExternalCommand)
    } else {
      optional_reserved_word(&word)
        || (colon_word(&word)
          && !word.iter().any(|byte| matches!(byte, b'\\' | b'\'' | b'"')))
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
    if !resolution.candidates.is_empty()
      || !resolution.issues.is_empty()
      || resolution.alternatives != [Alternative::Builtin].into()
    {
      for arg in args {
        self.argument_role(arg, None, frame);
      }
    }
    if !resolution.candidates.is_empty()
      || !resolution.issues.is_empty()
      || resolution
        .alternatives
        .iter()
        .any(|alternative| *alternative != Alternative::Builtin)
    {
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
    call.alternatives.forget_positionals();
    call.positional = super::super::expansion::positional_arguments(&args);
    self
      .invoke_candidates(node, candidates, &call, Flow::default(), frame)
      .then(move |resolver, mut flow| {
        for state in flow.environments_mut() {
          state.positional.clone_from(&env.positional);
          state.alternatives.restore_positionals(&env.alternatives);
        }
        let task = if resolution.alternatives.contains(&Alternative::Builtin) {
          if let Value::Literal(name) = &name {
            resolver.utility(node, name, args.clone(), env.clone(), frame)
          } else {
            let names = builtin_names(&name, &env, search != Search::Normal);
            resolver.invoke_builtins(node, names.into_iter(), args.clone(), env.clone(), Flow::default(), frame)
          }
        } else { Task::Done(Flow::default()) };
        task.then(move |resolver, builtin| {
          flow.join(builtin);
          if resolution
            .alternatives
            .contains(&Alternative::ExternalCommand)
          {
            let mut external = env.clone();
            let output = if search == Search::Standard {
              if let Value::Literal(name) = &name {
                super::builtins::output(
                  name,
                  &args,
                  &external,
                  resolver.result.request.limits.value_bytes,
                )
              } else {
                Value::Unknown(ValueIssue::Dynamic)
              }
            } else {
              Value::Unknown(ValueIssue::UnsupportedExpansion)
            };
            external.emit(output, resolver.result.request.limits.value_bytes);
            if search != Search::Standard || !matches!(&name, Value::Literal(name) if super::builtins::preserves_input(name)) {
              external.io.consume_input(None);
            }
            if external.io.stderr_captured && let Some(output) = &mut external.io.output { output.fail(ValueIssue::Dynamic); }
            external.set_status(Value::Unknown(
                ValueIssue::Dynamic,
              ),
            );
            flow.add(Transfer::Next, external);
          }
          if !resolution.issues.is_empty()
            || resolution.alternatives.contains(&Alternative::External)
          {
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
      if arg.boundary.is_ok() {
        let resolution = Self::command_resolution(&arg.value, &env, false);
        if !verbose {
          output = query_output(&arg.value, &resolution);
        }
        self.record_argument_use(
          &arg,
          Self::command_resolution(&arg.value, &env, false),
          &env,
          frame,
        );
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
    env.emit(output, self.result.request.limits.value_bytes);
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

fn redirects<'a>(
  prefix: Option<Node<'a>>,
  suffix: &[Node<'a>],
) -> Vec<Node<'a>> {
  prefix
    .into_iter()
    .flat_map(children)
    .chain(suffix.iter().copied())
    .filter(|node| node.kind() != "word" && node.kind() != "assignment_word")
    .collect()
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

fn unspecified(name: &[u8]) -> bool {
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

/// Words that an implementation may recognize as reserved words.
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

/// Whether POSIX reserves `word` for ending in a colon; the `:` utility is
/// specified separately.
fn colon_word(word: &[u8]) -> bool {
  word.len() > 1 && word.ends_with(b":")
}

/// Whether the prefix assignments of a command named `name`, which is not a
/// definite special built-in, may persist after it.
fn prefix_may_persist(name: &Value, resolution: &Resolution) -> bool {
  !resolution.candidates.is_empty()
    || !resolution.issues.is_empty()
    || resolution.alternatives.contains(&Alternative::External)
    || SPECIAL.iter().any(|special| name.can_equal(special))
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

/// Modeled regular built-ins, for which a PATH search may find an external
/// implementation.
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
  if let Value::Literal(bytes) = name
    && (special_builtin(bytes) || reserved_word(bytes))
  {
    let mut output = name.clone();
    output.concat(&Value::Literal(vec![b'\n']), usize::MAX);
    return output;
  }
  let mut outputs = Vec::new();
  let unknown = !resolution.issues.is_empty()
    || resolution.alternatives.contains(&Alternative::External);
  let bare_builtin = resolution.alternatives.contains(&Alternative::Builtin)
    && (!matches!(name, Value::Literal(_))
      || !resolution
        .alternatives
        .contains(&Alternative::ExternalCommand));
  if !resolution.candidates.is_empty() || unknown || bare_builtin {
    let mut output = name.clone();
    output.concat(&Value::Literal(vec![b'\n']), usize::MAX);
    outputs.push(output);
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
  if name.intersects(&Value::partial(
    Vec::new(),
    b":".to_vec(),
    2,
    ValueIssue::Dynamic,
  )) && !matches!(name, Value::Literal(bytes) if !colon_word(bytes))
  {
    let mut output = name.clone();
    output.concat(&Value::Literal(vec![b'\n']), usize::MAX);
    outputs.push(output);
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
  outputs
    .into_iter()
    .reduce(|mut left, right| {
      left.join(&right);
      left
    })
    .unwrap_or(Value::Unknown(issue))
}

fn temporary_redirections<'a>(
  mut name: &'a Value,
  mut args: &'a [Argument<'_>],
) -> bool {
  while name == &Value::Literal(b"command".to_vec()) {
    match command_options(args) {
      Ok(options) if !options.query => {
        let Some((command, rest)) = args[options.operands..].split_first()
        else {
          return true;
        };
        name = &command.value;
        args = rest;
      }
      Err(ResolutionIssue::DynamicName) => return false,
      Ok(_) | Err(_) => return true,
    }
  }
  !name.can_equal(b"exec") && !name.can_equal(b"command")
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Search {
  Normal,
  /// The name may be a reserved word with unspecified results.
  Reserved,
  SuppressFunctions,
  Standard,
}

/// Whether `name` is definitely the name of a special built-in.
pub(super) fn special_name(name: &Value) -> bool {
  matches!(name, Value::Literal(name) if special_builtin(name))
}

pub(in crate::interface::application::engine) fn invariant_command(
  name: &Value,
) -> Option<Resolution> {
  special_name(name).then(|| alternative(Alternative::Builtin))
}
