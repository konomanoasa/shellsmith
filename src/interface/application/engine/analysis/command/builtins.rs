use super::super::execution::{
  Binding, Environment, Flow, Transfer, alternative,
};
use super::super::expansion::Argument;
use super::super::{Frame, Resolver, Task};
use super::arguments::{
  NameTarget, argument_target, classify_arguments, decimal_operand,
  declaration_operand, read_options, trap_action, unset_options,
};
use crate::interface::application::engine::valid_name;
use crate::interface::application::engine::word;
use crate::interface::application::engine::{
  Access, Alternative, ArgumentKind, EffectTarget, InputQuery, InputResult,
  OccurrenceId, ResolutionIssue, SymbolId, UseSite, Value, ValueIssue,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use tree_sitter::Node;

pub(super) fn preserves_input(name: &[u8]) -> bool {
  matches!(
    name,
    b":"
      | b"true"
      | b"false"
      | b"echo"
      | b"printf"
      | b"pwd"
      | b"test"
      | b"["
      | b"getopts"
  )
}

impl<'a> Resolver<'a> {
  pub(super) fn utility(
    &mut self,
    node: Node<'a>,
    name: &[u8],
    mut args: Vec<Argument<'a>>,
    mut env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    if name == b"command" {
      return self.forward(node, args.into_iter(), env, frame);
    }
    let source = self
      .workspace
      .document(frame.snapshot.document)
      .expect("current document")
      .source
      .as_bytes();
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
      let output =
        output(name, &args, &env, self.result.request.limits.value_bytes);
      env.emit(output, self.result.request.limits.value_bytes);
      if env.io.stderr_captured
        && let Some(output) = &mut env.io.output
      {
        output.fail(ValueIssue::Dynamic);
      }
    }
    let previous_status = env.parameter(b"?").0;
    env
      .special
      .insert(b"?".to_vec(), Value::Unknown(ValueIssue::Dynamic));
    let task = match name {
      b":" | b"true" | b"false" => {
        env.special.insert(
          b"?".to_vec(),
          Value::Literal(if name == b"false" { b"1" } else { b"0" }.to_vec()),
        );
        Task::Done(Flow::next(env))
      }
      b"export" | b"readonly" => {
        Task::Done(self.declaration(node, name, &args, env, frame))
      }
      b"unset" => Task::Done(self.unset(node, &args, env, frame)),
      b"set" => Task::Done(self.set_options(node, &args, env, frame)),
      b"cd" => Task::Done(self.cd(node, &args, env, frame)),
      b"shift" => Task::Done(self.shift(node, &args, env, frame)),
      b"." => self.source(node, &args, env, frame),
      b"trap" => self.trap(node, &args, env, frame),
      b"eval" | b"alias" | b"unalias" => {
        self.gap(node, frame, ResolutionIssue::ExcludedEffect);
        Task::Done(Flow::next(env))
      }
      b"read" => Task::Done(self.read_arguments(node, &args, env, frame)),
      b"getopts" => Task::Done(self.getopts(node, &args, env, frame)),
      b"fc" | b"newgrp" => Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::UnknownEffect,
      )),
      b"echo" | b"printf" | b"pwd" | b"test" | b"[" | b"umask" | b"wait"
      | b"jobs" | b"fg" | b"bg" | b"hash" | b"type" | b"ulimit" | b"times"
      | b"kill" | b"exec" => Task::Done(Flow::next(env)),
      b"exit" | b"return" => {
        let status = exit_status(&args, previous_status);
        env.special.insert(b"?".to_vec(), status);
        Task::Done(if name == b"exit" {
          Flow::next(env)
        } else if frame.returnable {
          Flow(std::collections::BTreeMap::from([(Transfer::Return, env)]))
        } else {
          self.opaque(node, env, frame, ResolutionIssue::UnsupportedControl)
        })
      }
      b"break" | b"continue" => {
        Task::Done(self.control(node, name, &args, env, frame))
      }
      _ => Task::Done(self.unknown(
        node,
        env,
        frame,
        ResolutionIssue::UnknownEffect,
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
      let Value::Literal(option) = argument.value.clone() else {
        return self.unknown_options(
          node,
          env,
          frame,
          ResolutionIssue::UnknownEffect,
        );
      };
      if option == b"--" {
        env.alternatives.forget_positionals();
        env.positional =
          super::super::expansion::positional_arguments(arguments.as_slice());
        return Flow::next(env);
      }
      if !matches!(option.first(), Some(b'-' | b'+')) {
        let rest = std::iter::once(argument.clone())
          .chain(arguments.cloned())
          .collect::<Vec<_>>();
        env.alternatives.forget_positionals();
        env.positional = super::super::expansion::positional_arguments(&rest);
        return Flow::next(env);
      }
      env.special.remove(b"-".as_slice());
      let mut letters = option[1..].iter();
      if letters.as_slice().is_empty() {
        return self.unknown_options(
          node,
          env,
          frame,
          ResolutionIssue::UnknownEffect,
        );
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
                  | b"hashall"
                  | b"monitor"
                  | b"noexec"
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
              ResolutionIssue::UnknownEffect,
            );
          }
          _ => {
            return self.unknown_options(
              node,
              env,
              frame,
              ResolutionIssue::UnknownEffect,
            );
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
    for argument in args {
      let variable = argument.node.child_by_field_name("name");
      let mut definition =
        variable.and_then(|variable| self.site(variable, frame));
      let name = if let Some(variable) = variable {
        self.write(variable, &mut env, frame, false, argument.value.clone());
        self
          .site(variable, frame)
          .and_then(|id| self.workspace.occurrence(id))
          .map(|site| site.name.clone())
      } else if argument.boundary_known {
        if matches!(&argument.value, Value::Literal(bytes) if matches!(bytes.as_slice(), b"--" | b"-p"))
        {
          continue;
        }
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
        self.invalidate(
          node,
          &mut env,
          frame,
          ResolutionIssue::DynamicName,
          [argument_target(argument, NameTarget::Declaration)],
        );
        if utility == b"readonly" {
          self.invalidate(
            node,
            &mut env,
            frame,
            ResolutionIssue::UnknownEffect,
            [crate::interface::application::engine::EffectTarget::Attributes],
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
    use crate::interface::application::engine::EffectTarget;
    let Some((index, functions, explicit)) = unset_options(args) else {
      self.invalidate(
        node,
        &mut env,
        frame,
        ResolutionIssue::DynamicName,
        [EffectTarget::Variables, EffectTarget::Functions],
      );
      return Flow::next(env);
    };
    for arg in &args[index..] {
      let name = match &arg.value {
        Value::Literal(name) if arg.boundary_known => Some(name),
        _ => None,
      };
      let Some(name) = name else {
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
      let mut after = Binding::new(alternative(Alternative::Unset));
      if conditional_readonly {
        after.join(&before);
        after
          .resolution
          .issues
          .insert(ResolutionIssue::UnknownEffect);
      }
      bindings.values.insert(name.clone(), after);
      if function_unset {
        env.alternatives.forget_function(name);
        let mut function = env.functions.get(name).clone();
        function.join(&Binding::new(alternative(Alternative::Unset)));
        env.functions.values.insert(name.clone(), function);
      }
      if !functions && matches!(name.as_slice(), b"PWD" | b"OLDPWD") {
        env.directory_variables_modified = true;
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
      decimal_operand(args)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| text.parse::<u32>().ok())
        .filter(|depth| *depth > 0)
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
      Flow(std::collections::BTreeMap::from([(transfer, env)]))
    } else {
      self.opaque(node, env, frame, ResolutionIssue::UnsupportedControl)
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
    if let Some(command) = node
      .child_by_field_name("name")
      .or_else(|| node.child_by_field_name("word"))
      .and_then(|name| self.site(name, frame))
    {
      let action = trap_action(args).map(|index| {
        index.map(|index| {
          let action = &args[index];
          (
            crate::interface::application::engine::DerivedOrigin {
              registration: command,
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
    let Some((operand, physical)) = cd_arguments(args, &env) else {
      return self.unknown_directory(node, env, frame);
    };
    if env.directory_variables_modified
      || env.readonly(b"PWD").is_some()
      || env.readonly(b"OLDPWD").is_some()
    {
      return self.unknown_directory(node, env, frame);
    }
    let cwd = if operand.starts_with(b"/") {
      Vec::new()
    } else if let Value::Literal(cwd) = &env.cwd {
      cwd.clone()
    } else {
      return self.unknown_directory(node, env, frame);
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
            if !path.is_empty() {
              path.push(b'/');
            }
            path.extend(&operand);
            paths.push(path);
          }
        }
        _ => return self.unknown_directory(node, env, frame),
      }
    }
    paths.push(operand);
    for path in paths {
      match self.input(InputQuery::Directory {
        cwd: cwd.clone(),
        path,
        physical,
      }) {
        InputResult::Path(path) => {
          let previous = env.cwd.clone();
          env.cwd = Value::Literal(path);
          for (name, value) in
            [(b"OLDPWD".as_slice(), previous), (b"PWD", env.cwd.clone())]
          {
            env.alternatives.forget(name);
            env.variables.values.insert(
              name.to_vec(),
              Binding {
                resolution: alternative(Alternative::External),
                value: Arc::new(value),
              },
            );
          }
          env
            .special
            .insert(b"?".to_vec(), Value::Literal(b"0".to_vec()));
          return Flow::next(env);
        }
        InputResult::Missing => {}
        _ => return self.unknown_directory(node, env, frame),
      }
    }
    env
      .special
      .insert(b"?".to_vec(), Value::Unknown(ValueIssue::Dynamic));
    Flow::next(env)
  }

  fn unknown_directory(
    &mut self,
    node: Node<'_>,
    mut env: Environment,
    frame: Frame,
  ) -> Flow {
    use crate::interface::application::engine::EffectTarget;
    self.invalidate(
      node,
      &mut env,
      frame,
      ResolutionIssue::UnknownEffect,
      [
        EffectTarget::Directory,
        EffectTarget::Variable(b"PWD".to_vec()),
        EffectTarget::Variable(b"OLDPWD".to_vec()),
      ],
    );
    Flow::next(env)
  }
}

fn cd_arguments(
  args: &[Argument<'_>],
  env: &Environment,
) -> Option<(Vec<u8>, bool)> {
  let mut physical = false;
  let mut index = 0;
  while let Some(arg) = args.get(index) {
    let Value::Literal(value) = &arg.value else {
      return None;
    };
    if !arg.boundary_known {
      return None;
    }
    if value == b"--" {
      index += 1;
      break;
    }
    if !value.starts_with(b"-") || value == b"-" {
      break;
    }
    for flag in value.get(1..)? {
      match flag {
        b'P' => physical = true,
        b'L' => physical = false,
        b'e' => {}
        _ => return None,
      }
    }
    index += 1;
  }
  let operand = match &args[index..] {
    [] => match env.parameter(b"HOME") {
      (Value::Literal(home), Some(false)) if !home.is_empty() => home,
      _ => return None,
    },
    [arg] => match &arg.value {
      Value::Literal(value) if value == b"-" => {
        match env.parameter(b"OLDPWD").0 {
          Value::Literal(path) if path.starts_with(b"/") => path,
          _ => return None,
        }
      }
      Value::Literal(value) if !value.is_empty() => value.clone(),
      _ => return None,
    },
    _ => return None,
  };
  Some((operand, physical))
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
      decimal_operand(args)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| text.parse::<usize>().ok())
    };
    env.alternatives.forget_positionals();
    match (&mut env.positional, count) {
      (Some(args), Some(count)) if count <= args.len() => {
        args.drain(..count);
      }
      (Some(_), Some(_)) => {
        env.alternatives.forget_positionals();
        env.positional = None;
      }
      _ => self.invalidate(
        node,
        &mut env,
        frame,
        ResolutionIssue::UnknownEffect,
        [crate::interface::application::engine::EffectTarget::Positionals],
      ),
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
    issue: ResolutionIssue,
  ) -> Flow {
    use crate::interface::application::engine::EffectTarget;
    self.invalidate(
      node,
      &mut env,
      frame,
      issue,
      [EffectTarget::Options, EffectTarget::Positionals],
    );
    Flow::next(env)
  }

  fn read_arguments(
    &mut self,
    node: Node<'_>,
    args: &[Argument<'_>],
    mut env: Environment,
    frame: Frame,
  ) -> Flow {
    let index = read_options(args);
    let values = self.read_values(args, index, &mut env);
    let mut failed = None;
    for (offset, arg) in args[index..].iter().enumerate() {
      if failed.is_none()
        && !matches!(&arg.value, Value::Literal(name) if arg.boundary_known && valid_name(name) && env.readonly(name).is_none())
      {
        failed = Some(env.clone());
      }
      if let Value::Literal(name) = &arg.value
        && arg.boundary_known
        && valid_name(name)
      {
        if let Some(id) = self.write_argument(
          node,
          Some(arg),
          name,
          values.as_ref().map_or(
            Value::Unknown(ValueIssue::Dynamic),
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
      } else if matches!(arg.value, Value::Literal(_)) && arg.boundary_known {
        self.gap(node, frame, ResolutionIssue::DynamicName);
      } else {
        self.invalidate(
          node,
          &mut env,
          frame,
          ResolutionIssue::DynamicName,
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
    let computed = option_values(args, &env);
    let known_prefix = args.iter().take(2).all(|arg| arg.boundary_known);
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
        computed.as_ref().map_or(
          Value::Unknown(ValueIssue::Dynamic),
          |result| {
            if name == b"OPTIND" {
              Value::Literal(result.index.to_string().into_bytes())
            } else {
              result
                .argument
                .clone()
                .unwrap_or(Value::Literal(Vec::new()))
            }
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
          .is_some_and(|result| result.argument.is_none())
      {
        binding = Binding::new(alternative(Alternative::Unset));
      } else if computed.is_none() {
        binding.resolution.alternatives.insert(Alternative::Unset);
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
        computed
          .as_ref()
          .map_or(Value::Unknown(ValueIssue::Dynamic), |result| {
            Value::Literal(vec![result.option])
          }),
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
        [args
          .get(1)
          .filter(|_| known_prefix)
          .map_or(EffectTarget::Variables, |arg| {
            argument_target(arg, NameTarget::Variable)
          })],
      );
    }
    env.options_reset = false;
    env.option_cursor = computed
      .as_ref()
      .map(|result| (result.arguments.clone(), result.offset));
    if computed.is_some() {
      env
        .special
        .insert(b"?".to_vec(), Value::Literal(b"0".to_vec()));
    }
    Flow::next(env)
  }

  fn argument_definition(
    &mut self,
    node: Node<'_>,
    arg: Option<&Argument<'_>>,
    name: &[u8],
    frame: Frame,
  ) -> Option<(SymbolId, OccurrenceId)> {
    use crate::interface::application::engine::{
      DerivedOrigin, OriginMapping, OriginPart,
    };
    let registration = node
      .child_by_field_name("name")
      .or_else(|| node.child_by_field_name("word"))
      .and_then(|name| self.site(name, frame))?;
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
    let key = self.derive_body(
      DerivedOrigin {
        registration,
        argument: frame.location(arg.map_or(node, |arg| arg.node)),
        parts,
      },
      &body,
    );
    self
      .workspace
      .derived_keys
      .get(&key)
      .and_then(|snapshot| self.workspace.document(snapshot.document))
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
        [
          crate::interface::application::engine::EffectTarget::Variable(
            name.to_vec(),
          ),
        ],
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
        self.record(definition, frame.context, resolution, env);
      }
    }
    if env.readonly(name) == Some(true) {
      return Some(definition);
    }
    env.variable_changed(
      name,
      if env.readonly(name).is_none() {
        Some(&value)
      } else {
        None
      },
    );
    env.alternatives.forget(name);
    let mut binding = Binding {
      resolution: super::super::execution::definition(symbol),
      value: Arc::new(value),
    };
    if env.readonly(name).is_some() {
      binding.join(env.variables.get(name));
    }
    env.variables.values.insert(name.to_vec(), binding);
    if matches!(name, b"PWD" | b"OLDPWD") {
      env.directory_variables_modified = true;
    }
    Some(definition)
  }
}

fn exit_status(args: &[Argument<'_>], previous: Value) -> Value {
  if args.is_empty() {
    previous
  } else {
    let issue = match args {
      [
        Argument {
          value: Value::Unknown(issue),
          ..
        },
      ] => *issue,
      _ => ValueIssue::Unspecified,
    };
    decimal_operand(args)
      .and_then(|bytes| std::str::from_utf8(bytes).ok())
      .and_then(|text| text.parse::<u8>().ok())
      .map_or(Value::Unknown(issue), |status| {
        Value::Literal(status.to_string().into_bytes())
      })
  }
}

pub(super) fn output(
  name: &[u8],
  args: &[Argument<'_>],
  env: &Environment,
  limit: usize,
) -> Value {
  let known: Option<Vec<_>> = args
    .iter()
    .map(|arg| match &arg.value {
      Value::Literal(bytes) if arg.boundary_known => Some(bytes.as_slice()),
      _ => None,
    })
    .collect();
  let unknown = || Value::Unknown(ValueIssue::UnsupportedExpansion);
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
        && known.as_ref().is_some_and(|args| {
          !args
            .iter()
            .any(|arg| arg.starts_with(b"-") || arg.starts_with(b"+"))
            || args.first() == Some(&b"--".as_slice())
        }) =>
    {
      Value::Literal(Vec::new())
    }
    b"echo" => {
      let Some(args) = known else {
        return unknown();
      };
      if args.first() == Some(&b"-n".as_slice())
        || args.iter().any(|a| a.contains(&b'\\'))
      {
        return Value::Unknown(ValueIssue::Unspecified);
      }
      let size = args
        .iter()
        .map(|a| a.len())
        .sum::<usize>()
        .saturating_add(args.len().max(1));
      if size > limit {
        return Value::Unknown(ValueIssue::SizeLimit);
      }
      let mut bytes = args.join(&b' ');
      bytes.push(b'\n');
      Value::Literal(bytes)
    }
    b"printf" => formatted_output(args, limit),
    b"pwd"
      if known.as_ref().is_some_and(|args| {
        args.is_empty() || args.as_slice() == [b"-L".as_slice()]
      }) && !env.directory_variables_modified =>
    {
      if let Value::Literal(cwd) = &env.cwd {
        let mut bytes = cwd.clone();
        bytes.push(b'\n');
        Value::Literal(bytes)
      } else {
        unknown()
      }
    }
    _ => unknown(),
  }
}

fn formatted_output(args: &[Argument<'_>], limit: usize) -> Value {
  let Some((format, operands)) = args.split_first() else {
    return Value::Unknown(ValueIssue::Unspecified);
  };
  let Value::Literal(format) = &format.value else {
    return Value::Unknown(ValueIssue::UnsupportedExpansion);
  };
  if format.starts_with(b"-") || args.iter().any(|arg| !arg.boundary_known) {
    return Value::Unknown(ValueIssue::UnsupportedExpansion);
  }
  let mut output = Value::Literal(Vec::new());
  let mut operand = 0;
  loop {
    let before = operand;
    let mut bytes = format.iter().copied();
    while let Some(byte) = bytes.next() {
      let value = match byte {
        b'%' => match bytes.next() {
          Some(b'%') => Value::Literal(vec![b'%']),
          Some(b's') => {
            let value = operands.get(operand).map_or_else(
              || Value::Literal(Vec::new()),
              |arg| arg.value.clone(),
            );
            operand += 1;
            value
          }
          _ => return Value::Unknown(ValueIssue::UnsupportedExpansion),
        },
        b'\\' => Value::Literal(vec![match bytes.next() {
          Some(b'n') => b'\n',
          Some(b't') => b'\t',
          Some(b'r') => b'\r',
          Some(b'\\') => b'\\',
          _ => return Value::Unknown(ValueIssue::UnsupportedExpansion),
        }]),
        _ => Value::Literal(vec![byte]),
      };
      output.concat(&value, limit);
    }
    if operand == before || operand >= operands.len() {
      break;
    }
  }
  output
}

impl Resolver<'_> {
  fn read_values(
    &mut self,
    args: &[Argument<'_>],
    first: usize,
    env: &mut Environment,
  ) -> Option<Vec<Value>> {
    if args[first..].is_empty()
      || args[first..].iter().any(|arg| {
        !arg.boundary_known
          || !matches!(&arg.value, Value::Literal(name) if valid_name(name))
      })
    {
      env.io.consume_input(None);
      return None;
    }
    let raw = args[..first]
      .iter()
      .any(|arg| arg.value == Value::Literal(b"-r".to_vec()));
    let ifs = env.ifs();
    let utf8 = self.utf8(env);
    if env.io.input_order_unknown {
      env.io.consume_input(None);
      return Some(vec![
        Value::Unknown(ValueIssue::Unspecified);
        args.len() - first
      ]);
    }
    let (input, mut offset) = env.io.input.clone()?;
    let word = input_line(
      &input,
      &mut offset,
      raw,
      ifs.empty() == Some(true),
      self.result.request.limits.value_bytes,
    );
    env.io.consume_input(Some(offset));
    let word = word?;
    if !matches!(ifs, Value::Literal(_)) {
      return None;
    }
    let fields = word.split(&ifs, utf8, args.len() - first).ok()?;
    if args[first..].iter().all(|arg| matches!(&arg.value, Value::Literal(name) if env.readonly(name).is_none())) {
      env.special.insert(b"?".to_vec(), Value::Literal(b"0".to_vec()));
    }
    Some(fields.iter().map(word::Field::value).collect())
  }
}

fn input_line(
  input: &[u8],
  offset: &mut usize,
  raw: bool,
  empty_ifs: bool,
  limit: usize,
) -> Option<word::Word> {
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
      return word.issue.is_none().then_some(word);
    }
    if byte == b'\\' && !raw {
      let next = *input.get(*offset)?;
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
  None
}

struct OptionValues {
  option: u8,
  argument: Option<Value>,
  index: usize,
  offset: usize,
  arguments: Vec<Value>,
}
fn option_values(
  args: &[Argument<'_>],
  env: &Environment,
) -> Option<OptionValues> {
  if args.len() < 2 || args.iter().any(|arg| !arg.boundary_known) {
    return None;
  }
  let Value::Literal(options) = &args[0].value else {
    return None;
  };
  let options = options.strip_prefix(b":").unwrap_or(options);
  let mut seen = BTreeSet::new();
  let mut colon_allowed = false;
  for byte in options {
    if *byte == b':' && colon_allowed {
      colon_allowed = false;
    } else if byte.is_ascii_alphanumeric() && seen.insert(byte) {
      colon_allowed = true;
    } else {
      return None;
    }
  }
  let Value::Literal(target) = &args[1].value else {
    return None;
  };
  if !valid_name(target)
    || matches!(target.as_slice(), b"OPTIND" | b"OPTARG")
    || [target.as_slice(), b"OPTIND", b"OPTARG"]
      .iter()
      .any(|name| env.readonly(name).is_some())
  {
    return None;
  }
  let arguments = if args.len() == 2 {
    env.positional.clone()?
  } else {
    args[2..].iter().map(|arg| arg.value.clone()).collect()
  };
  let Value::Literal(index) = env.parameter(b"OPTIND").0 else {
    return None;
  };
  let mut index: usize = std::str::from_utf8(&index).ok()?.parse().ok()?;
  let mut offset = if env.options_reset && index == 1 {
    1
  } else {
    let (previous, offset) = env.option_cursor.as_ref()?;
    if *previous != arguments {
      return None;
    }
    (*offset).max(1)
  };
  let Value::Literal(current) = arguments.get(index.checked_sub(1)?)? else {
    return None;
  };
  if !current.starts_with(b"-") || current == b"--" {
    return None;
  }
  let option = *current.get(offset)?;
  if !option.is_ascii_alphanumeric() {
    return None;
  }
  let position = options.iter().position(|byte| *byte == option)?;
  offset += 1;
  let argument = if options.get(position + 1) == Some(&b':') {
    let argument = if offset < current.len() {
      Value::Literal(current[offset..].to_vec())
    } else {
      index += 1;
      arguments.get(index - 1)?.clone()
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
  Some(OptionValues {
    option,
    argument,
    index,
    offset,
    arguments,
  })
}
