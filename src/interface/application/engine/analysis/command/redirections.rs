use super::super::execution::children;
use super::super::execution::{Environment, Flow};
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
    let source = self
      .workspace
      .document(frame.snapshot.document)
      .expect("document")
      .source
      .as_bytes();
    let fd = descriptor(node, source);
    let operator = node
      .child_by_field_name("operator")
      .map(|op| word::lexical(op, source))
      .unwrap_or_default();
    let Some(filename) = node.child_by_field_name("filename") else {
      return Task::Done(Flow::next(env));
    };
    Task::expand(filename, env, frame).then_word(move |_, mut result| {
      let target = result.word.scalar();
      let env = &mut result.env;
      match (fd, operator.as_slice(), &target) {
        (0, b"<&", Value::Literal(target)) if target == b"0" => {}
        (0, b"<" | b"<>" | b">" | b">>" | b">|", _) => {
          env.io.connect_input(None);
        }
        (0, b"<&", Value::Literal(target)) if target == b"-" => {
          env.io.connect_input(None);
        }
        (1, b"<" | b"<>" | b">" | b">>" | b">|", _) => {
          env.io.stdout_redirected = true;
        }
        (2, b"<" | b"<>" | b">" | b">>" | b">|", _) => {
          env.io.stderr_captured = false;
        }
        (1, b">&", Value::Literal(target)) if target == b"2" => {
          env.io.stdout_redirected = !env.io.stderr_captured;
        }
        (2, b">&", Value::Literal(target)) if target == b"1" => {
          env.io.stderr_captured = !env.io.stdout_redirected;
        }
        (1, b">&", Value::Literal(target)) if target == b"1" => {}
        (2, b">&", Value::Literal(target)) if target == b"2" => {}
        (1, b">&", Value::Literal(target)) if target == b"-" => {
          env.io.stdout_redirected = true;
        }
        (2, b">&", Value::Literal(target)) if target == b"-" => {
          env.io.stderr_captured = false;
        }
        _ => {
          env.io.forget_inputs();
          if let Some(output) = &mut env.io.output {
            output.fail(ValueIssue::UnsupportedExpansion);
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
    let Some(body) = self
      .index
      .heredocs
      .get(&(frame.snapshot, node.start_byte()))
      .copied()
    else {
      return Task::Done(self.opaque(
        node,
        env,
        frame,
        ResolutionIssue::Recovery,
      ));
    };
    let source = self
      .workspace
      .document(frame.snapshot.document)
      .expect("document")
      .source
      .as_bytes();
    let fd = descriptor(node, source);
    let strip = node
      .child_by_field_name("operator")
      .is_some_and(|op| word::lexical(op, source) == b"<<-");
    let task = if body.kind() == "quoted_here_document_body" {
      Task::expanded(super::super::expansion::Expansion {
        word: word::Word::value(
          Value::Literal(source[body.byte_range()].to_vec()),
          true,
        ),
        env,
      })
    } else {
      self.expand_here(body, strip, env, frame)
    };
    task.then_word(move |_, mut result| {
      if fd == 0 {
        let input = match result.word.scalar() {
          Value::Literal(bytes) => {
            let bytes = if strip && body.kind() == "quoted_here_document_body" {
              bytes
                .split_inclusive(|byte| *byte == b'\n')
                .flat_map(|line| {
                  line.iter().skip_while(|byte| **byte == b'\t').copied()
                })
                .collect()
            } else {
              bytes
            };
            Some((std::sync::Arc::new(bytes), 0))
          }
          _ => None,
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
  cancelled: &AtomicBool,
) -> Result<(), Error> {
  let mut headers: BTreeMap<usize, Vec<Node<'a>>> = BTreeMap::new();
  let mut bodies: BTreeMap<usize, Vec<Node<'a>>> = BTreeMap::new();
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
        if let Some(body) = node.child_by_field_name("body") {
          bodies.entry(owner).or_default().push(body);
        }
      }
      "here_end" | "tilde_expansion" => continue,
      _ => {}
    }
    pending.extend(
      super::super::execution::children(node)
        .into_iter()
        .rev()
        .map(|child| (child, owner)),
    );
  }
  for (owner, mut headers) in headers {
    if let Some(mut bodies) = bodies.remove(&owner)
      && headers.len() == bodies.len()
    {
      headers.sort_by_key(Node::start_byte);
      bodies.sort_by_key(Node::start_byte);
      for (header, body) in headers.into_iter().zip(bodies) {
        output.insert((document.snapshot(), header.start_byte()), body);
      }
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
}
impl Redirections {
  pub(in crate::interface::application::engine::analysis) fn new(
    nodes: &[Node<'_>],
    source: &[u8],
    env: &mut Environment,
  ) -> Self {
    let mut pending = nodes.to_vec();
    let mut saved = Self {
      input: None,
      stdout: None,
      stderr: None,
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
    for env in flow.0.values_mut() {
      if let Some(input) = self.input {
        env.io.restore_input(input);
      }
      if let Some(stdout) = self.stdout {
        env.io.stdout_redirected = stdout;
      }
      if let Some(stderr) = self.stderr {
        env.io.stderr_captured = stderr;
      }
    }
  }
}
