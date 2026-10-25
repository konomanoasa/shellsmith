use std::collections::BTreeSet;
use std::sync::Arc;
use tree_sitter::Node;

use super::super::command::Redirections;
use super::super::expansion::Expansion;
use super::super::{Frame, Resolver};
use super::alternatives::{Alternatives, StateKey};
use super::environment::{
  Binding, CommandFailure, Environment, Flow, Output, Transfer, alternative,
  definition, merge_value,
};
use crate::interface::application::engine::word::{Word, WordContext};
use crate::interface::application::engine::workspace::parser;
use crate::interface::application::engine::{
  Access, Alternative, EffectTarget, ExecutionContextId, ExecutionKind,
  OccurrenceId, OccurrenceKind, ResolutionIssue, UseSite, Value, ValueIssue,
};
use crate::interface::application::engine::{Error, check_cancelled};

impl<'a> Resolver<'a> {
  pub(in crate::interface::application::engine::analysis) fn eval(
    &mut self,
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    if parser::damaged(node) {
      return Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::Recovery,
        None,
      ));
    }
    if !env.alternatives.is_empty() {
      let target = match node.kind() {
        "simple_command" => parser::command_word(node),
        "assignment_word" => Some(node),
        "word" if node.child_by_field_name("name").is_some() => Some(node),
        "for_clause" => node.child_by_field_name("words"),
        "io_redirect" if !self.expansion_targets([node], frame).is_empty() => {
          Some(node)
        }
        _ => None,
      };
      let mut names = self.value_dependencies(target, frame);
      if node.kind() == "for_clause"
        && !node
          .children(&mut node.walk())
          .any(|child| child.kind() == "in")
      {
        names.insert(StateKey::Positionals);
      }
      if env.alternatives.choice(&names).is_some() {
        if node.kind() == "assignment_word"
          || node.kind() == "word" && node.child_by_field_name("name").is_some()
        {
          match self.conditional_assignment(node, &env, frame) {
            Ok(Some(output)) => return Task::Done(Flow::next(output)),
            Ok(None) => {}
            Err(error) => {
              self.error = Some(error);
              return Task::Done(Flow::default());
            }
          }
        }
        return self.each_alternative(env, names, move |_, env| {
          Task::eval(node, env, frame)
        });
      }
    }
    self.eval_syntax(node, env, frame)
  }

  pub(in crate::interface::application::engine::analysis) fn value_dependencies(
    &self,
    nodes: impl IntoIterator<Item = Node<'a>>,
    frame: Frame,
  ) -> BTreeSet<StateKey> {
    let mut names = BTreeSet::new();
    for child in self.expansion_nodes(nodes, frame) {
      if let Some(site) = self
        .site(child, frame)
        .and_then(|id| self.workspace.occurrence(id))
        && !matches!(site.access, Access::Write | Access::Definition)
        && let Value::Literal(name) = &site.name
      {
        names.insert(match site.kind {
          OccurrenceKind::Command => StateKey::Function(name.clone()),
          OccurrenceKind::PositionalParameter => StateKey::Positionals,
          OccurrenceKind::SpecialParameter
            if matches!(name.as_slice(), b"@" | b"*" | b"#") =>
          {
            StateKey::Positionals
          }
          _ => StateKey::Variable(name.clone()),
        });
      }
      if child.kind() == "tilde_expansion" {
        names.insert(StateKey::Variable(b"HOME".to_vec()));
      }
    }
    names
  }

  /// Nodes evaluated in the current environment when `nodes` are expanded,
  /// with here-document bodies and without subshells.
  pub(in crate::interface::application::engine::analysis) fn expansion_nodes(
    &self,
    nodes: impl IntoIterator<Item = Node<'a>>,
    frame: Frame,
  ) -> impl Iterator<Item = Node<'a>> {
    let mut pending: Vec<_> = nodes.into_iter().collect();
    std::iter::from_fn(move || {
      let node = loop {
        let node = pending.pop()?;
        if !parser::subshell(node.kind()) {
          break node;
        }
      };
      if node.kind() == "io_here" {
        pending.extend(self.index.heredoc(frame.snapshot, node));
      }
      pending.extend(children(node));
      Some(node)
    })
  }

  /// Runs `step` on each side of the choices that `names` depend on and joins
  /// the results on the same choices.
  pub(in crate::interface::application::engine::analysis) fn each_alternative(
    &mut self,
    env: Environment,
    names: BTreeSet<StateKey>,
    step: impl Fn(&mut Self, Environment) -> Task<'a> + Clone + 'a,
  ) -> Task<'a> {
    let Some(choice) = env.alternatives.choice(&names) else {
      return step(self, env);
    };
    let (left, split) = Alternatives::split(env, choice);
    let (right_names, right_step) = (names.clone(), step.clone());
    self
      .each_alternative(left, names, step)
      .then(move |resolver, flow| {
        let (right, join) = split.resume(flow);
        resolver
          .each_alternative(right, right_names, right_step)
          .then(move |_, right| Task::Done(join(right)))
      })
  }

  fn eval_syntax(
    &mut self,
    node: Node<'a>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    if node.children(&mut node.walk()).any(parser::damaged) {
      self.gap(node, frame, ResolutionIssue::Recovery);
    }
    match node.kind() {
      "comment"
      | "here_end"
      | "tilde_expansion"
      | "quoted_here_document_body"
      | "here_document"
      | "here_document_end" => Task::Done(Flow::next(env)),
      "simple_command" => self.command(node, env, frame),
      "pipeline" if node.child_by_field_name("negation").is_some() => {
        Self::negated_pipeline(node, env, frame)
      }
      "io_here" => self.heredoc(node, env, frame),
      "io_file" => {
        let env = self.recover(
          &damage_outside(node, &["operator", "filename"]),
          env,
          frame,
        );
        self.redirect(node, env, frame)
      }
      "command" | "function_body" => self.compound(node, env, frame),
      "complete_command" | "compound_list" => {
        let [body] = regions(node, ["body"]);
        Task::sequence(body, env, frame)
      }
      "list" | "term" => self.list(node, env, frame),
      "and_or" => Self::short_circuit(node, env, frame),
      "pipe_sequence" => self.pipeline(node, env, frame),
      "assignment_word" => Self::assignment(node, env, frame),
      "word" if node.child_by_field_name("name").is_some() => {
        Self::assignment(node, env, frame)
      }
      "parameter_expansion"
      | "word"
      | "cmd_name"
      | "cmd_word"
      | "assignment_value" => Task::expand(node, false, env, frame)
        .then_word(|_, result| Task::Done(Flow::next(result.env))),
      "arithmetic_expansion"
      | "arithmetic_assignment_expression"
      | "arithmetic_binary_expression"
      | "arithmetic_conditional_expression" => {
        Task::arithmetic(node, env, frame)
          .then_word(|_, result| Task::Done(Flow::next(result.env)))
      }
      "if_clause" | "else_part" => Self::conditional(node, env, frame),
      "arithmetic_dynamic_expression" => {
        self.runtime_arithmetic(node, env, frame)
      }
      "function_definition" => {
        if let Some(name) = node.child_by_field_name("name") {
          self.write(
            name,
            &mut env,
            frame,
            true,
            Value::Unknown(ValueIssue::Dynamic),
          );
        }
        env.set_status(Value::Literal(b"0".to_vec()));
        let env =
          self.recover(&damage_outside(node, &["name", "body"]), env, frame);
        Task::Done(Flow::next(env))
      }
      kind if parser::subshell(kind) => self.isolated(node, env, frame),
      "for_clause" | "while_clause" | "until_clause" => {
        self.loop_clause(node, env, frame)
      }
      "case_clause" => Self::case_clause(node, env, frame),
      _ => {
        if let Some(id) = self.site(node, frame) {
          self.read(id, &env, frame);
        }
        Task::sequence(children(node), env, frame)
      }
    }
  }

  fn negated_pipeline(
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    Task::sequence(children(node), env, frame).then(|_, mut flow| {
      if let Some(env) = flow.get_mut(Transfer::Next) {
        let value = match env.status() {
          Value::Literal(bytes) => {
            Value::Literal(if bytes == b"0" { b"1" } else { b"0" }.to_vec())
          }
          unknown @ (Value::Unknown(_) | Value::Partial(_)) => {
            Value::Unknown(unknown.issue().expect("unknown value"))
          }
        };
        env.set_status(value);
      }
      Task::Done(flow)
    })
  }

  fn compound(
    &mut self,
    node: Node<'a>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let enclosing = std::mem::take(&mut env.failure);
    self
      .compound_body(node, env, frame)
      .then(move |_, mut flow| {
        for state in flow.environments_mut() {
          state.complete_command();
          state.failure = enclosing;
        }
        Task::Done(flow)
      })
  }

  fn compound_body(
    &mut self,
    node: Node<'a>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let body = node.child_by_field_name("body");
    let redirects = node.child_by_field_name("redirects");
    let damaged = self.index.damaged(frame.snapshot, node);
    let subshell = body
      .filter(|body| body.kind() == "compound_command")
      .and_then(|body| body.named_child(0))
      .filter(|body| body.kind() == "subshell");
    if let Some(subshell) = subshell
      && (redirects.is_some() || damaged)
    {
      let nodes = redirects.into_iter().chain(children(subshell)).collect();
      return self.isolated_sequence(
        if damaged { node } else { subshell },
        nodes,
        env,
        frame,
        ExecutionKind::Subshell,
      );
    }
    if damaged {
      return Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::Recovery,
        None,
      ));
    }
    let source = self.workspace.source(frame.snapshot.document);
    let io = Redirections::new(
      &redirects.into_iter().collect::<Vec<_>>(),
      source,
      &mut env,
      false,
    );
    Task::sequence(redirects.into_iter().chain(body).collect(), env, frame)
      .then(move |_, mut flow| {
        io.restore(&mut flow);
        Task::Done(flow)
      })
  }

  fn isolated(
    &mut self,
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let kind = if node.kind() == "subshell" {
      ExecutionKind::Subshell
    } else {
      ExecutionKind::Substitution
    };
    self.isolated_sequence(node, children(node), env, frame, kind)
  }

  fn isolated_sequence(
    &mut self,
    node: Node<'a>,
    nodes: Vec<Node<'a>>,
    mut env: Environment,
    frame: Frame,
    kind: ExecutionKind,
  ) -> Task<'a> {
    let substitution = kind == ExecutionKind::Substitution;
    let inner = self.execution(node, frame, kind, frame.snapshot);
    let input = env.io.save_input();
    let mut input_env = env.clone();
    input_env.failure = CommandFailure::None;
    let task = self.recovered_sequence(node, nodes, input_env, inner);
    task.then(move |_, mut flow| {
      for state in flow.environments_mut() {
        state.complete_command();
      }
      let status = Value::join_all(
        flow.environments().map(Environment::status),
        ValueIssue::Dynamic,
      );
      let mut env = env;
      if substitution {
        env.substitution_status = Some(status);
      } else {
        env.set_status(status);
      }
      let returned = flow.merged();
      if let Some(returned) = returned {
        env.io.inherit_input(&returned.io, input);
        if !substitution {
          env.io.output = returned.io.output;
        }
      } else {
        env.io.restore_input(input);
      }
      Task::Done(Flow::next(env))
    })
  }

  pub(in crate::interface::application::engine::analysis) fn substitute(
    &mut self,
    node: Node<'a>,
    quoted: bool,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let body = substitution_body(node);
    let inner =
      self.execution(node, frame, ExecutionKind::Substitution, frame.snapshot);
    let saved_input = env.io.save_input();
    let mut input = env.clone();
    input.failure = CommandFailure::None;
    input.io.output = Some(Output::default());
    input.io.stdout_redirected = false;
    input.io.stderr_captured = false;
    if env.io.stderr_captured
      && let Some(output) = &mut env.io.output
    {
      output.fail(ValueIssue::Dynamic);
    }
    let task = self.recovered_sequence(node, children(node), input, inner);
    task.then(move |resolver, flow| {
      let result = flow.merged();
      let mut trace = None;
      let value = if let Some(result) = result {
        env.substitution_status = Some(match body {
          SubstitutionBody::Empty => Value::Literal(b"0".to_vec()),
          SubstitutionBody::Redirections => {
            Value::Unknown(ValueIssue::Unspecified)
          }
          SubstitutionBody::Commands => result.status(),
        });
        env.io.inherit_input(&result.io, saved_input);
        trace = result.io.output.as_ref().and_then(Output::trace);
        let mut value =
          result.io.output.map_or(
            Value::Unknown(ValueIssue::UnsupportedExpansion),
            |output| output.value(),
          );
        value.trim_newlines();
        if matches!(body, SubstitutionBody::Redirections) {
          Value::Unknown(ValueIssue::Unspecified)
        } else {
          value
        }
      } else {
        env.io.restore_input(saved_input);
        Value::Unknown(ValueIssue::UnsupportedExpansion)
      };
      let trace = value.literal().ok().and_then(|bytes| {
        trace
          .map(|trace| trace.slice(0..bytes.len()))
          .and_then(|trace| trace.matching(bytes))
      });
      resolver.substitution_trace(&frame.location(node), frame.context, trace);
      Task::expanded(Expansion {
        word: Word::value(value, quoted),
        env,
      })
    })
  }

  fn recovered_sequence(
    &mut self,
    node: Node<'a>,
    nodes: Vec<Node<'a>>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    if self.index.damaged(frame.snapshot, node) {
      Task::Done(self.opaque(node, env, frame, ResolutionIssue::Recovery, None))
    } else {
      Task::sequence(nodes, env, frame)
    }
  }

  /// Any node may observe the others' writes to `targets`.
  pub(in crate::interface::application::engine::analysis) fn uncertain_order(
    &mut self,
    node: Node<'a>,
    nodes: Vec<Node<'a>>,
    targets: BTreeSet<EffectTarget>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    self.gap(node, frame, ResolutionIssue::EvaluationOrder);
    let previous = env.order.clone();
    let previous_input_order = env.io.input_order_unknown;
    env.io.input_order_unknown = true;
    env.order.extend(targets);
    Task::sequence(nodes, env, frame).then(move |_, mut flow| {
      for state in flow.environments_mut() {
        state.order.clone_from(&previous);
        state.io.input_order_unknown = previous_input_order;
      }
      Task::Done(flow)
    })
  }

  pub(in crate::interface::application::engine::analysis) fn read(
    &mut self,
    id: OccurrenceId,
    env: &Environment,
    frame: Frame,
  ) {
    let Some(site) = self.workspace.occurrence(id) else {
      return;
    };
    if matches!(site.access, Access::Write | Access::Definition)
      || site.kind == OccurrenceKind::Command
    {
      return;
    }
    let special = matches!(
      site.kind,
      OccurrenceKind::PositionalParameter | OccurrenceKind::SpecialParameter
    );
    let mut result = if special {
      alternative(Alternative::SpecialParameter)
    } else if let Value::Literal(name) = &site.name {
      env.variables.get(name).resolution.clone()
    } else {
      let mut result = alternative(Alternative::External);
      result.issues.insert(ResolutionIssue::UndecodedSource);
      result
    };
    if site.access == Access::Unknown {
      result.issues.insert(ResolutionIssue::UnknownEffect);
    }
    if !special {
      let value = self
        .variable_value(id, env)
        .cloned()
        .unwrap_or_else(|| Arc::new(Value::Unknown(ValueIssue::Dynamic)));
      self
        .result
        .values
        .entry((id, frame.context))
        .and_modify(|old| merge_value(old, &value))
        .or_insert(value);
    }
    self.record_attribute(id, frame.context, env);
    let variable = match &site.name {
      Value::Literal(name) if !special => Some(name.as_slice()),
      _ => None,
    };
    self.record_use(UseSite::Syntax(id), frame.context, result, variable, env);
  }

  pub(in crate::interface::application::engine::analysis) fn write(
    &mut self,
    node: Node<'_>,
    env: &mut Environment,
    frame: Frame,
    function: bool,
    value: Value,
  ) -> bool {
    if !function
      && let Some(id) = self.site(node, frame)
      && self
        .workspace
        .occurrence(id)
        .is_some_and(|site| site.kind != OccurrenceKind::DeclarationAssignment)
    {
      self.record_attribute(id, frame.context, env);
    }
    let symbol = self
      .site(node, frame)
      .and_then(|id| self.index.defined_symbol(id))
      .and_then(|id| self.workspace.symbol(id));
    if let Some(symbol) = symbol {
      self.result.writes.insert((symbol.id, frame.context));
      if function {
        env.alternatives.forget_function(&symbol.name);
        env.functions.values.insert(
          symbol.name.clone(),
          Binding {
            resolution: definition(symbol.id),
            value: Arc::new(value),
          },
        );
        return true;
      }
      let previous = env
        .readonly(&symbol.name)
        .map(|_| env.variables.get(&symbol.name).resolution.clone());
      match env.readonly(&symbol.name) {
        Some(true) => env.failure = CommandFailure::Definite,
        Some(false) => env.failure.uncertain(ValueIssue::Dynamic),
        None => {}
      }
      env.assign(&symbol.name, definition(symbol.id), value);
      if let Some(previous) = previous {
        self.record_use(
          UseSite::Syntax(symbol.definition),
          frame.context,
          previous,
          Some(&symbol.name),
          env,
        );
        return false;
      }
      true
    } else {
      self.invalidate(
        node,
        env,
        frame,
        ResolutionIssue::UndecodedSource,
        ValueIssue::Dynamic,
        [if function {
          EffectTarget::Functions
        } else {
          EffectTarget::Variables
        }],
      );
      false
    }
  }

  fn assignment(node: Node<'a>, env: Environment, frame: Frame) -> Task<'a> {
    let task = if let Some(value) = node.child_by_field_name("value") {
      Task::expand(value, false, env, frame)
    } else {
      Task::expanded(Expansion {
        word: Word::default(),
        env,
      })
    };
    task.then_word(move |resolver, mut result| {
      let value = match resolver.fields(
        node,
        &result.word,
        &result.env,
        frame,
        WordContext::Assignment,
      ) {
        Ok(fields) => {
          fields.into_iter().next().expect("assignment field").value()
        }
        Err(issue) => Value::Unknown(issue),
      };
      if let Some(name) = node.child_by_field_name("name") {
        resolver.write(name, &mut result.env, frame, false, value);
      }
      Task::Done(Flow::next(result.env))
    })
  }
}

struct PipelineState {
  input: Environment,
  output: Environment,
  transfers: Flow,
}

struct LoopState<'a> {
  /// Condition of `while` and `until` with its damaged syntax.
  condition: Vec<Node<'a>>,
  header: Environment,
  output: Flow,
  exit: Flow,
  /// Completion status of the last body run, or zero before any run.
  status: Value,
  value: Value,
}

struct CaseState {
  unmatched: Environment,
  fallthrough: Option<Environment>,
  output: Flow,
}

impl<'a> Resolver<'a> {
  fn conditional(node: Node<'a>, env: Environment, frame: Frame) -> Task<'a> {
    if node.child_by_field_name("body").is_some() {
      let [body] = regions(node, ["body"]);
      return Task::sequence(body, env, frame);
    }
    let [condition, consequence, alternative] =
      regions(node, ["condition", "consequence", "alternative"]);
    Task::sequence(condition, env, frame).then(move |_, mut flow| {
      let Some(state) = flow.take(Transfer::Next) else {
        return Task::Done(flow);
      };
      Task::sequence(consequence, state.clone(), frame).then(
        move |_, consequence| {
          flow.join(consequence);
          if alternative.is_empty() {
            let mut state = state;
            state.set_status(Value::Literal(b"0".to_vec()));
            flow.add(Transfer::Next, state);
            return Task::Done(flow);
          }
          Task::sequence(alternative, state, frame).then(
            move |_, alternative| {
              flow.join(alternative);
              Task::Done(flow)
            },
          )
        },
      )
    })
  }

  fn short_circuit(node: Node<'a>, env: Environment, frame: Frame) -> Task<'a> {
    let [pipelines] = regions(node, ["pipeline"]);
    Self::short_circuit_next(
      pipelines.into_iter().enumerate(),
      Flow::next(env),
      frame,
    )
  }

  fn short_circuit_next(
    mut pipelines: std::iter::Enumerate<std::vec::IntoIter<Node<'a>>>,
    mut flow: Flow,
    frame: Frame,
  ) -> Task<'a> {
    let Some((index, pipeline)) = pipelines.next() else {
      return Task::Done(flow);
    };
    let Some(state) = flow.take(Transfer::Next) else {
      return Task::Done(flow);
    };
    if index > 0 {
      flow.add(Transfer::Next, state.clone());
    }
    Task::eval(pipeline, state, frame).then(move |_, next| {
      flow.join(next);
      Self::short_circuit_next(pipelines, flow, frame)
    })
  }

  fn list(
    &mut self,
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let [entries] = regions(node, ["and_or"]);
    let entries: Vec<_> = entries
      .into_iter()
      .map(|child| (child, parser::asynchronous(child)))
      .collect();
    self.list_next(entries.into_iter(), Flow::next(env), frame)
  }

  fn list_next(
    &mut self,
    mut entries: std::vec::IntoIter<(Node<'a>, bool)>,
    mut flow: Flow,
    frame: Frame,
  ) -> Task<'a> {
    let Some((child, asynchronous)) = entries.next() else {
      return Task::Done(flow);
    };
    let Some(state) = flow.take(Transfer::Next) else {
      return Task::Done(flow);
    };
    let task = if asynchronous {
      let inner = self.execution(
        child,
        frame,
        ExecutionKind::Asynchronous,
        frame.snapshot,
      );
      let mut input = state.clone();
      input.io.connect_input(Err(ValueIssue::Dynamic));
      Task::eval(child, input, inner).then(move |_, child_flow| {
        let mut state = state;
        state.io.consume_input(None);
        state
          .special
          .insert(b"!".to_vec(), Value::Unknown(ValueIssue::Dynamic));
        if child_flow
          .environments()
          .any(|child| child.io.output != state.io.output)
          && let Some(output) = &mut state.io.output
        {
          output.fail(ValueIssue::Dynamic);
        }
        state.set_status(Value::Literal(b"0".to_vec()));
        Task::Done(Flow::next(state))
      })
    } else {
      Task::eval(child, state, frame)
    };

    task.then(move |resolver, next| {
      flow.join(next);
      resolver.list_next(entries, flow, frame)
    })
  }

  fn pipeline(
    &mut self,
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let [commands] = regions(node, ["command"]);
    if commands.len() <= 1 {
      return Task::sequence(commands, env, frame);
    }
    self.pipeline_sequence(node, commands, env, frame)
  }

  fn pipeline_sequence(
    &mut self,
    node: Node<'a>,
    commands: Vec<Node<'a>>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    self.gap(node, frame, ResolutionIssue::PipelineEnvironment);
    let mut env = env;
    if let Some(output) = &mut env.io.output {
      output.fail(ValueIssue::UnsupportedExpansion);
    }
    self
      .pipeline_next(
        commands.clone().into_iter(),
        PipelineState {
          input: env.clone(),
          output: env.clone(),
          transfers: Flow::default(),
        },
        frame,
      )
      .then(move |resolver, mut flow| {
        let Some(next) = flow
          .get(Transfer::Next)
          .filter(|next| **next != env)
          .cloned()
        else {
          return Task::Done(flow);
        };
        resolver
          .pipeline_sequence(node, commands, next, frame)
          .then(move |_, repeated| {
            flow.join(repeated);
            Task::Done(flow)
          })
      })
  }

  fn pipeline_next(
    &mut self,
    mut commands: std::vec::IntoIter<Node<'a>>,
    mut state: PipelineState,
    frame: Frame,
  ) -> Task<'a> {
    let Some(command) = commands.next() else {
      state.transfers.add(Transfer::Next, state.output);
      return Task::Done(state.transfers);
    };
    let inner =
      self.execution(command, frame, ExecutionKind::Pipeline, frame.snapshot);
    let task = Task::eval(command, state.input.clone(), inner);
    state.input.io.connect_input(Err(ValueIssue::Dynamic));
    task.then(move |resolver, flow| {
      for (transfer, env) in flow {
        state.output.widen(&env);
        if transfer != Transfer::Next {
          state.transfers.add(transfer, env);
        }
      }
      resolver.pipeline_next(commands, state, frame)
    })
  }

  fn loop_clause(
    &mut self,
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let list = node.child_by_field_name("words");
    let words: Vec<_> = list
      .into_iter()
      .flat_map(|words| {
        words
          .children_by_field_name("word", &mut words.walk())
          .collect::<Vec<_>>()
      })
      .collect();
    let [mut condition, mut tail, mut after] =
      regions(node, ["condition", "words", "body"]);
    tail.retain(|child| Some(*child) != list);
    let tail: Vec<_> = list
      .map_or_else(Vec::new, |list| damage_outside(list, &["word"]))
      .into_iter()
      .chain(tail)
      .collect();
    after.retain(|child| Some(*child) != node.child_by_field_name("body"));
    // A `for` header runs once around its words; conditions run per iteration.
    let env = if node.kind() == "for_clause" {
      self.recover(&std::mem::take(&mut condition), env, frame)
    } else {
      env
    };
    self
      .arguments(
        words.into_iter(),
        Vec::new(),
        env,
        frame,
        false,
        Box::new(move |resolver, arguments, header| {
          let implicit = node.kind() == "for_clause"
            && !node
              .children(&mut node.walk())
              .any(|child| child.kind() == "in");
          let values: Vec<_> = if implicit {
            header
              .positional
              .fields
              .iter()
              .map(|(value, _)| value.clone())
              .collect()
          } else {
            arguments.into_iter().map(|arg| arg.value).collect()
          };
          let value = Value::join_all(values, ValueIssue::Dynamic);
          Self::loop_next(
            node,
            LoopState {
              condition,
              header: resolver.recover(&tail, header, frame),
              output: Flow::default(),
              exit: Flow::default(),
              status: Value::Literal(b"0".to_vec()),
              value,
            },
            frame,
          )
        }),
      )
      .then_recover(after, frame)
  }

  fn loop_next(
    node: Node<'a>,
    mut state: LoopState<'a>,
    frame: Frame,
  ) -> Task<'a> {
    let inner = Frame {
      loops: frame.loops + 1,
      ..frame
    };
    Task::sequence(state.condition.clone(), state.header.clone(), inner).then(
      move |resolver, mut condition| {
        let task = if let Some(mut body_input) = condition.take(Transfer::Next)
        {
          state.exit.add(Transfer::Next, body_input.clone());
          if let Some(name) = node.child_by_field_name("name") {
            resolver.write(
              name,
              &mut body_input,
              inner,
              false,
              state.value.clone(),
            );
          }
          Task::sequence(
            node.child_by_field_name("body").into_iter().collect(),
            body_input,
            inner,
          )
        } else {
          Task::Done(Flow::default())
        };
        task.then(move |_, body| {
          for transfer in [Transfer::Next, Transfer::Continue(1)] {
            if let Some(env) = body.get(transfer) {
              state.status.join(&env.status());
            }
          }
          let mut next = state.header.clone();
          loop_transfers(body, &mut next, &mut state.output);
          loop_transfers(condition, &mut next, &mut state.output);
          if next == state.header {
            if let Some(mut exit) = state.exit.take(Transfer::Next) {
              exit.set_status(state.status);
              state.output.add(Transfer::Next, exit);
            }
            return Task::Done(state.output);
          }
          state.header = next;
          Self::loop_next(node, state, frame)
        })
      },
    )
  }

  fn case_clause(node: Node<'a>, env: Environment, frame: Frame) -> Task<'a> {
    let [word, items] = regions(node, ["word", "items"]);
    Task::sequence(word, env, frame).then(move |resolver, mut output| {
      let Some(unmatched) = output.take(Transfer::Next) else {
        return Task::Done(output);
      };
      let mut pending: Vec<_> = items.into_iter().rev().collect();
      let mut items = Vec::new();
      while let Some(item) = pending.pop() {
        if matches!(item.kind(), "case_list" | "case_list_ns") {
          let [terminated, item] = regions(item, ["terminated", "item"]);
          pending.extend(terminated.into_iter().chain(item).rev());
        } else {
          items.push(item);
        }
      }
      resolver.case_next(
        items.into_iter(),
        CaseState {
          unmatched,
          fallthrough: None,
          output,
        },
        frame,
      )
    })
  }

  /// Runs damaged syntax as unknown commands.
  fn recover(
    &mut self,
    damaged: &[Node<'_>],
    env: Environment,
    frame: Frame,
  ) -> Environment {
    damaged.iter().fold(env, |env, node| {
      self
        .opaque(*node, env, frame, ResolutionIssue::Recovery, None)
        .take(Transfer::Next)
        .expect("opaque state")
    })
  }

  fn case_next(
    &mut self,
    mut items: std::vec::IntoIter<Node<'a>>,
    mut state: CaseState,
    frame: Frame,
  ) -> Task<'a> {
    let Some(item) = items.next() else {
      if let Some(env) = state.fallthrough {
        state.output.add(Transfer::Next, env);
      }
      state.unmatched.set_status(Value::Literal(b"0".to_vec()));
      state.output.add(Transfer::Next, state.unmatched);
      return Task::Done(state.output);
    };
    if !matches!(item.kind(), "case_item" | "case_item_ns") {
      // Damaged syntax between clauses runs on the paths that reach it.
      state.unmatched = self.recover(&[item], state.unmatched, frame);
      state.fallthrough = state
        .fallthrough
        .map(|env| self.recover(&[item], env, frame));
      return self.case_next(items, state, frame);
    }
    let patterns: Vec<_> =
      item.child_by_field_name("patterns").into_iter().collect();
    // Clause patterns expand in an unspecified order until one matches.
    let unordered = patterns.first().is_some_and(|patterns| {
      patterns
        .children_by_field_name("word", &mut patterns.walk())
        .nth(1)
        .is_some()
    });
    let affected = if unordered {
      self.expansion_targets(patterns.iter().copied(), frame)
    } else {
      BTreeSet::new()
    };
    let expanded = if unordered
      && (!affected.is_empty() || state.unmatched.io.input.is_ok())
    {
      self.uncertain_order(
        item,
        patterns,
        affected.clone(),
        state.unmatched.clone(),
        frame,
      )
    } else {
      Task::sequence(patterns, state.unmatched.clone(), frame)
    };
    expanded.then(move |resolver, mut patterns| {
      let input = patterns.take(Transfer::Next);
      state.output.join(patterns);
      let Some(mut input) = input else {
        return resolver.case_next(items, state, frame);
      };
      resolver.invalidate(
        item,
        &mut input,
        frame,
        ResolutionIssue::EvaluationOrder,
        ValueIssue::Dynamic,
        affected,
      );
      state.unmatched = input.clone();
      if let Some(previous) = state.fallthrough.take() {
        input.join(&previous);
      }
      let body = item.child_by_field_name("body");
      let fallthrough = item
        .child_by_field_name("terminator")
        .is_some_and(|end| end.kind() == "semi_and");
      if body.is_none() && (!fallthrough || items.len() == 0) {
        input.set_status(Value::Literal(b"0".to_vec()));
      }
      let [patterns, body] = regions(item, ["patterns", "body"]);
      let nodes = patterns
        .into_iter()
        .filter(|child| Some(*child) != item.child_by_field_name("patterns"))
        .chain(body)
        .collect();
      Task::sequence(nodes, input, frame).then(move |resolver, mut body| {
        if fallthrough {
          state.fallthrough = body.take(Transfer::Next);
        }
        state.output.join(body);
        resolver.case_next(items, state, frame)
      })
    })
  }
}

fn loop_transfers(flow: Flow, next: &mut Environment, output: &mut Flow) {
  for (transfer, state) in flow {
    match transfer {
      Transfer::Next | Transfer::Continue(1) => next.widen(&state),
      Transfer::Break(1) => output.add(Transfer::Next, state),
      Transfer::Break(depth) => output.add(Transfer::Break(depth - 1), state),
      Transfer::Continue(depth) => {
        output.add(Transfer::Continue(depth - 1), state);
      }
      Transfer::Return => output.add(Transfer::Return, state),
    }
  }
}

enum SubstitutionBody {
  Empty,
  Redirections,
  Commands,
}

fn substitution_body(node: Node<'_>) -> SubstitutionBody {
  let mut pending = children(node);
  let mut redirections = false;
  while let Some(child) = pending.pop() {
    if child.is_error() || child.is_missing() {
      return SubstitutionBody::Commands;
    }
    match child.kind() {
      "command" => {
        let Some(body) = child.child_by_field_name("body") else {
          return SubstitutionBody::Commands;
        };
        if body.kind() != "simple_command"
          || body.child_by_field_name("name").is_some()
          || body.child_by_field_name("word").is_some()
          || body
            .child_by_field_name("prefix")
            .into_iter()
            .flat_map(children)
            .any(|part| part.kind() != "io_redirect")
        {
          return SubstitutionBody::Commands;
        }
        redirections = true;
      }
      "pipeline" if child.child_by_field_name("negation").is_some() => {
        return SubstitutionBody::Commands;
      }
      "comment"
      | "here_document"
      | "here_document_end"
      | "quoted_here_document_body" => {}
      _ => pending.extend(children(child)),
    }
  }
  if redirections {
    if node.kind() == "command_substitution" {
      SubstitutionBody::Redirections
    } else {
      SubstitutionBody::Commands
    }
  } else {
    SubstitutionBody::Empty
  }
}

pub(in crate::interface::application::engine::analysis) fn children(
  node: Node<'_>,
) -> Vec<Node<'_>> {
  node.named_children(&mut node.walk()).collect()
}

/// Groups the children in `fields` with the following children that contain
/// damaged syntax; damaged children before every field join the first group.
fn regions<'a, const N: usize>(
  node: Node<'a>,
  fields: [&str; N],
) -> [Vec<Node<'a>>; N] {
  let mut regions = std::array::from_fn(|_| Vec::new());
  let mut current = 0;
  let mut cursor = node.walk();
  let mut more = cursor.goto_first_child();
  while more {
    let child = cursor.node();
    if let Some(index) = cursor
      .field_name()
      .and_then(|name| fields.iter().position(|field| *field == name))
    {
      current = index;
      regions[index].push(child);
    } else if child.has_error() {
      regions[current].push(child);
    }
    more = cursor.goto_next_sibling();
  }
  regions
}

/// Children outside `fields` that contain damaged syntax.
pub(in crate::interface::application::engine::analysis) fn damage_outside<
  'a,
>(
  node: Node<'a>,
  fields: &[&str],
) -> Vec<Node<'a>> {
  let mut cursor = node.walk();
  let mut nodes = Vec::new();
  let mut more = cursor.goto_first_child();
  while more {
    if cursor
      .field_name()
      .is_none_or(|name| !fields.contains(&name))
      && cursor.node().has_error()
    {
      nodes.push(cursor.node());
    }
    more = cursor.goto_next_sibling();
  }
  nodes
}

type FlowContinuation<'a> =
  Box<dyn FnOnce(&mut Resolver<'a>, Flow) -> Task<'a> + 'a>;

type WordContinuation<'a> =
  Box<dyn FnOnce(&mut Resolver<'a>, Expansion) -> Task<'a> + 'a>;

pub(in crate::interface::application::engine::analysis) enum Continuation<'a> {
  Flow(FlowContinuation<'a>),
  Word(WordContinuation<'a>),
  Observe(ExecutionContextId),
}

pub(in crate::interface::application::engine::analysis) enum Task<'a> {
  Expand(Node<'a>, bool, Box<Environment>, Frame),
  Arithmetic(Node<'a>, Box<Environment>, Frame),
  Eval(Node<'a>, Box<Environment>, Frame),
  Done(Flow),
  Expanded(Box<Expansion>),
  Then(Box<Self>, Continuation<'a>),
}

impl<'a> Task<'a> {
  pub fn arithmetic(node: Node<'a>, env: Environment, frame: Frame) -> Self {
    Self::Arithmetic(node, Box::new(env), frame)
  }

  pub fn expand(
    node: Node<'a>,
    quoted: bool,
    env: Environment,
    frame: Frame,
  ) -> Self {
    Self::Expand(node, quoted, Box::new(env), frame)
  }

  pub fn expanded(result: Expansion) -> Self {
    Self::Expanded(Box::new(result))
  }

  pub fn eval(node: Node<'a>, env: Environment, frame: Frame) -> Self {
    Self::Eval(node, Box::new(env), frame)
  }

  pub fn then(
    self,
    resume: impl FnOnce(&mut Resolver<'a>, Flow) -> Self + 'a,
  ) -> Self {
    Self::Then(Box::new(self), Continuation::Flow(Box::new(resume)))
  }

  pub fn then_word(
    self,
    resume: impl FnOnce(&mut Resolver<'a>, Expansion) -> Self + 'a,
  ) -> Self {
    Self::Then(Box::new(self), Continuation::Word(Box::new(resume)))
  }

  /// Runs damaged syntax as unknown commands after normal completion.
  fn then_recover(self, damaged: Vec<Node<'a>>, frame: Frame) -> Self {
    if damaged.is_empty() {
      return self;
    }
    self.then(move |resolver, mut flow| {
      if let Some(env) = flow.take(Transfer::Next) {
        flow.add(Transfer::Next, resolver.recover(&damaged, env, frame));
      }
      Self::Done(flow)
    })
  }

  pub fn sequence(
    nodes: Vec<Node<'a>>,
    env: Environment,
    frame: Frame,
  ) -> Self {
    if let [node] = nodes[..] {
      return Self::eval(node, env, frame);
    }
    Self::sequence_next(nodes.into_iter(), Flow::next(env), frame)
  }

  fn sequence_next(
    mut nodes: std::vec::IntoIter<Node<'a>>,
    mut flow: Flow,
    frame: Frame,
  ) -> Self {
    let Some(node) = nodes.next() else {
      return Self::Done(flow);
    };
    let Some(env) = flow.take(Transfer::Next) else {
      return Self::Done(flow);
    };
    Self::eval(node, env, frame).then(move |_, next| {
      flow.join(next);
      Self::sequence_next(nodes, flow, frame)
    })
  }
}

impl<'a> Resolver<'a> {
  pub(in crate::interface::application::engine::analysis) fn run(
    &mut self,
    mut task: Task<'a>,
  ) -> Result<Flow, Error> {
    let mut pending = Vec::new();
    loop {
      check_cancelled(self.cancelled)?;
      if let Some(error) = self.error.take() {
        return Err(error);
      }
      if self.track_invariants
        && let Task::Expand(_, _, env, frame)
        | Task::Arithmetic(_, env, frame)
        | Task::Eval(_, env, frame) = &task
      {
        let environment = self.result.contexts[frame.context.0].environment;
        self.invariants.observe(environment, env);
        pending.push(Continuation::Observe(environment));
      }
      task = match task {
        Task::Expand(node, quoted, env, frame) => {
          self.expand_start(node, quoted, *env, frame)
        }
        Task::Arithmetic(node, env, frame) => {
          self.arithmetic(node, *env, frame)
        }
        Task::Eval(node, env, frame) => self.eval(node, *env, frame),
        Task::Then(next, resume) => {
          pending.push(resume);
          *next
        }
        Task::Expanded(expansion) => {
          if let Some(Continuation::Observe(environment)) = pending.last() {
            self.invariants.observe(*environment, &expansion.env);
            pending.pop();
            task = Task::Expanded(expansion);
            continue;
          }
          let Some(Continuation::Word(resume)) = pending.pop() else {
            unreachable!("word continuation")
          };
          resume(self, *expansion)
        }
        Task::Done(flow) => {
          if let Some(Continuation::Observe(environment)) = pending.last() {
            for env in flow.environments() {
              self.invariants.observe(*environment, env);
            }
            pending.pop();
            task = Task::Done(flow);
            continue;
          }
          let Some(resume) = pending.pop() else {
            return Ok(flow);
          };
          let Continuation::Flow(resume) = resume else {
            unreachable!("flow continuation")
          };
          resume(self, flow)
        }
      };
    }
  }
}
