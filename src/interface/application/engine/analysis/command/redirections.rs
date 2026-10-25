use super::super::execution::{CommandFailure, Environment, Flow};
use super::super::execution::{children, damage_outside};
use super::super::expansion::Expansion;
use super::super::{Frame, Resolver, Task};
use crate::interface::application::engine::{
  Document, Error, ResolutionIssue, Snapshot, Value, ValueIssue,
  check_cancelled, word,
};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;

impl<'a> Resolver<'a> {
  pub(in crate::interface::application::engine::analysis) fn redirect(
    &mut self,
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let source = self.workspace.source(frame.snapshot.document);
    let fd = descriptor(node, source);
    let operator = node
      .child_by_field_name("operator")
      .map(|op| word::lexical(op, source))
      .unwrap_or_default();
    let Some(filename) = node.child_by_field_name("filename") else {
      return Task::Done(Flow::next(env));
    };
    Task::expand(filename, false, env, frame).then_word(move |_, mut result| {
      let target = result.word.scalar();
      let env = &mut result.env;
      if matches!(operator.as_slice(), b"<" | b"<>" | b">" | b">>" | b">|") {
        if target == Value::Literal(Vec::new()) {
          env.failure = CommandFailure::Definite;
        } else {
          env
            .failure
            .uncertain(target.issue().unwrap_or(ValueIssue::Dynamic));
        }
      } else {
        let known = matches!(
          (fd, operator.as_slice(), target.literal().ok()),
          (_, b"<&" | b">&", Some(b"-"))
            | (0, b"<&", Some(b"0"))
            | (1 | 2, b">&", Some(b"1" | b"2"))
        );
        if !known {
          env
            .failure
            .uncertain(target.issue().unwrap_or(ValueIssue::Dynamic));
        }
      }
      match (fd, operator.as_slice(), target.literal().ok()) {
        (0, b"<&", Some(b"0"))
        | (1, b">&", Some(b"1"))
        | (2, b">&", Some(b"2")) => {}
        (0, b"<" | b"<>" | b">" | b">>" | b">|", _) => {
          env
            .io
            .connect_input(Err(target.issue().unwrap_or(ValueIssue::Dynamic)));
        }
        (0, b"<&", Some(b"-")) => {
          env.io.connect_input(Err(ValueIssue::Dynamic));
        }
        (1, b"<" | b"<>" | b">" | b">>" | b">|", _)
        | (1, b">&", Some(b"-")) => {
          env.io.stdout_redirected = true;
        }
        (2, b"<" | b"<>" | b">" | b">>" | b">|", _)
        | (2, b">&", Some(b"-")) => {
          env.io.stderr_captured = false;
        }
        (1, b">&", Some(b"2")) => {
          env.io.stdout_redirected = !env.io.stderr_captured;
        }
        (2, b">&", Some(b"1")) => {
          env.io.stderr_captured = !env.io.stdout_redirected;
        }
        _ => {
          env
            .io
            .forget_inputs(target.issue().unwrap_or(ValueIssue::Dynamic));
          if let Some(output) = &mut env.io.output {
            output
              .fail(target.issue().unwrap_or(ValueIssue::UnsupportedExpansion));
          }
        }
      }
      Task::Done(Flow::next(result.env))
    })
  }

  pub(in crate::interface::application::engine::analysis) fn heredoc(
    &mut self,
    node: Node<'a>,
    env: Environment,
    frame: Frame,
  ) -> Task<'a> {
    let Some(document) = self.index.heredoc(frame.snapshot, node) else {
      return Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::Recovery,
        None,
      ));
    };
    let source = self.workspace.source(frame.snapshot.document);
    let fd = descriptor(node, source);
    // Damaged syntax outside the body ends the here-document content.
    let rest = damage_outside(document, &["body"]);
    let task = match document.child_by_field_name("body") {
      Some(body) => self.expand_start(body, false, env, frame),
      None => Task::expanded(Expansion {
        word: word::Word::value(Value::Literal(Vec::new()), true),
        env,
      }),
    };
    task.then_word(move |resolver, mut result| {
      for node in rest {
        result.word.fail(ValueIssue::Recovery);
        resolver.damaged_expansion(node, &mut result.env, frame);
      }
      if fd == 0 {
        let input = match result.word.scalar() {
          Value::Literal(bytes) => Ok((std::sync::Arc::new(bytes), 0)),
          value => Err(value.issue().expect("unknown input")),
        };
        result.env.io.connect_input(input);
      } else if fd == 1 {
        result.env.io.stdout_redirected = true;
      } else if fd == 2 {
        result.env.io.stderr_captured = false;
      }
      Task::Done(Flow::next(result.env))
    })
  }
}

pub(in crate::interface::application::engine::analysis) fn index_heredocs<
  'a,
>(
  document: &'a Document,
  output: &mut BTreeMap<(Snapshot, usize), Node<'a>>,
  stripped: &mut BTreeMap<(Snapshot, usize), usize>,
  cancelled: &AtomicBool,
) -> Result<(), Error> {
  let mut headers: BTreeMap<usize, Vec<Node<'a>>> = BTreeMap::new();
  let mut documents: BTreeMap<usize, Vec<Node<'a>>> = BTreeMap::new();
  let mut pending = vec![(document.tree.root_node(), 0)];
  while let Some((node, mut owner)) = pending.pop() {
    check_cancelled(cancelled)?;
    if node.is_error() || node.is_missing() {
      continue;
    }
    if matches!(
      node.kind(),
      "program" | "command_substitution_body" | "backquote_substitution_body"
    ) {
      owner = node.id();
    }
    match node.kind() {
      "io_here" => {
        headers.entry(owner).or_default().push(node);
      }
      "here_document" => {
        documents.entry(owner).or_default().push(node);
      }
      "here_end" | "tilde_expansion" => continue,
      _ => {}
    }
    pending
      .extend(children(node).into_iter().rev().map(|child| (child, owner)));
  }
  let mut bodies = Vec::new();
  for (owner, mut headers) in headers {
    if let Some(mut documents) = documents.remove(&owner)
      && headers.len() == documents.len()
    {
      headers.sort_by_key(Node::start_byte);
      documents.sort_by_key(Node::start_byte);
      for (header, here) in headers.into_iter().zip(documents) {
        if header
          .child_by_field_name("operator")
          .is_some_and(|operator| {
            word::lexical(operator, document.source().as_bytes()) == b"<<-"
          })
          && let Some(body) = here.child_by_field_name("body")
        {
          bodies.push(body.byte_range());
        }
        output.insert((document.snapshot(), header.start_byte()), here);
      }
    }
  }
  // Bodies nest or are disjoint, so the outermost ones cover the rest.
  bodies.sort_by_key(|body| body.start);
  let mut covered = 0;
  for body in bodies {
    if body.start >= covered {
      covered = body.end;
      stripped.insert((document.snapshot(), body.start), body.end);
    }
  }
  Ok(())
}

fn descriptor(node: Node<'_>, source: &[u8]) -> u32 {
  if let Some(number) = node
    .parent()
    .and_then(|parent| parent.child_by_field_name("number"))
  {
    std::str::from_utf8(&source[number.byte_range()])
      .ok()
      .and_then(|number| number.parse().ok())
      .unwrap_or(u32::MAX)
  } else {
    u32::from(
      !node
        .child_by_field_name("operator")
        .is_some_and(|op| source[op.byte_range()].starts_with(b"<")),
    )
  }
}

#[derive(Clone)]
pub(in crate::interface::application::engine::analysis) struct Redirections {
  input: Option<usize>,
  stdout: Option<bool>,
  stderr: Option<bool>,
  /// Redirections that may also persist leave their descriptors unknown.
  may_persist: bool,
}

impl Redirections {
  pub(in crate::interface::application::engine::analysis) fn new(
    nodes: &[Node<'_>],
    source: &[u8],
    env: &mut Environment,
    may_persist: bool,
  ) -> Self {
    let mut pending = nodes.to_vec();
    let mut saved = Self {
      input: None,
      stdout: None,
      stderr: None,
      may_persist,
    };
    while let Some(node) = pending.pop() {
      if matches!(node.kind(), "io_here" | "io_file") {
        match descriptor(node, source) {
          0 => {
            saved.input.get_or_insert_with(|| env.io.save_input());
          }
          1 => saved.stdout = Some(env.io.stdout_redirected),
          2 => saved.stderr = Some(env.io.stderr_captured),
          _ => {}
        }
      } else {
        pending.extend(children(node));
      }
    }
    saved
  }

  pub(in crate::interface::application::engine::analysis) fn restore(
    &self,
    flow: &mut Flow,
  ) {
    for env in flow.environments_mut() {
      if let Some(input) = self.input {
        env.io.restore_input(input);
        if self.may_persist {
          env.io.input = Err(ValueIssue::Dynamic);
        }
      }
      if let Some(stdout) = self.stdout {
        env.io.stdout_redirected = stdout;
        if self.may_persist
          && let Some(output) = &mut env.io.output
        {
          output.fail(ValueIssue::Dynamic);
        }
      }
      if let Some(stderr) = self.stderr {
        env.io.stderr_captured =
          stderr || (self.may_persist && env.io.stderr_captured);
      }
    }
  }
}
