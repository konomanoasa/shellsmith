use super::redirections::Redirections;
use std::collections::BTreeSet;
use tree_sitter::Node;

use super::super::execution::children;
use super::super::execution::{Environment, Flow, Transfer, alternative};
use super::super::expansion::Argument;
use super::super::{Frame, Resolver, Task};
use crate::interface::application::engine::word;
use crate::interface::application::engine::{
  Alternative, ArgumentKind, ExecutionKind, Resolution, ResolutionIssue,
  UseSite, Value, ValueIssue,
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
        let fields = words.filter(|arg| arg.boundary_known).count()
          + arguments
            .iter()
            .filter(|node| word::has_guaranteed_field(**node, source))
            .count();
        if fields > 1 {
          return ArgumentUse::Effects;
        }
      }
      return if super::arguments::uses_arguments(name)
        || env.io.output.is_some()
      {
        ArgumentUse::Values
      } else {
        ArgumentUse::Effects
      };
    }
    loop {
      let Some(argument) = words.as_slice().first() else {
        return ArgumentUse::Command;
      };
      match forward_option(argument) {
        ForwardOption::Name => break,
        ForwardOption::End => {
          words.next();
          break;
        }
        ForwardOption::Search => {
          words.next();
        }
        ForwardOption::Query => return ArgumentUse::Values,
        ForwardOption::Invalid | ForwardOption::Unknown => {
          return ArgumentUse::Effects;
        }
      }
    }
    suppress = true;
  }
}

pub(super) enum ForwardOption {
  Name,
  End,
  Search,
  Query,
  Invalid,
  Unknown,
}

pub(super) fn forward_option(argument: &Argument<'_>) -> ForwardOption {
  if !argument.boundary_known {
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
    let Some(command) = node
      .child_by_field_name("name")
      .or_else(|| node.child_by_field_name("word"))
    else {
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
      let source = self
        .workspace
        .document(call.frame.snapshot.document)
        .expect("current document")
        .source
        .as_bytes();
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
        return self.alternative_arguments(call, states, Flow::default());
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

  fn alternative_arguments(
    &mut self,
    call: CallArguments<'a>,
    mut states: super::super::execution::AlternativeStates,
    mut flow: Flow,
  ) -> Task<'a> {
    let Some(env) = states.next() else {
      return Task::Done(flow);
    };
    self
      .command_arguments(call.clone(), env)
      .then(move |resolver, next| {
        flow.join(next);
        resolver.alternative_arguments(call, states, flow)
      })
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
      && !matches!(&name, Value::Literal(name) if !special_builtin(name));
    let order = (env.order.clone(), env.io.input_order_unknown);
    let affected = if uncertain_order {
      self
        .expansion_targets(assignments.iter().chain(&redirects).copied(), frame)
    } else {
      std::collections::BTreeSet::new()
    };
    env.order.extend(affected.iter().cloned());
    if uncertain_order {
      self.gap(node, frame, ResolutionIssue::EvaluationOrder);
    }
    let source = self
      .workspace
      .document(frame.snapshot.document)
      .expect("document")
      .source
      .as_bytes();
    let io = temporary_redirections(&name, &args)
      .then(|| Redirections::new(&redirects, source, &mut env));
    env.io.input_order_unknown |= uncertain_order;
    Task::sequence(redirects, env, frame).then(
      move |resolver, mut redirected| {
        let before = redirected.0.remove(&Transfer::Next);
        flow.join(redirected);
        let Some(mut before) = before else {
          return Task::Done(flow);
        };
        let input_order = before.io.input_order_unknown && order.1;
        before.io.input_order_unknown |= uncertain_order;
        let frame = resolver.temporary_prefix(node, &name, &before, frame);
        let prepared = if assignments.len() > 1
          && !matches!(&name, Value::Literal(name) if special_builtin(name))
        {
          resolver.uncertain_order(node, assignments, before.clone(), frame)
        } else {
          Task::sequence(assignments, before.clone(), frame)
        };
        prepared.then(move |resolver, mut prepared| {
          let env = prepared.0.remove(&Transfer::Next);
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
              affected,
            );
          }
          let resolution = Self::command_resolution(&name, &env, false);
          if let Some(id) = resolver.site(command, frame) {
            resolver.record(id, frame.context, resolution.clone(), &env);
          }
          if let Some(prefix) = prefix
            && !matches!(&name, Value::Literal(name) if special_builtin(name))
          {
            resolver.prefix_order(node, prefix, &mut env, frame);
          }
          resolver
            .invoke(node, &name, &args, env, frame, Search::Normal)
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
        && let Some(env) = flow.0.get_mut(&Transfer::Next)
      {
        let assigned_status = env.substitution_status.take();
        let saved_input = env.io.save_input();
        let inner = resolver.execution(
          node,
          frame,
          ExecutionKind::Redirection,
          frame.snapshot,
          true,
        );
        let task = if uncertain_order {
          let mut input = env.clone();
          input
            .order
            .extend(resolver.expansion_targets(possible_writes, frame));
          resolver.uncertain_order(node, redirects, input, inner)
        } else {
          Task::sequence(redirects, env.clone(), inner)
        };
        return task.then(move |_, redirects| {
          if let Some(state) = flow.0.get_mut(&Transfer::Next) {
            let returned =
              redirects.0.into_values().reduce(|mut left, right| {
                left.join(&right);
                left
              });
            let mut status = assigned_status;
            if let Some(returned) = returned {
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
            state.special.insert(
              b"?".to_vec(),
              status.unwrap_or(Value::Unknown(ValueIssue::Dynamic)),
            );
            state.io.input_order_unknown = previous_input_order;
          }
          Task::Done(flow)
        });
      }
      if let Some(state) = flow.0.get_mut(&Transfer::Next) {
        state.io.input_order_unknown = previous_input_order;
        state.special.insert(
          b"?".to_vec(),
          state
            .substitution_status
            .take()
            .unwrap_or(Value::Literal(b"0".to_vec())),
        );
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
      targets,
    );
  }

  fn temporary_prefix(
    &mut self,
    node: Node<'_>,
    name: &Value,
    env: &Environment,
    frame: Frame,
  ) -> Frame {
    let assignments = assignments(node.child_by_field_name("prefix"));
    if assignments.is_empty()
      || matches!(name, Value::Literal(name) if special_builtin(name))
    {
      return frame;
    }
    let resolution = Self::command_resolution(name, env, false);
    if !resolution.candidates.is_empty()
      || !resolution.issues.is_empty()
      || resolution.alternatives.contains(&Alternative::External)
    {
      return frame;
    }
    let inner = self.execution(
      node,
      frame,
      ExecutionKind::TemporaryAssignments,
      frame.snapshot,
      false,
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
    if matches!(name, Value::Literal(name) if special_builtin(name)) {
      return;
    }
    let uncertain = !resolution.candidates.is_empty()
      || !resolution.issues.is_empty()
      || resolution.alternatives.contains(&Alternative::External)
      || SPECIAL.iter().any(|special| name.can_equal(special));
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
      for env in flow.0.values_mut() {
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
      let mut result = alternative(Alternative::External);
      result.issues.insert(ResolutionIssue::UnknownEffect);
      result
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

  fn invoke(
    &mut self,
    node: Node<'a>,
    name: &Value,
    args: &[Argument<'a>],
    env: Environment,
    frame: Frame,
    search: Search,
  ) -> Task<'a> {
    let resolution =
      Self::command_resolution(name, &env, search != Search::Normal);
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
        for state in flow.0.values_mut() {
          state.positional.clone_from(&env.positional);
          state.alternatives.restore_positionals(&env.alternatives);
        }
        let task = if resolution.alternatives.contains(&Alternative::Builtin) {
          if let Value::Literal(name) = &name {
            resolver.utility(node, name, args.clone(), env.clone(), frame)
          } else {
            let names = builtin_names(&name, &env, search != Search::Normal);
            resolver.invoke_builtins(node, names.into_iter(), &args, &env, Flow::default(), frame)
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
            external.special.insert(
              b"?".to_vec(),
              Value::Unknown(
                crate::interface::application::engine::ValueIssue::Dynamic,
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
            flow.join(resolver.opaque(node, env, frame, issue));
          }
          Task::Done(flow)
        })
      })
  }

  fn invoke_builtins(
    &mut self,
    node: Node<'a>,
    mut names: std::vec::IntoIter<Vec<u8>>,
    args: &[Argument<'a>],
    env: &Environment,
    mut flow: Flow,
    frame: Frame,
  ) -> Task<'a> {
    let Some(name) = names.next() else {
      return Task::Done(flow);
    };
    let env = env.clone();
    let args = args.to_vec();
    self
      .utility(node, &name, args.clone(), env.clone(), frame)
      .then(move |resolver, result| {
        flow.join(result);
        resolver.invoke_builtins(node, names, &args, &env, flow, frame)
      })
  }

  fn invoke_candidates(
    &mut self,
    node: Node<'a>,
    mut candidates: std::vec::IntoIter<
      crate::interface::application::engine::SymbolId,
    >,
    env: &Environment,
    mut flow: Flow,
    frame: Frame,
  ) -> Task<'a> {
    let next = candidates.find_map(|id| {
      self
        .index
        .functions
        .get(&id)
        .copied()
        .map(|body| (id, body))
    });
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
      env
        .special
        .insert(b"?".to_vec(), Value::Unknown(ValueIssue::Dynamic));
      self.gap(node, frame, ResolutionIssue::Recursion);
      self.invalidate(
        node,
        &mut env,
        frame,
        ResolutionIssue::Recursion,
        targets,
      );
      Task::Done(Flow::next(env))
    } else {
      let inner = self.execution(
        node,
        frame,
        ExecutionKind::Function(id),
        id.snapshot,
        false,
      );
      Task::eval(body, env.clone(), inner).then(|_, mut returned| {
        if let Some(state) = returned.0.remove(&Transfer::Return) {
          returned.add(Transfer::Next, state);
        }
        Task::Done(returned)
      })
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
    let mut standard_search = false;
    while let Some(argument) = args.as_slice().first() {
      match forward_option(argument) {
        ForwardOption::Name => break,
        ForwardOption::End => {
          self.argument_role(argument, None, frame);
          args.next();
          break;
        }
        ForwardOption::Search => {
          standard_search = true;
          self.argument_role(argument, None, frame);
          args.next();
        }
        ForwardOption::Query => {
          return self.command_query(node, args, env, frame);
        }
        option @ (ForwardOption::Invalid | ForwardOption::Unknown) => {
          for arg in args {
            self.argument_role(&arg, None, frame);
          }
          self.non_source(node, frame);
          return Task::Done(self.opaque(
            node,
            env,
            frame,
            if matches!(option, ForwardOption::Unknown) {
              ResolutionIssue::DynamicName
            } else {
              ResolutionIssue::UnknownEffect
            },
          ));
        }
      }
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
          flow.0.remove(&Transfer::Next).expect("forward input"),
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
      if standard_search {
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
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let mut valid = true;
    let mut verbose = false;
    let mut output = Value::Unknown(ValueIssue::UnsupportedExpansion);
    while let Some(arg) = args.as_slice().first() {
      match forward_option(arg) {
        ForwardOption::Name => break,
        ForwardOption::End => {
          self.argument_role(arg, None, frame);
          args.next();
          break;
        }
        ForwardOption::Search | ForwardOption::Query => {
          verbose |= matches!(&arg.value, Value::Literal(value) if value.contains(&b'V'));
          self.argument_role(arg, None, frame);
          args.next();
        }
        ForwardOption::Invalid | ForwardOption::Unknown => {
          valid = false;
          break;
        }
      }
    }
    if valid && args.len() == 1 {
      let arg = args.next().expect("query operand");
      if arg.boundary_known {
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
    env
      .special
      .insert(b"?".to_vec(), Value::Unknown(ValueIssue::Dynamic));
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

pub(super) const UNSPECIFIED: &[&[u8]] = &[
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

const BUILTINS: &[&[u8]] = &[
  b"command", b"alias", b"unalias", b"read", b"getopts", b"true", b"false",
  b"echo", b"printf", b"pwd", b"test", b"[", b"cd", b"umask", b"wait", b"jobs",
  b"fg", b"bg", b"hash", b"type", b"ulimit", b"kill", b"fc", b"newgrp",
];

fn builtin(name: &[u8]) -> bool {
  special_builtin(name) || BUILTINS.contains(&name)
}

pub(super) fn builtin_candidates(
  value: &Value,
) -> impl Iterator<Item = &'static [u8]> + '_ {
  SPECIAL
    .iter()
    .chain(BUILTINS)
    .copied()
    .filter(|name| value.can_equal(name))
}

fn utility_resolution(name: &[u8]) -> Resolution {
  let mut result = alternative(if builtin(name) {
    Alternative::Builtin
  } else {
    Alternative::ExternalCommand
  });
  if matches!(
    name,
    b"["
      | b"echo"
      | b"false"
      | b"newgrp"
      | b"printf"
      | b"pwd"
      | b"test"
      | b"true"
  ) {
    result.alternatives.insert(Alternative::ExternalCommand);
  }
  result
}

fn command_names(value: &Value, env: &Environment) -> BTreeSet<Vec<u8>> {
  let mut names: BTreeSet<_> = SPECIAL
    .iter()
    .chain(BUILTINS)
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
  let reserved: &[&[u8]] = &[
    b"!", b"{", b"}", b"case", b"do", b"done", b"elif", b"else", b"esac",
    b"fi", b"for", b"if", b"in", b"then", b"until", b"while",
  ];
  if let Value::Literal(bytes) = name
    && (special_builtin(bytes) || reserved.contains(&bytes.as_slice()))
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
  for reserved in reserved.iter().filter(|reserved| name.can_equal(reserved)) {
    let mut output = reserved.to_vec();
    output.push(b'\n');
    outputs.push(Value::Literal(output));
  }
  if unknown
    || resolution
      .alternatives
      .contains(&Alternative::ExternalCommand)
  {
    let mut path =
      Value::partial(b"/".to_vec(), b"\n".to_vec(), 2, ValueIssue::Dynamic);
    path.join(&Value::Literal(Vec::new()));
    outputs.push(path);
  }
  outputs
    .into_iter()
    .reduce(|mut left, right| {
      left.join(&right);
      left
    })
    .unwrap_or(Value::Unknown(ValueIssue::Dynamic))
}

fn temporary_redirections<'a>(
  mut name: &'a Value,
  args: &'a [Argument<'_>],
) -> bool {
  let mut args = args.iter();
  while name == &Value::Literal(b"command".to_vec()) {
    loop {
      let Some(arg) = args.next() else {
        return true;
      };
      match forward_option(arg) {
        ForwardOption::Search => {}
        ForwardOption::End => {
          let Some(arg) = args.next() else {
            return true;
          };
          name = &arg.value;
          break;
        }
        ForwardOption::Name => {
          name = &arg.value;
          break;
        }
        ForwardOption::Query | ForwardOption::Invalid => return true,
        ForwardOption::Unknown => return false,
      }
    }
  }
  !name.can_equal(b"exec") && !name.can_equal(b"command")
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Search {
  Normal,
  SuppressFunctions,
  Standard,
}

pub(in crate::interface::application::engine) fn invariant_command(
  name: &Value,
) -> Option<Resolution> {
  matches!(name, Value::Literal(name) if special_builtin(name))
    .then(|| alternative(Alternative::Builtin))
}
