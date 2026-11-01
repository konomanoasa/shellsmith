use super::super::deferred::OutputTrace;
use super::super::execution::{
  ALWAYS, Binding, Environment, Flow, NEVER, OptionCursor, StateKey, Transfer,
  ValueId, Values, alternative,
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
  Access, Alternative, ArgumentKind, DerivedOrigin, EffectTarget, Error,
  InputQuery, InputResult, OccurrenceId, OriginMapping, OriginPart,
  ResolutionIssue, SymbolId, UseSite, Value, ValueIssue,
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
    if (name == b"shift"
      && decimal_value::<usize>(&args) != Some(0)
      && env
        .positional
        .fields
        .iter()
        .any(|(_, count)| count.boundary().is_err()))
      || (name == b"getopts" && args.len() == 2)
    {
      let names = BTreeSet::from([StateKey::Positionals]);
      if env.alternatives.depends(&names) {
        let name = name.to_vec();
        return self.each_alternative(env, &names, move |resolver, env| {
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
      let (output, trace, expression) =
        self.utility_output(node, name, &args, &env, frame);
      self.emit(&mut env, output, trace, expression);
      env.io.may_write_stderr();
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
      b"printf" if args.is_empty() => {
        env.set_status(Value::Literal(b"2".to_vec()));
        Task::Done(Flow::next(env))
      }
      b"echo" | b"printf" | b"pwd" | b"test" | b"[" | b"umask" | b"wait"
      | b"jobs" | b"fg" | b"bg" | b"hash" | b"type" | b"ulimit" | b"times"
      | b"kill" | b"exec" => Task::Done(Flow::next(env)),
      b"exit" | b"return" => {
        let status = exit_status(&args, previous_status, name == b"exit");
        env.set_status(status);
        Task::Done(if name == b"exit" {
          Flow::next(env)
        } else if frame.returnable {
          Flow::single(Transfer::Return, env)
        } else {
          Flow::next(env)
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
        env.set_positionals(arguments.as_slice());
        return Flow::next(env);
      }
      if !matches!(option.first(), Some(b'-' | b'+')) {
        let rest = std::iter::once(argument.clone())
          .chain(arguments.cloned())
          .collect::<Vec<_>>();
        env.set_positionals(&rest);
        return Flow::next(env);
      }
      env.special.remove(b"-".as_slice());
      let enabled = Some(option[0] == b'-');
      let mut letters = option[1..].iter();
      if letters.as_slice().is_empty() {
        return self.unknown_options(node, env, frame, ValueIssue::Dynamic);
      }
      while let Some(letter) = letters.next() {
        match letter {
          b'u' => env.nounset = enabled,
          b'f' => env.noglob = enabled,
          b'a' => env.allexport = enabled,
          b'b' | b'C' | b'e' | b'h' | b'm' | b'n' | b'v' | b'x' => {}
          b'o' => {
            let name = if letters.as_slice().is_empty() {
              let Some(name) = arguments.next() else {
                return Flow::next(env);
              };
              name.value.clone()
            } else {
              Value::Literal(letters.as_slice().to_vec())
            };
            let Value::Literal(name) = name else {
              return self.unknown_options(
                node,
                env,
                frame,
                name.issue().expect("unknown option name"),
              );
            };
            match name.as_slice() {
              b"nounset" => env.nounset = enabled,
              b"noglob" => env.noglob = enabled,
              b"allexport" => env.allexport = enabled,
              b"pipefail" => env.pipefail = enabled,
              b"errexit" | b"notify" | b"noclobber" | b"monitor"
              | b"noexec" | b"nolog" | b"verbose" | b"xtrace"
              | b"ignoreeof" | b"vi" => {}
              _ => {
                return self.unknown_options(
                  node,
                  env,
                  frame,
                  ValueIssue::Dynamic,
                );
              }
            }
            break;
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
      let attributes = if utility == b"readonly" {
        EffectTarget::Attributes
      } else {
        EffectTarget::Exports
      };
      self.invalidate(
        node,
        &mut env,
        frame,
        ResolutionIssue::UnknownEffect,
        argument_issue(args).unwrap_or(ValueIssue::Dynamic),
        [EffectTarget::Variables, attributes],
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
        self.declare_attributes(
          utility,
          argument,
          &name,
          definition.is_some(),
          &mut env,
          frame,
        );
      } else {
        let target = argument_target(argument, NameTarget::Declaration);
        let attribute = attribute_target(&target, utility == b"export");
        self.invalidate(
          node,
          &mut env,
          frame,
          ResolutionIssue::DynamicName,
          argument.value.issue().unwrap_or(ValueIssue::Dynamic),
          [target, attribute],
        );
      }
      if let Some(id) = definition {
        if let Some(symbol) = self.index.defined_symbol(id) {
          self.record_argument_definition(argument, symbol, frame);
        }
        self.record_attribute(id, frame.context, &env);
      }
    }
    Flow::next(env)
  }

  fn declare_attributes(
    &mut self,
    utility: &[u8],
    argument: &Argument<'_>,
    name: &[u8],
    assigned: bool,
    env: &mut Environment,
    frame: Frame,
  ) {
    if utility == b"export" {
      let readonly = assigned.then(|| env.readonly(name)).flatten();
      if readonly != Some(true) {
        env.exports.mark(name, true, readonly.is_none());
      }
    }
    if matches!(
      argument.kind,
      Some(ArgumentKind::Variable {
        access: Access::NameReference,
        ..
      })
    ) {
      self.record_argument_use(
        argument,
        env.variables.get(name).resolution.clone(),
        env,
        frame,
      );
    }
    if utility == b"readonly" {
      env.readonly.insert(name.to_vec(), true);
    }
    self.record_site_attributes(
      UseSite::argument(&frame.location(argument.node)),
      frame.context,
      name,
      env,
    );
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
      let mut after = Binding::new(
        alternative(Alternative::Unset)
          .in_context(self.result.binding_context(frame.context)),
      );
      if conditional_readonly {
        after.join(env.variables.get(name));
        after
          .resolution
          .issues
          .insert(ResolutionIssue::UnknownEffect);
      }
      if functions {
        env.replace_function(name, after);
      } else {
        env.variable_changed(name, None);
        env.replace_variable(name, after);
        env.exports.mark(name, false, !conditional_readonly);
      }
      if function_unset {
        let mut function = env.functions.get(name).clone();
        function.join(&Binding::new(
          alternative(Alternative::Unset)
            .in_context(self.result.binding_context(frame.context)),
        ));
        env.replace_function(name, function);
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
    {
      let depth = depth.min(frame.loops);
      let transfer = if name == b"break" {
        Transfer::Break(depth)
      } else {
        Transfer::Continue(depth)
      };
      Flow::single(transfer, env)
    } else if depth.is_some() {
      Flow::next(env)
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
      match self.input(&InputQuery::Directory {
        cwd: cwd.clone(),
        path,
        physical,
      }) {
        InputResult::Path(curpath) => {
          let previous = env.parameter(b"PWD").0;
          let directory = simplified(&curpath);
          let pwd = Value::Literal(directory.clone());
          env.cwd = Value::Literal(directory);
          for (name, value) in [(b"OLDPWD".as_slice(), previous), (b"PWD", pwd)]
          {
            env.export_assignment(name);
            env.replace_variable(
              name,
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
    let mut shared = None;
    if let Some(count) = count
      && count <= env.positional.fields.len()
      && env
        .positional
        .fields
        .iter()
        .all(|(_, count)| count.boundary().is_ok())
      && env
        .alternatives
        .depends(&BTreeSet::from([StateKey::Positionals]))
    {
      let fields = (count..env.positional.fields.len())
        .map(|index| {
          env
            .alternatives
            .positional(&env, index, self.cancelled)
            .map(|value| (value.expect("fixed field"), word::FieldCount::One))
        })
        .collect::<Result<Vec<_>, _>>();
      match fields {
        Ok(fields) => shared = Some(fields),
        Err(error) => {
          self.error = Some(error);
          return Flow::default();
        }
      }
    }
    if count != Some(0) {
      env.alternatives.forget_positionals();
    }
    if count.is_some_and(|count| env.positional.shift(count)) {
      if let Some(fields) = shared {
        env.alternatives.store_positionals(fields);
      }
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
        self.write_argument(
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
        );
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
      .and_then(|_| option_values(args, &env));
    let known_prefix = operands.is_some()
      && args.iter().take(2).all(|arg| arg.count.boundary().is_ok());
    if known_prefix && args.len() < 2 {
      self.gap(node, frame, ResolutionIssue::DynamicName);
      return Flow::next(env);
    }
    let target = args.get(1).filter(|_| known_prefix);
    let mut flow = Flow::default();
    for (index, (name, arg)) in [
      (Some(b"OPTARG".as_slice()), None),
      (Some(b"OPTIND".as_slice()), None),
      (target.and_then(Argument::literal), target),
    ]
    .into_iter()
    .enumerate()
    {
      let failure = name.map_or(Some(false), |name| {
        if valid_name(name) {
          env.readonly(name)
        } else {
          Some(true)
        }
      });
      if failure.is_some() {
        let mut failed = env.clone();
        failed.options_reset = true;
        failed.option_cursor = None;
        failed.set_status(Value::Literal(b"2".to_vec()));
        flow.add(Transfer::Next, failed);
      }
      if let Some(name) = name.filter(|name| valid_name(name)) {
        let previous = env.variables.get(name).clone();
        self.write_argument(
          node,
          arg,
          name,
          computed.as_ref().map_or_else(
            |issue| Value::Unknown(*issue),
            |result| match index {
              0 => result
                .argument
                .clone()
                .unwrap_or(Value::Literal(Vec::new())),
              1 => Value::Literal(
                (result.cursor.index + usize::from(result.cursor.offset > 1))
                  .to_string()
                  .into_bytes(),
              ),
              _ => Value::Literal(vec![result.option]),
            },
          ),
          &mut env,
          frame,
        );
        if index == 0 {
          self.unset_optarg(&mut env, &previous, &computed, frame);
        }
      } else if name.is_none() {
        self.invalidate(
          node,
          &mut env,
          frame,
          ResolutionIssue::DynamicName,
          argument_issue(args).unwrap_or(ValueIssue::Dynamic),
          [target.map_or(EffectTarget::Variables, |arg| {
            argument_target(arg, NameTarget::Variable)
          })],
        );
      }
      if failure == Some(true) {
        return flow;
      }
    }
    env.options_reset = false;
    env.option_cursor =
      computed.as_ref().ok().map(|result| result.cursor.clone());
    env.set_status(computed.map_or_else(Value::Unknown, |result| {
      Value::Literal(result.status.to_string().into_bytes())
    }));
    flow.add(Transfer::Next, env);
    flow
  }

  fn unset_optarg(
    &self,
    env: &mut Environment,
    previous: &Binding,
    computed: &Result<OptionValues, ValueIssue>,
    frame: Frame,
  ) {
    if env.readonly(b"OPTARG") == Some(true) {
      return;
    }
    let mut binding = env.variables.get(b"OPTARG").clone();
    if computed
      .as_ref()
      .is_ok_and(|result| result.argument.is_none())
    {
      binding = Binding::new(
        alternative(Alternative::Unset)
          .in_context(self.result.binding_context(frame.context)),
      );
      if env.readonly(b"OPTARG").is_some() {
        binding.join(previous);
      }
      env
        .exports
        .mark(b"OPTARG", false, env.readonly(b"OPTARG").is_none());
    } else if computed.is_err() {
      binding.resolution.alternatives.insert(Alternative::Unset);
      binding.resolution.origins.insert(BindingOrigin::Context(
        self.result.binding_context(frame.context),
      ));
      env.exports.mark(b"OPTARG", false, false);
    }
    env.replace_variable(b"OPTARG", binding);
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
      self.record_argument_definition(arg, symbol, frame);
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
    if !arg.is_some_and(|arg| {
      matches!(
        arg.kind,
        Some(ArgumentKind::Variable {
          declaration: true,
          ..
        })
      )
    }) {
      self.record_attribute(definition, frame.context, env);
      if let Some(arg) = arg {
        self.record_site_attributes(
          UseSite::argument(&frame.location(arg.node)),
          frame.context,
          name,
          env,
        );
      }
    }
    Some(definition)
  }

  fn record_argument_definition(
    &mut self,
    arg: &Argument<'_>,
    symbol: SymbolId,
    frame: Frame,
  ) {
    self
      .result
      .argument_definitions
      .entry((UseSite::argument(&frame.location(arg.node)), frame.context))
      .or_default()
      .insert(symbol);
  }
}

fn exit_status(args: &[Argument<'_>], previous: Value, process: bool) -> Value {
  let value = match args.first() {
    None => previous,
    Some(arg) => {
      let bytes = match arg.count.boundary().and_then(|()| arg.value.literal())
      {
        Ok(bytes) => bytes,
        Err(issue) => return Value::Unknown(issue),
      };
      let status = std::str::from_utf8(bytes)
        .ok()
        .map(|number| {
          number.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\u{b}')
        })
        .and_then(|number| number.parse::<i32>().ok())
        .filter(|status| *status >= 0)
        .unwrap_or(2);
      Value::Literal(status.to_string().into_bytes())
    }
  };
  if process {
    process_status(value)
  } else {
    value
  }
}

pub(in crate::interface::application::engine::analysis) fn process_status(
  value: Value,
) -> Value {
  match &value {
    Value::Literal(bytes) => std::str::from_utf8(bytes)
      .ok()
      .and_then(|number| number.parse::<i64>().ok())
      .map_or(value.clone(), |status| {
        Value::Literal(status.rem_euclid(256).to_string().into_bytes())
      }),
    _ => value,
  }
}

impl Resolver<'_> {
  pub(super) fn utility_output(
    &mut self,
    node: Node<'_>,
    name: &[u8],
    args: &[Argument<'_>],
    env: &Environment,
    frame: Frame,
  ) -> (Value, Option<OutputTrace>, Option<ValueId>) {
    if !args.iter().any(|arg| arg.expression.is_some()) {
      let (value, trace) = self.concrete_output(node, name, args, env, frame);
      return (value, trace, None);
    }
    let result = (|| {
      let mut values = env.alternatives.expressions();
      let roots = args
        .iter()
        .map(|arg| {
          arg
            .expression
            .unwrap_or_else(|| values.literal(Arc::new(arg.value.clone())))
        })
        .collect::<Vec<_>>();
      let limit = self.result.request.limits.value_bytes;
      let shared = if name == b"echo"
        && args.iter().all(|arg| arg.count.boundary().is_ok())
      {
        shared_echo(&mut values, &roots, limit, self.cancelled)?
      } else {
        None
      };
      let id = if let Some(id) = shared {
        id
      } else {
        let mut result = values.literal(Arc::new(Value::Literal(Vec::new())));
        for (input, guard) in
          values.combinations(&roots, ALWAYS, self.cancelled)?
        {
          let mut args = args.to_vec();
          for (arg, value) in args.iter_mut().zip(input) {
            arg.value = value.as_ref().clone();
            arg.expression = None;
          }
          let value = output(name, &args, env, limit, &mut |_, _, _| {});
          let id = values.literal(Arc::new(value));
          result = values.select(guard, id, result);
        }
        result
      };
      Ok::<_, Error>((
        values.projected(id, self.cancelled)?.as_ref().clone(),
        id,
      ))
    })();
    match result {
      Ok((value, id)) => {
        let (_, trace) = self.concrete_output(node, name, args, env, frame);
        (value, trace, Some(id))
      }
      Err(error) => {
        self.error = Some(error);
        (Value::Unknown(ValueIssue::Dynamic), None, None)
      }
    }
  }

  pub(super) fn emit(
    &mut self,
    env: &mut Environment,
    value: Value,
    trace: Option<OutputTrace>,
    expression: Option<ValueId>,
  ) {
    if env.io.stdout_redirected || env.io.output.is_none() {
      return;
    }
    let previous = env.io.output.as_ref().and_then(|output| output.expression);
    let limit = self.result.request.limits.value_bytes;
    if expression.is_none() && previous.is_none() {
      env.emit(value, trace, limit);
      return;
    }
    let before = env.alternatives.output(env);
    let next = expression.unwrap_or_else(|| {
      env
        .alternatives
        .expressions()
        .literal(Arc::new(value.clone()))
    });
    let result = (|| {
      let mut values = env.alternatives.expressions();
      let Some(id) = values.concat(before, next, limit, self.cancelled)? else {
        return Ok(None);
      };
      Ok::<_, Error>(Some((
        id,
        values.projected(id, self.cancelled)?.as_ref().clone(),
      )))
    })();
    env.emit(value, trace, limit);
    match result {
      Ok(Some((id, value))) => {
        env
          .io
          .output
          .as_mut()
          .expect("captured output")
          .set_expression(id, value, limit);
      }
      Ok(None) => {}
      Err(error) => self.error = Some(error),
    }
  }

  fn concrete_output(
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

fn shared_echo(
  values: &mut Values,
  args: &[ValueId],
  limit: usize,
  cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Option<ValueId>, Error> {
  let maximum = args.iter().try_fold(args.len().max(1), |size, id| {
    size.checked_add(values.maximum(*id)?)
  });
  if maximum.is_none_or(|size| size > limit) {
    return Ok(None);
  }
  let mut issue = NEVER;
  let mut result = values.literal(Arc::new(Value::Literal(Vec::new())));
  let space = values.literal(Arc::new(Value::Literal(b" ".to_vec())));
  for (index, arg) in args.iter().enumerate() {
    if index == 0 {
      issue = values.equals(*arg, b"-n", cancelled)?;
    } else {
      result = values
        .concat(result, space, limit, cancelled)?
        .expect("finite echo");
    }
    let escaped = values.contains(*arg, b'\\', cancelled)?;
    issue = values.conditions.or(issue, escaped);
    result = values
      .concat(result, *arg, limit, cancelled)?
      .expect("finite echo");
  }
  let newline = values.literal(Arc::new(Value::Literal(b"\n".to_vec())));
  result = values
    .concat(result, newline, limit, cancelled)?
    .expect("finite echo");
  Ok((issue == NEVER).then_some(result))
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
      let Some(args) = known else {
        return unknown();
      };
      echo_output(&args, limit, emit)
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

fn echo_output(
  args: &[&[u8]],
  limit: usize,
  emit: &mut impl FnMut(Option<usize>, Range<usize>, &[u8]),
) -> Value {
  let first = usize::from(args.first() == Some(&b"-n".as_slice()));
  let mut output = Value::Literal(Vec::new());
  for (index, argument) in args.iter().enumerate().skip(first) {
    if index > first {
      output.concat(&Value::Literal(b" ".to_vec()), limit);
      emit(None, 0..0, b" ");
    }
    let mut offset = 0;
    while offset < argument.len() {
      let start = offset;
      if argument[offset] != b'\\' {
        offset += argument[offset..]
          .iter()
          .position(|byte| *byte == b'\\')
          .unwrap_or(argument.len() - offset);
        let bytes = &argument[start..offset];
        output.concat(&Value::Literal(bytes.to_vec()), limit);
        emit(Some(index), start..offset, bytes);
        continue;
      }
      let mut byte = argument[offset];
      offset += 1;
      if byte == b'\\'
        && let Some(&escaped) = argument.get(offset)
      {
        offset += 1;
        byte = match escaped {
          b'a' => 7,
          b'b' => 8,
          b'e' => 27,
          b'f' => 12,
          b'n' => b'\n',
          b'r' => b'\r',
          b't' => b'\t',
          b'v' => 11,
          b'\\' => b'\\',
          b'c' => return output,
          b'0'..=b'7' | b'x' => {
            let radix: u8 = if escaped == b'x' { 16 } else { 8 };
            let maximum = if escaped == b'0' { 3 } else { 2 };
            let digits = offset;
            let mut number = if escaped == b'x' { 0 } else { escaped - b'0' };
            while offset < argument.len() && offset - digits < maximum {
              let Some(digit) =
                char::from(argument[offset]).to_digit(u32::from(radix))
              else {
                break;
              };
              number = number
                .wrapping_mul(radix)
                .wrapping_add(u8::try_from(digit).expect("byte digit"));
              offset += 1;
            }
            number
          }
          _ => {
            offset -= 1;
            b'\\'
          }
        };
      }
      output.concat(&Value::Literal(vec![byte]), limit);
      emit(Some(index), start..offset, &[byte]);
    }
  }
  if first == 0 {
    output.concat(&Value::Literal(b"\n".to_vec()), limit);
    emit(None, 0..0, b"\n");
  }
  output
}

fn formatted_output(
  args: &[Argument<'_>],
  limit: usize,
  emit: &mut impl FnMut(Option<usize>, Range<usize>, &[u8]),
) -> Value {
  let Some((format, operands)) = args.split_first() else {
    return Value::Literal(Vec::new());
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
    let (input, mut offset) = env.io.input.clone()?;
    let word = input_line(
      &input,
      &mut offset,
      options.raw,
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
          bytes: vec![next],
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
  status: u8,
}

fn option_values(
  args: &[Argument<'_>],
  env: &Environment,
) -> Result<OptionValues, ValueIssue> {
  if args.len() < 2 || args[..2].iter().any(|arg| arg.count.boundary().is_err())
  {
    return Err(argument_issue(args).unwrap_or(ValueIssue::Dynamic));
  }
  let options = args[0].value.literal()?;
  let silent = options.starts_with(b":");
  let options = options.strip_prefix(b":").unwrap_or(options);
  let arguments = if args.len() == 2 {
    env.positional.clone()
  } else {
    positional_arguments(&args[2..])
  };
  let (mut index, mut offset) = if env.options_reset {
    (1, 1)
  } else if let Some(cursor) = &env.option_cursor {
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
  let (current, absent) =
    arguments.parameter(index.checked_sub(1).ok_or(ValueIssue::Dynamic)?);
  let current = current.literal()?;
  if absent.is_none() {
    return Err(ValueIssue::Dynamic);
  }
  if absent == Some(true)
    || !current.starts_with(b"-")
    || matches!(current, b"-" | b"--")
  {
    if current == b"--" {
      index += 1;
    }
    return Ok(OptionValues {
      option: b'?',
      argument: None,
      cursor: OptionCursor { index, offset: 1 },
      status: 1,
    });
  }
  let option = *current.get(offset).ok_or(ValueIssue::Dynamic)?;
  let position = options
    .iter()
    .position(|byte| *byte == option && option != b':');
  offset += 1;
  let mut result = option;
  let mut argument = None;
  if let Some(position) = position {
    if options.get(position + 1) == Some(&b':') {
      if offset < current.len() {
        argument = Some(Value::Literal(current[offset..].to_vec()));
      } else {
        index += 1;
        let (value, absent) = arguments.parameter(index - 1);
        match absent {
          Some(false) => argument = Some(value),
          Some(true) => {
            result = if silent { b':' } else { b'?' };
            argument = silent.then(|| Value::Literal(vec![option]));
            index -= 1;
          }
          None => return Err(value.issue().unwrap_or(ValueIssue::Dynamic)),
        }
      }
      offset = current.len();
    }
  } else {
    result = b'?';
    argument = silent.then(|| Value::Literal(vec![option]));
  }
  if offset >= current.len() {
    index += 1;
    offset = 1;
  }
  Ok(OptionValues {
    option: result,
    argument,
    cursor: OptionCursor { index, offset },
    status: 0,
  })
}
