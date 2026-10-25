use super::super::deferred::OutputTrace;
use super::super::execution::{
  Binding, Environment, Flow, OptionCursor, StateKey, Transfer, alternative,
};
use super::super::expansion::{Argument, positional_arguments};
use super::super::{BindingOrigin, Frame, Resolver, Task};
use super::arguments::{
  ReadOptions, argument_issue, classify_arguments, decimal_value,
  declaration_operand, declaration_operands, getopts_operands,
  numeric_argument, read_options, trap_action, unset_options,
};
use super::effects::{NameTarget, argument_target, attribute_target};
use crate::interface::application::engine::valid_name;
use crate::interface::application::engine::word;
use crate::interface::application::engine::workspace::parser;
use crate::interface::application::engine::{
  Access, Alternative, ArgumentKind, DerivedOrigin, EffectTarget, InputQuery,
  InputResult, OccurrenceId, OriginMapping, OriginPart, ResolutionIssue,
  SymbolId, UseSite, Value, ValueIssue,
};
use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::Arc;
use tree_sitter::Node;

pub(super) fn uses_arguments(name: &[u8]) -> bool {
  numeric_argument(name)
    || matches!(
      name,
      b"export"
        | b"readonly"
        | b"unset"
        | b"set"
        | b"cd"
        | b"."
        | b"read"
        | b"getopts"
        | b"trap"
    )
}

/// External implementations also leave standard input unread.
pub(super) fn preserves_input(name: &[u8]) -> bool {
  matches!(
    name,
    b"true" | b"false" | b"echo" | b"printf" | b"pwd" | b"test" | b"["
  )
}

impl<'a> Resolver<'a> {
  pub(super) fn utility(
    &mut self,
    node: Node<'a>,
    name: &[u8],
    args: Vec<Argument<'a>>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    if (name == b"shift" && decimal_value::<usize>(&args) != Some(0))
      || (name == b"getopts" && args.len() == 2)
    {
      let names = BTreeSet::from([StateKey::Positionals]);
      if env.alternatives.choice(&names).is_some() {
        let name = name.to_vec();
        return self.each_alternative(env, names, move |resolver, env| {
          resolver.utility(node, &name, args.clone(), env, frame)
        });
      }
    }
    if name == b"command" {
      return self.forward(node, args.into_iter(), env, frame);
    }
    self.builtin(node, name, args, env, frame)
  }

  fn builtin(
    &mut self,
    node: Node<'a>,
    name: &[u8],
    mut args: Vec<Argument<'a>>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let source = self.workspace.source(frame.snapshot.document);
    classify_arguments(name, &mut args, Some(&env), source);
    if name == b"exec" {
      if !args.is_empty() {
        env.io.consume_input(None);
      }
      for arg in &args {
        if matches!(arg.kind, Some(ArgumentKind::Command { .. })) {
          self.record_argument_use(
            arg,
            alternative(Alternative::ExternalCommand),
            &env,
            frame,
          );
        }
      }
    }
    if name != b"." {
      self.non_source(node, frame);
    }
    if env.io.output.is_some() {
      let (output, trace) = self.utility_output(node, name, &args, &env, frame);
      env.emit(output, trace, self.result.request.limits.value_bytes);
      if env.io.stderr_captured
        && let Some(output) = &mut env.io.output
      {
        output.fail(ValueIssue::Dynamic);
      }
    }
    let previous_status = env.status();
    env.set_status(Value::Unknown(ValueIssue::Dynamic));
    let task = match name {
      b":" | b"true" | b"false" => {
        env.set_status(Value::Literal(
          if name == b"false" { b"1" } else { b"0" }.to_vec(),
        ));
        Task::Done(Flow::next(env))
      }
      b"export" | b"readonly" => {
        Task::Done(self.declaration(node, name, &args, env, frame))
      }
      b"unset" => Task::Done(self.unset(node, &args, env, frame)),
      b"set" => Task::Done(self.set_options(node, &args, env, frame)),
      b"cd" => Task::Done(self.cd(node, &args, env, frame)),
      b"shift" => Task::Done(self.shift(node, &args, env, frame)),
      b"." => {
        env.set_status(previous_status);
        self.source(node, &args, env, frame)
      }
      b"trap" => self.trap(node, &args, env, frame),
      b"eval" | b"alias" | b"unalias" => {
        self.gap(node, frame, ResolutionIssue::ExcludedEffect);
        Task::Done(Flow::next(env))
      }
      b"read" => Task::Done(self.read_arguments(node, &args, env, frame)),
      b"getopts" => Task::Done(self.getopts(node, &args, env, frame)),
      b"echo" | b"printf" | b"pwd" | b"test" | b"[" | b"umask" | b"wait"
      | b"jobs" | b"fg" | b"bg" | b"hash" | b"type" | b"ulimit" | b"times"
      | b"kill" | b"exec" => Task::Done(Flow::next(env)),
      b"exit" | b"return" => {
        let status = exit_status(&args, previous_status);
        env.set_status(status);
        Task::Done(if name == b"exit" {
          Flow::next(env)
        } else if frame.returnable {
          Flow::single(Transfer::Return, env)
        } else {
          self.opaque(
            node,
            env,
            frame,
            ResolutionIssue::UnsupportedControl,
            None,
          )
        })
      }
      b"break" | b"continue" => {
        Task::Done(self.control(node, name, &args, env, frame))
      }
      _ => Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::UnknownEffect,
        None,
      )),
    };
    for argument in args {
      self.argument_role(&argument, argument.kind.clone(), frame);
    }
    task
  }

  fn set_options(
    &mut self,
    node: Node<'_>,
    args: &[Argument<'_>],
    mut env: Environment,
    frame: Frame,
  ) -> Flow {
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
      if let Err(issue) = argument.count.boundary() {
        return self.unknown_options(node, env, frame, issue);
      }
      let Value::Literal(option) = argument.value.clone() else {
        return self.unknown_options(
          node,
          env,
          frame,
          argument.value.issue().expect("unknown option"),
        );
      };
      if option == b"--" {
        env.alternatives.forget_positionals();
        env.positional = positional_arguments(arguments.as_slice());
        return Flow::next(env);
      }
      if !matches!(option.first(), Some(b'-' | b'+')) {
        let rest = std::iter::once(argument.clone())
          .chain(arguments.cloned())
          .collect::<Vec<_>>();
        env.alternatives.forget_positionals();
        env.positional = positional_arguments(&rest);
        return Flow::next(env);
      }
      env.special.remove(b"-".as_slice());
      let mut letters = option[1..].iter();
      if letters.as_slice().is_empty() {
        return self.unknown_options(node, env, frame, ValueIssue::Dynamic);
      }
      while let Some(letter) = letters.next() {
        match letter {
          b'e' => env.errexit = Some(option[0] == b'-'),
          b'u' => env.nounset = Some(option[0] == b'-'),
          b'f' => env.noglob = Some(option[0] == b'-'),
          b'a' | b'b' | b'C' | b'h' | b'm' | b'n' | b'v' | b'x' => {}
          b'o' => {
            let name = if letters.as_slice().is_empty() {
              let Some(name) = arguments.next() else {
                return Flow::next(env);
              };
              name.value.clone()
            } else {
              Value::Literal(letters.as_slice().to_vec())
            };
            if let Value::Literal(name) = &name
              && matches!(
                name.as_slice(),
                b"errexit"
                  | b"nounset"
                  | b"noglob"
                  | b"allexport"
                  | b"notify"
                  | b"noclobber"
                  | b"monitor"
                  | b"noexec"
                  | b"nolog"
                  | b"pipefail"
                  | b"verbose"
                  | b"xtrace"
                  | b"ignoreeof"
                  | b"vi"
              )
            {
              if name == b"errexit" {
                env.errexit = Some(option[0] == b'-');
              } else if name == b"nounset" {
                env.nounset = Some(option[0] == b'-');
              } else if name == b"noglob" {
                env.noglob = Some(option[0] == b'-');
              }
              break;
            }
            return self.unknown_options(
              node,
              env,
              frame,
              name.issue().unwrap_or(ValueIssue::Dynamic),
            );
          }
          _ => {
            return self.unknown_options(node, env, frame, ValueIssue::Dynamic);
          }
        }
      }
    }
    Flow::next(env)
  }

  fn declaration(
    &mut self,
    node: Node<'_>,
    utility: &[u8],
    args: &[Argument<'_>],
    mut env: Environment,
    frame: Frame,
  ) -> Flow {
    let source = self.workspace.source(frame.snapshot.document);
    let Some(start) = declaration_operands(args) else {
      let attributes =
        (utility == b"readonly").then_some(EffectTarget::Attributes);
      self.invalidate(
        node,
        &mut env,
        frame,
        ResolutionIssue::UnknownEffect,
        argument_issue(args).unwrap_or(ValueIssue::Dynamic),
        std::iter::once(EffectTarget::Variables).chain(attributes),
      );
      return Flow::next(env);
    };
    for argument in &args[start..] {
      let variable = argument.node.child_by_field_name("name");
      let mut definition =
        variable.and_then(|variable| self.site(variable, frame));
      let name = if let Some(variable) = variable {
        if self.write(variable, &mut env, frame, false, argument.value.clone())
          && let Some(expression) = argument.expression
        {
          let spelling = word::lexical(variable, source);
          let resolution = env.variables.get(&spelling).resolution.clone();
          env.alternatives.store(&spelling, expression, resolution);
        }
        definition
          .and_then(|id| self.workspace.occurrence(id))
          .map(|site| site.name.clone())
      } else if argument.count.boundary().is_ok() {
        if let Some((name, value)) = declaration_operand(&argument.value) {
          if !valid_name(&name) {
            self.gap(node, frame, ResolutionIssue::DynamicName);
            continue;
          }
          if let Some(value) = value {
            definition = self.write_argument(
              node,
              Some(argument),
              &name,
              value,
              &mut env,
              frame,
            );
          }
          Some(Value::Literal(name))
        } else {
          None
        }
      } else {
        None
      };
      if let Some(Value::Literal(name)) = name {
        if matches!(
          argument.kind,
          Some(ArgumentKind::Variable {
            access: Access::NameReference,
            ..
          })
        ) {
          self.record_argument_use(
            argument,
            env.variables.get(&name).resolution.clone(),
            &env,
            frame,
          );
        }
        if utility == b"readonly" {
          env.readonly.insert(name.clone(), true);
        }
      } else {
        let target = argument_target(argument, NameTarget::Declaration);
        let attribute = attribute_target(&target);
        self.invalidate(
          node,
          &mut env,
          frame,
          ResolutionIssue::DynamicName,
          argument.value.issue().unwrap_or(ValueIssue::Dynamic),
          [target],
        );
        if utility == b"readonly" {
          self.invalidate(
            node,
            &mut env,
            frame,
            ResolutionIssue::UnknownEffect,
            argument.value.issue().unwrap_or(ValueIssue::Dynamic),
            [attribute],
          );
        }
      }
      if let Some(id) = definition {
        self.record_attribute(id, frame.context, &env);
      }
    }
    Flow::next(env)
  }

  fn unset(
    &mut self,
    node: Node<'_>,
    args: &[Argument<'_>],
    mut env: Environment,
    frame: Frame,
  ) -> Flow {
    let Some((index, functions, explicit)) = unset_options(args) else {
      self.invalidate(
        node,
        &mut env,
        frame,
        ResolutionIssue::DynamicName,
        argument_issue(args).unwrap_or(ValueIssue::Dynamic),
        [EffectTarget::Variables, EffectTarget::Functions],
      );
      return Flow::next(env);
    };
    for arg in &args[index..] {
      let Some(name) = arg.literal() else {
        let targets = if !explicit {
          vec![
            argument_target(arg, NameTarget::Variable),
            argument_target(arg, NameTarget::Function),
          ]
        } else if functions {
          vec![argument_target(arg, NameTarget::Function)]
        } else {
          vec![argument_target(arg, NameTarget::Variable)]
        };
        self.invalidate(
          node,
          &mut env,
          frame,
          ResolutionIssue::DynamicName,
          arg.value.issue().unwrap_or(ValueIssue::Dynamic),
          targets,
        );
        continue;
      };
      let mut resolution = if functions {
        env.functions.get(name).resolution.clone()
      } else {
        env.variables.get(name).resolution.clone()
      };
      let function_unset =
        !explicit && resolution.alternatives.contains(&Alternative::Unset);
      if function_unset {
        resolution.issues.insert(ResolutionIssue::UnknownEffect);
      }
      self.record_argument_use(arg, resolution, &env, frame);
      if !functions && env.readonly(name) == Some(true) {
        continue;
      }
      let conditional_readonly = !functions && env.readonly(name).is_some();
      if functions {
        env.alternatives.forget_function(name);
      } else {
        env.alternatives.forget(name);
        env.variable_changed(name, None);
      }
      let bindings = if functions {
        &mut env.functions
      } else {
        &mut env.variables
      };
      let before = bindings.get(name).clone();
      let mut after = Binding::new(
        alternative(Alternative::Unset)
          .in_context(self.result.binding_context(frame.context)),
      );
      if conditional_readonly {
        after.join(&before);
        after
          .resolution
          .issues
          .insert(ResolutionIssue::UnknownEffect);
      }
      bindings.values.insert(name.to_vec(), after);
      if function_unset {
        env.alternatives.forget_function(name);
        let mut function = env.functions.get(name).clone();
        function.join(&Binding::new(
          alternative(Alternative::Unset)
            .in_context(self.result.binding_context(frame.context)),
        ));
        env.functions.values.insert(name.to_vec(), function);
      }
    }
    Flow::next(env)
  }

  fn control(
    &mut self,
    node: Node<'_>,
    name: &[u8],
    args: &[Argument<'_>],
    env: Environment,
    frame: Frame,
  ) -> Flow {
    let depth = if args.is_empty() {
      Some(1)
    } else {
      decimal_value::<u32>(args).filter(|depth| *depth > 0)
    };
    if let Some(depth) = depth
      && frame.loops > 0
      && !(depth > frame.loops && frame.dynamic_loops)
    {
      let depth = depth.min(frame.loops);
      let transfer = if name == b"break" {
        Transfer::Break(depth)
      } else {
        Transfer::Continue(depth)
      };
      Flow::single(transfer, env)
    } else {
      self.opaque(node, env, frame, ResolutionIssue::UnsupportedControl, None)
    }
  }
}

impl Resolver<'_> {
  fn trap<'a>(
    &mut self,
    node: Node<'a>,
    args: &[Argument<'a>],
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    if let Some(command) =
      parser::command_word(node).and_then(|name| self.site(name, frame))
    {
      let action = trap_action(args).map(|index| {
        index.map(|index| {
          let action = &args[index];
          (
            DerivedOrigin {
              argument: frame.location(action.node),
              parts: action.origins.clone(),
            },
            action.value.clone(),
          )
        })
      });
      if let Err(error) = self.defer_body(node, command, action, &env, frame) {
        self.error = Some(error);
      }
    }
    self.gap(node, frame, ResolutionIssue::ExcludedEffect);
    Task::Done(Flow::next(env))
  }
}

impl Resolver<'_> {
  fn cd(
    &mut self,
    node: Node<'_>,
    args: &[Argument<'_>],
    mut env: Environment,
    frame: Frame,
  ) -> Flow {
    let (operand, physical) = match cd_arguments(args, &env) {
      Ok(arguments) => arguments,
      Err(issue) => return self.unknown_directory(node, env, frame, issue),
    };
    if env.directory_variables_modified
      || env.readonly(b"PWD").is_some()
      || env.readonly(b"OLDPWD").is_some()
    {
      return self.unknown_directory(node, env, frame, ValueIssue::Dynamic);
    }
    let cwd = if operand.starts_with(b"/") {
      Vec::new()
    } else if let Value::Literal(cwd) = &env.cwd {
      cwd.clone()
    } else {
      let issue = env.cwd.issue().expect("unknown cwd");
      return self.unknown_directory(node, env, frame, issue);
    };
    let mut paths = Vec::new();
    if !operand.starts_with(b"/")
      && !matches!(operand.split(|c| *c == b'/').next(), Some(b"." | b".."))
    {
      match env.parameter(b"CDPATH") {
        (_, Some(true)) => {}
        (Value::Literal(path), _) => {
          for entry in path.split(|c| *c == b':') {
            let mut path = entry.to_vec();
            if !path.is_empty() && !path.ends_with(b"/") {
              path.push(b'/');
            }
            path.extend(&operand);
            paths.push(path);
          }
        }
        (value, _) => {
          return self.unknown_directory(
            node,
            env,
            frame,
            value.issue().expect("unknown CDPATH"),
          );
        }
      }
    }
    paths.push(operand);
    for path in paths {
      let absolute = path.starts_with(b"/");
      match self.input(&InputQuery::Directory {
        cwd: cwd.clone(),
        path,
        physical,
      }) {
        InputResult::Path(curpath) => {
          let previous = env.parameter(b"PWD").0;
          let directory = simplified(&curpath);
          // Step 8c simplifications and an inexact PWD prefix are optional.
          let pwd = if directory == curpath
            && (absolute || physical || matches!(previous, Value::Literal(_)))
          {
            Value::Literal(curpath)
          } else {
            Value::Unknown(ValueIssue::Unspecified)
          };
          env.cwd = Value::Literal(directory);
          for (name, value) in [(b"OLDPWD".as_slice(), previous), (b"PWD", pwd)]
          {
            env.alternatives.forget(name);
            env.variables.values.insert(
              name.to_vec(),
              Binding {
                resolution: alternative(Alternative::External)
                  .in_context(self.result.binding_context(frame.context)),
                value: Arc::new(value),
              },
            );
          }
          env.set_status(Value::Literal(b"0".to_vec()));
          return Flow::next(env);
        }
        InputResult::Missing => {}
        InputResult::Unknown(issue) => {
          return self.unknown_directory(node, env, frame, issue);
        }
        _ => {
          return self.unknown_directory(node, env, frame, ValueIssue::Dynamic);
        }
      }
    }
    env.set_status(Value::Unknown(ValueIssue::Dynamic));
    Flow::next(env)
  }

  fn unknown_directory(
    &mut self,
    node: Node<'_>,
    mut env: Environment,
    frame: Frame,
    issue: ValueIssue,
  ) -> Flow {
    self.invalidate(
      node,
      &mut env,
      frame,
      ResolutionIssue::UnknownEffect,
      issue,
      [EffectTarget::Directory],
    );
    env.set_status(Value::Unknown(issue));
    Flow::next(env)
  }
}

/// Applies every simplification that POSIX `cd` step 8c permits.
fn simplified(path: &[u8]) -> Vec<u8> {
  let leading = path.iter().take_while(|byte| **byte == b'/').count();
  let mut result = vec![b'/'; if leading == 2 { 2 } else { leading.min(1) }];
  for component in path[leading..]
    .split(|byte| *byte == b'/')
    .filter(|component| !component.is_empty())
  {
    if result.last().is_some_and(|byte| *byte != b'/') {
      result.push(b'/');
    }
    result.extend(component);
  }
  result
}

fn cd_arguments(
  args: &[Argument<'_>],
  env: &Environment,
) -> Result<(Vec<u8>, bool), ValueIssue> {
  let mut physical = false;
  let mut index = 0;
  while let Some(arg) = args.get(index) {
    let value = arg.value.literal()?;
    arg.count.boundary()?;
    if value == b"--" {
      index += 1;
      break;
    }
    if !value.starts_with(b"-") || value == b"-" {
      break;
    }
    for flag in &value[1..] {
      match flag {
        b'P' => physical = true,
        b'L' => physical = false,
        b'e' => {}
        _ => return Err(ValueIssue::Dynamic),
      }
    }
    index += 1;
  }
  let operand = match &args[index..] {
    [] => match env.parameter(b"HOME") {
      (Value::Literal(home), Some(false)) if !home.is_empty() => home,
      (value, _) => return Err(value.issue().unwrap_or(ValueIssue::Dynamic)),
    },
    [arg] => match &arg.value {
      Value::Literal(value) if value == b"-" => {
        match env.parameter(b"OLDPWD").0 {
          Value::Literal(path) if path.starts_with(b"/") => path,
          value => return Err(value.issue().unwrap_or(ValueIssue::Dynamic)),
        }
      }
      Value::Literal(value) if !value.is_empty() => value.clone(),
      value => return Err(value.issue().unwrap_or(ValueIssue::Dynamic)),
    },
    _ => return Err(ValueIssue::Dynamic),
  };
  Ok((operand, physical))
}

impl Resolver<'_> {
  fn shift(
    &mut self,
    node: Node<'_>,
    args: &[Argument<'_>],
    mut env: Environment,
    frame: Frame,
  ) -> Flow {
    let count = if args.is_empty() {
      Some(1)
    } else {
      decimal_value::<usize>(args)
    };
    if count != Some(0) {
      env.alternatives.forget_positionals();
    }
    if count.is_some_and(|count| env.positional.shift(count)) {
      env.set_status(Value::Literal(b"0".to_vec()));
    } else {
      let issue = argument_issue(args)
        .or_else(|| env.positional.count().issue())
        .unwrap_or(ValueIssue::Dynamic);
      self.invalidate(
        node,
        &mut env,
        frame,
        ResolutionIssue::UnknownEffect,
        issue,
        [EffectTarget::Positionals],
      );
      env.set_status(Value::Unknown(
        env
          .positional
          .count()
          .issue()
          .expect("unknown positional count"),
      ));
    }
    Flow::next(env)
  }
}

impl Resolver<'_> {
  fn unknown_options(
    &mut self,
    node: Node<'_>,
    mut env: Environment,
    frame: Frame,
    issue: ValueIssue,
  ) -> Flow {
    self.invalidate(
      node,
      &mut env,
      frame,
      ResolutionIssue::UnknownEffect,
      issue,
      [EffectTarget::Options, EffectTarget::Positionals],
    );
    env.set_status(Value::Unknown(issue));
    Flow::next(env)
  }

  fn read_arguments(
    &mut self,
    node: Node<'_>,
    args: &[Argument<'_>],
    mut env: Environment,
    frame: Frame,
  ) -> Flow {
    let Some(options) = read_options(args) else {
      env.io.consume_input(None);
      self.invalidate(
        node,
        &mut env,
        frame,
        ResolutionIssue::DynamicName,
        argument_issue(args).unwrap_or(ValueIssue::Dynamic),
        [EffectTarget::Variables],
      );
      return Flow::next(env);
    };
    let values = self.read_values(args, &options, &mut env);
    if let Err(issue) = values {
      env.set_status(Value::Unknown(issue));
    }
    let mut failed = None;
    for (offset, arg) in args[options.operands..].iter().enumerate() {
      let name = arg.literal();
      if failed.is_none()
        && !name
          .is_some_and(|name| valid_name(name) && env.readonly(name).is_none())
      {
        failed = Some(env.clone());
      }
      if let Some(name) = name
        && valid_name(name)
      {
        if let Some(id) = self.write_argument(
          node,
          Some(arg),
          name,
          values.as_ref().map_or_else(
            |issue| Value::Unknown(*issue),
            |values| {
              values
                .get(offset)
                .cloned()
                .unwrap_or(Value::Literal(Vec::new()))
            },
          ),
          &mut env,
          frame,
        ) {
          self.record_attribute(id, frame.context, &env);
        }
      } else if name.is_some() {
        self.gap(node, frame, ResolutionIssue::DynamicName);
      } else {
        self.invalidate(
          node,
          &mut env,
          frame,
          ResolutionIssue::DynamicName,
          arg.value.issue().unwrap_or(ValueIssue::Dynamic),
          [argument_target(arg, NameTarget::Variable)],
        );
      }
    }
    if let Some(failed) = failed {
      env.join(&failed);
    }
    Flow::next(env)
  }

  fn getopts(
    &mut self,
    node: Node<'_>,
    args: &[Argument<'_>],
    mut env: Environment,
    frame: Frame,
  ) -> Flow {
    let operands = getopts_operands(args);
    let args = &args[operands.unwrap_or(0)..];
    let computed = operands
      .ok_or_else(|| argument_issue(args).unwrap_or(ValueIssue::Dynamic))
      .and_then(|_| option_values(args, &env, frame.functions));
    let known_prefix = operands.is_some()
      && args.iter().take(2).all(|arg| arg.count.boundary().is_ok());
    if known_prefix
      && (args.len() < 2
        || matches!(&args[1].value, Value::Literal(name) if !valid_name(name)))
    {
      self.gap(node, frame, ResolutionIssue::DynamicName);
      return Flow::next(env);
    }
    for name in [b"OPTIND".as_slice(), b"OPTARG"] {
      let _ = self.write_argument(
        node,
        None,
        name,
        computed.as_ref().map_or_else(
          |issue| Value::Unknown(*issue),
          |result| match &result.argument {
            Some(_) if name == b"OPTIND" => {
              Value::Literal(result.cursor.index.to_string().into_bytes())
            }
            // POSIX fixes OPTIND only after an option-argument; the option
            // cursor keeps the position for the next getopts.
            None if name == b"OPTIND" => {
              Value::Unknown(ValueIssue::Unspecified)
            }
            argument => argument.clone().unwrap_or(Value::Literal(Vec::new())),
          },
        ),
        &mut env,
        frame,
      );
    }
    if env.readonly(b"OPTARG") != Some(true) {
      env.alternatives.forget(b"OPTARG");
      let mut binding = env.variables.get(b"OPTARG").clone();
      if env.readonly(b"OPTARG").is_none()
        && computed
          .as_ref()
          .is_ok_and(|result| result.argument.is_none())
      {
        binding = Binding::new(
          alternative(Alternative::Unset)
            .in_context(self.result.binding_context(frame.context)),
        );
      } else if computed.is_err() {
        binding.resolution.alternatives.insert(Alternative::Unset);
        binding.resolution.origins.insert(BindingOrigin::Context(
          self.result.binding_context(frame.context),
        ));
      }
      env.variables.values.insert(b"OPTARG".to_vec(), binding);
    }
    if let Some(arg) = args.get(1)
      && known_prefix
      && let Value::Literal(name) = &arg.value
    {
      if let Some(id) = self.write_argument(
        node,
        Some(arg),
        name,
        computed.as_ref().map_or_else(
          |issue| Value::Unknown(*issue),
          |result| Value::Literal(vec![result.option]),
        ),
        &mut env,
        frame,
      ) {
        self.record_attribute(id, frame.context, &env);
      }
    } else {
      self.invalidate(
        node,
        &mut env,
        frame,
        ResolutionIssue::DynamicName,
        argument_issue(args).unwrap_or(ValueIssue::Dynamic),
        [args
          .get(1)
          .filter(|_| known_prefix)
          .map_or(EffectTarget::Variables, |arg| {
            argument_target(arg, NameTarget::Variable)
          })],
      );
    }
    env.options_reset = false;
    env.option_cursor =
      computed.as_ref().ok().map(|result| result.cursor.clone());
    env.set_status(
      computed.map_or_else(Value::Unknown, |_| Value::Literal(b"0".to_vec())),
    );
    Flow::next(env)
  }

  fn argument_definition(
    &mut self,
    node: Node<'_>,
    arg: Option<&Argument<'_>>,
    name: &[u8],
    frame: Frame,
  ) -> Option<(SymbolId, OccurrenceId)> {
    let mut body = name.to_vec();
    body.push(b'=');
    let mut parts = arg.map_or_else(Vec::new, |arg| {
      arg
        .origins
        .iter()
        .filter_map(|part| {
          let end = part.bytes.end.min(name.len());
          if part.bytes.start >= end {
            return None;
          }
          let mut part = part.clone();
          if end < part.bytes.end
            && !matches!(part.source, OriginMapping::Linear(_))
          {
            part.source = OriginMapping::Generated;
          }
          part.bytes.end = end;
          Some(part)
        })
        .collect()
    });
    if arg.is_none() {
      parts.push(OriginPart {
        bytes: 0..name.len(),
        source: OriginMapping::Generated,
      });
    }
    parts.push(OriginPart {
      bytes: name.len()..body.len(),
      source: OriginMapping::Generated,
    });
    let snapshot = self.derive_body(
      DerivedOrigin {
        argument: frame.location(arg.map_or(node, |arg| arg.node)),
        parts,
      },
      &body,
    )?;
    self
      .workspace
      .document(snapshot.document)
      .and_then(|doc| doc.symbols().first())
      .map(|symbol| (symbol.id, symbol.definition))
  }

  fn write_argument(
    &mut self,
    node: Node<'_>,
    arg: Option<&Argument<'_>>,
    name: &[u8],
    value: Value,
    env: &mut Environment,
    frame: Frame,
  ) -> Option<OccurrenceId> {
    let Some((symbol, definition)) =
      self.argument_definition(node, arg, name, frame)
    else {
      self.invalidate(
        node,
        env,
        frame,
        ResolutionIssue::UndecodedSource,
        ValueIssue::Dynamic,
        [EffectTarget::Variable(name.to_vec())],
      );
      return None;
    };
    self.result.writes.insert((symbol, frame.context));
    if let Some(arg) = arg {
      self
        .result
        .argument_definitions
        .entry((UseSite::argument(&frame.location(arg.node)), frame.context))
        .or_default()
        .insert(symbol);
    }
    if env.readonly(name).is_some() {
      let resolution = env.variables.get(name).resolution.clone();
      if let Some(arg) = arg {
        self.record_argument_use(arg, resolution, env, frame);
      } else {
        self.record_use(
          UseSite::Syntax(definition),
          frame.context,
          resolution,
          Some(name),
          env,
        );
      }
    }
    env.assign(name, super::super::execution::definition(symbol), value);
    Some(definition)
  }
}

fn exit_status(args: &[Argument<'_>], previous: Value) -> Value {
  if args.is_empty() {
    previous
  } else {
    let issue = if args.len() == 1
      || args.iter().any(|arg| arg.count.boundary().is_err())
    {
      argument_issue(args).unwrap_or(ValueIssue::Unspecified)
    } else {
      ValueIssue::Unspecified
    };
    decimal_value::<u8>(args).map_or(Value::Unknown(issue), |status| {
      Value::Literal(status.to_string().into_bytes())
    })
  }
}

impl Resolver<'_> {
  pub(super) fn utility_output(
    &self,
    node: Node<'_>,
    name: &[u8],
    args: &[Argument<'_>],
    env: &Environment,
    frame: Frame,
  ) -> (Value, Option<OutputTrace>) {
    let mut arguments = vec![None; args.len()];
    let mut trace = (env.io.output.is_some() && !env.io.stdout_redirected)
      .then(OutputTrace::default);
    let value = output(
      name,
      args,
      env,
      self.result.request.limits.value_bytes,
      &mut |index, range, bytes| {
        if trace.is_none() {
          return;
        }
        let part = if let Some(index) = index {
          arguments[index]
            .get_or_insert_with(|| {
              Self::argument_output_trace(&args[index], frame)
            })
            .as_ref()
            .map(|trace| trace.slice(range))
        } else {
          parser::command_word(node).and_then(|command| {
            self.literal_output_trace(&frame.location(command), frame.context)
          })
        };
        if let Some(mut part) = part {
          part.replace_bytes(bytes);
          trace.as_mut().expect("output trace").append(part);
        } else {
          trace = None;
        }
      },
    );
    let trace = value
      .literal()
      .ok()
      .and_then(|bytes| trace?.matching(bytes));
    (value, trace)
  }
}

fn output(
  name: &[u8],
  args: &[Argument<'_>],
  env: &Environment,
  limit: usize,
  emit: &mut impl FnMut(Option<usize>, Range<usize>, &[u8]),
) -> Value {
  let known: Option<Vec<_>> = args.iter().map(Argument::literal).collect();
  let unknown = || {
    Value::Unknown(
      argument_issue(args).unwrap_or(ValueIssue::UnsupportedExpansion),
    )
  };
  match name {
    b":" | b"true" | b"false" | b"exit" | b"return" | b"shift" | b"break"
    | b"continue" | b"read" | b"getopts" | b"unset" | b"." => {
      Value::Literal(Vec::new())
    }
    b"exec" if args.is_empty() => Value::Literal(Vec::new()),
    b"export" | b"readonly"
      if args
        .iter()
        .any(|arg| matches!(arg.kind, Some(ArgumentKind::Variable { .. })))
        && args.iter().all(|arg| {
          matches!(arg.kind, Some(ArgumentKind::Variable { .. }))
            || arg.value == Value::Literal(b"--".to_vec())
        }) =>
    {
      Value::Literal(Vec::new())
    }
    b"set"
      if !args.is_empty()
        && (args.first().and_then(Argument::literal) == Some(b"--")
          || known.as_ref().is_some_and(|args| {
            !args
              .iter()
              .any(|arg| arg.starts_with(b"-") || arg.starts_with(b"+"))
          })) =>
    {
      Value::Literal(Vec::new())
    }
    b"echo" => {
      if args
        .first()
        .is_some_and(|arg| arg.value == Value::Literal(b"-n".to_vec()))
        || args
          .iter()
          .any(|arg| arg.value.literal().is_ok_and(|a| a.contains(&b'\\')))
      {
        return Value::Unknown(
          argument_issue(args).map_or(ValueIssue::Unspecified, |issue| {
            issue.combine(ValueIssue::Unspecified)
          }),
        );
      }
      let Some(args) = known else {
        return unknown();
      };
      let size = args
        .iter()
        .map(|a| a.len())
        .sum::<usize>()
        .saturating_add(args.len().max(1));
      if size > limit {
        return Value::Unknown(ValueIssue::SizeLimit);
      }
      let mut bytes = Vec::with_capacity(size);
      for (index, argument) in args.iter().enumerate() {
        if index != 0 {
          emit(None, 0..0, b" ");
          bytes.push(b' ');
        }
        emit(Some(index), 0..argument.len(), argument);
        bytes.extend_from_slice(argument);
      }
      emit(None, 0..0, b"\n");
      bytes.push(b'\n');
      Value::Literal(bytes)
    }
    b"printf" => formatted_output(args, limit, emit),
    b"pwd"
      if known.as_ref().is_some_and(|args| {
        args.is_empty() || args.as_slice() == [b"-L".as_slice()]
      }) && !env.directory_variables_modified =>
    {
      if let Some(issue) = env.cwd.issue() {
        return Value::Unknown(issue);
      }
      match env.parameter(b"PWD").0 {
        Value::Literal(mut pwd)
          if pwd.starts_with(b"/")
            && pwd
              .split(|byte| *byte == b'/')
              .all(|component| !matches!(component, b"." | b"..")) =>
        {
          pwd.push(b'\n');
          Value::Literal(pwd)
        }
        Value::Literal(_) => unknown(),
        pwd => Value::Unknown(pwd.issue().expect("unknown PWD")),
      }
    }
    _ => unknown(),
  }
}

fn formatted_output(
  args: &[Argument<'_>],
  limit: usize,
  emit: &mut impl FnMut(Option<usize>, Range<usize>, &[u8]),
) -> Value {
  let Some((format, operands)) = args.split_first() else {
    return Value::Unknown(ValueIssue::Unspecified);
  };
  if format.count.boundary().is_err() {
    return Value::Unknown(argument_issue(args).unwrap_or(ValueIssue::Dynamic));
  }
  let format = match format.value.literal() {
    Ok(format) => format,
    Err(issue) => return Value::Unknown(issue),
  };
  let unsupported = |output: &Value| {
    Value::Unknown(
      output
        .issue()
        .map_or(ValueIssue::UnsupportedExpansion, |issue| {
          issue.combine(ValueIssue::UnsupportedExpansion)
        }),
    )
  };
  if format.starts_with(b"-") {
    return Value::Unknown(ValueIssue::UnsupportedExpansion);
  }
  let boundaries_known =
    operands.iter().all(|arg| arg.count.boundary().is_ok());
  let mut output = Value::Literal(Vec::new());
  let mut operand = 0;
  loop {
    let before = operand;
    let mut bytes = format.iter().copied().enumerate();
    while let Some((start, byte)) = bytes.next() {
      let mut end = start + 1;
      let mut argument = 0;
      let value = match byte {
        b'%' => match bytes.next().map(|(index, byte)| {
          end = index + 1;
          byte
        }) {
          Some(b'%') => Value::Literal(vec![b'%']),
          Some(b's') => {
            let value = if boundaries_known {
              operands.get(operand).map_or_else(
                || Value::Literal(Vec::new()),
                |arg| arg.value.clone(),
              )
            } else {
              Value::Unknown(
                argument_issue(operands).unwrap_or(ValueIssue::Dynamic),
              )
            };
            argument = operand + 1;
            operand += 1;
            value
          }
          _ => return unsupported(&output),
        },
        b'\\' => Value::Literal(vec![match bytes.next().map(|(index, byte)| {
          end = index + 1;
          byte
        }) {
          Some(b'n') => b'\n',
          Some(b't') => b'\t',
          Some(b'r') => b'\r',
          Some(b'\\') => b'\\',
          _ => return unsupported(&output),
        }]),
        _ => Value::Literal(vec![byte]),
      };
      output.concat(&value, limit);
      if let (Value::Literal(_), Value::Literal(bytes)) = (&output, &value) {
        if argument == 0 {
          emit(Some(0), start..end, bytes);
        } else if !bytes.is_empty() {
          emit(Some(argument), 0..bytes.len(), bytes);
        }
      }
    }
    if !boundaries_known || operand == before || operand >= operands.len() {
      break;
    }
  }
  output
}

impl Resolver<'_> {
  fn read_values(
    &mut self,
    args: &[Argument<'_>],
    options: &ReadOptions,
    env: &mut Environment,
  ) -> Result<Vec<Value>, ValueIssue> {
    let operands = &args[options.operands..];
    if options.delimiter
      || operands.is_empty()
      || !operands
        .iter()
        .all(|arg| arg.literal().is_some_and(valid_name))
    {
      env.io.consume_input(None);
      return Err(argument_issue(args).unwrap_or(ValueIssue::Dynamic));
    }
    let ifs = env.ifs();
    let utf8 = self.utf8(env);
    if env.io.input_order_unknown {
      env.io.consume_input(None);
      return Ok(vec![
        Value::Unknown(ValueIssue::Unspecified);
        operands.len()
      ]);
    }
    let (input, mut offset) = env.io.input.clone()?;
    let word = input_line(
      &input,
      &mut offset,
      options.raw,
      ifs.empty() == Some(true),
      self.result.request.limits.value_bytes,
    );
    env.io.consume_input(Some(offset));
    let word = word?;
    let (fields, _) = word.split(&ifs, utf8, operands.len())?;
    if operands
      .iter()
      .filter_map(Argument::literal)
      .all(|name| env.readonly(name).is_none())
    {
      env.set_status(Value::Literal(b"0".to_vec()));
    }
    Ok(fields.iter().map(word::Field::value).collect())
  }
}

fn input_line(
  input: &[u8],
  offset: &mut usize,
  raw: bool,
  empty_ifs: bool,
  limit: usize,
) -> Result<word::Word, ValueIssue> {
  let mut word = word::Word::default();
  let mut line = Vec::new();
  while let Some(byte) = input.get(*offset).copied() {
    *offset += 1;
    if byte == b'\n' {
      word.append(
        word::Fragment {
          bytes: line,
          source: None,
          quoted: false,
          split: true,
        },
        limit,
      );
      return word.issue.map_or(Ok(word), Err);
    }
    if byte == b'\\' && !raw {
      let next = *input.get(*offset).ok_or(ValueIssue::Dynamic)?;
      *offset += 1;
      if next == b'\n' {
        continue;
      }
      word.append(
        word::Fragment {
          bytes: std::mem::take(&mut line),
          source: None,
          quoted: false,
          split: true,
        },
        limit,
      );
      word.append(
        word::Fragment {
          bytes: if empty_ifs {
            vec![b'\\', next]
          } else {
            vec![next]
          },
          source: None,
          quoted: true,
          split: false,
        },
        limit,
      );
    } else {
      line.push(byte);
    }
  }
  Err(ValueIssue::Dynamic)
}

struct OptionValues {
  option: u8,
  argument: Option<Value>,
  cursor: OptionCursor,
}

fn option_values(
  args: &[Argument<'_>],
  env: &Environment,
  functions: u32,
) -> Result<OptionValues, ValueIssue> {
  if args.len() < 2 || args[..2].iter().any(|arg| arg.count.boundary().is_err())
  {
    return Err(argument_issue(args).unwrap_or(ValueIssue::Dynamic));
  }
  let options = args[0].value.literal()?;
  let options = options.strip_prefix(b":").unwrap_or(options);
  let mut seen = BTreeSet::new();
  let mut colon_allowed = false;
  for byte in options {
    if *byte == b':' && colon_allowed {
      colon_allowed = false;
    } else if byte.is_ascii_alphanumeric() && seen.insert(byte) {
      colon_allowed = true;
    } else {
      return Err(ValueIssue::Dynamic);
    }
  }
  let target = args[1].value.literal()?;
  if !valid_name(target)
    || matches!(target, b"OPTIND" | b"OPTARG")
    || [target, b"OPTIND", b"OPTARG"]
      .iter()
      .any(|name| env.readonly(name).is_some())
  {
    return Err(ValueIssue::Dynamic);
  }
  let arguments = if args.len() == 2 {
    env.positional.clone()
  } else {
    positional_arguments(&args[2..])
  };
  let (mut index, mut offset) = if env.options_reset {
    (1, 1)
  } else if let Some(cursor) = &env.option_cursor {
    // POSIX leaves the results unspecified when a caller and its functions
    // both parse with getopts.
    if cursor.functions != functions {
      return Err(ValueIssue::Unspecified);
    }
    if cursor.arguments != arguments {
      return Err(ValueIssue::Dynamic);
    }
    (cursor.index, cursor.offset.max(1))
  } else {
    return Err(
      env
        .parameter(b"OPTIND")
        .0
        .issue()
        .unwrap_or(ValueIssue::Dynamic),
    );
  };
  let current =
    arguments.present(index.checked_sub(1).ok_or(ValueIssue::Dynamic)?)?;
  let current = current.literal()?;
  if !current.starts_with(b"-") || current == b"--" {
    return Err(ValueIssue::Dynamic);
  }
  let option = *current.get(offset).ok_or(ValueIssue::Dynamic)?;
  if !option.is_ascii_alphanumeric() {
    return Err(ValueIssue::Dynamic);
  }
  let position = options
    .iter()
    .position(|byte| *byte == option)
    .ok_or(ValueIssue::Dynamic)?;
  offset += 1;
  let argument = if options.get(position + 1) == Some(&b':') {
    let argument = if offset < current.len() {
      Value::Literal(current[offset..].to_vec())
    } else {
      index += 1;
      arguments.present(index - 1)?
    };
    index += 1;
    offset = 1;
    Some(argument)
  } else {
    if offset >= current.len() {
      index += 1;
      offset = 1;
    }
    None
  };
  Ok(OptionValues {
    option,
    argument,
    cursor: OptionCursor {
      arguments,
      index,
      offset,
      functions,
    },
  })
}
