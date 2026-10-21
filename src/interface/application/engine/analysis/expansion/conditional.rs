use super::super::execution::{Environment, ValueId};
use super::super::{Frame, Resolver};
use super::arithmetic::{binary, integer, unary, value};
use crate::interface::application::engine::word::{self, Part, Parts};
use crate::interface::application::engine::{
  Alternative, Error, OccurrenceId, Value, check_cancelled, valid_name,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tree_sitter::Node;

impl<'a> Resolver<'a> {
  pub(in crate::interface::application::engine::analysis) fn conditional_assignment(
    &mut self,
    node: Node<'a>,
    env: &Environment,
    frame: Frame,
  ) -> Result<Option<Environment>, Error> {
    let source = self.workspace.source(frame.snapshot.document);
    let (Some(name), Some(body)) = (
      node.child_by_field_name("name"),
      node.child_by_field_name("value"),
    ) else {
      return Ok(None);
    };
    let spelling = word::lexical(name, source);
    if !valid_name(&spelling)
      || env.readonly(&spelling).is_some()
      || node.has_error()
      || word::transformed(node, source)
    {
      return Ok(None);
    }
    let mut reads = BTreeSet::new();
    let Some(result) = self.conditional_word(body, env, frame, &mut reads)?
    else {
      return Ok(None);
    };
    let projected = env
      .alternatives
      .expressions()
      .projected(result)
      .as_ref()
      .clone();
    for id in reads {
      self.read(id, env, frame);
    }
    let mut output = env.clone();
    if self.write(name, &mut output, frame, false, projected) {
      let resolution = output.variables.get(&spelling).resolution.clone();
      output.alternatives.store(&spelling, result, resolution);
    }
    Ok(Some(output))
  }

  fn conditional_variable(
    &self,
    node: Node<'a>,
    env: &Environment,
    frame: Frame,
    parameter: bool,
    reads: &mut BTreeSet<OccurrenceId>,
  ) -> Result<Option<ValueId>, Error> {
    let Some(id) = self.site(node, frame) else {
      return Ok(None);
    };
    if self.variable_value(id, env).is_none() {
      return Ok(None);
    }
    let source = self.workspace.source(frame.snapshot.document);
    let name = word::lexical(node, source);
    if !valid_name(&name)
      || (parameter
        && env.nounset != Some(false)
        && env
          .variables
          .get(&name)
          .resolution
          .alternatives
          .contains(&Alternative::Unset))
    {
      return Ok(None);
    }
    let result = env.alternatives.scalar(env, &name, self.cancelled)?;
    if env.alternatives.expressions().maximum(result).is_none() {
      return Ok(None);
    }
    reads.insert(id);
    Ok(Some(result))
  }

  fn conditional_word(
    &self,
    node: Node<'a>,
    env: &Environment,
    frame: Frame,
    reads: &mut BTreeSet<OccurrenceId>,
  ) -> Result<Option<ValueId>, Error> {
    let source = self.workspace.source(frame.snapshot.document);
    let mut result = env
      .alternatives
      .expressions()
      .literal(Arc::new(Value::Literal(Vec::new())));
    for part in Parts::new(node, false, source) {
      check_cancelled(self.cancelled)?;
      let next = match part {
        Part::Fragment(fragment) => env
          .alternatives
          .expressions()
          .literal(Arc::new(Value::Literal(fragment.bytes))),
        Part::Expansion(node, _) if node.kind() == "arithmetic_expansion" => {
          let Some(value) =
            self.conditional_arithmetic(node, env, frame, reads)?
          else {
            return Ok(None);
          };
          value
        }
        Part::Expansion(node, _) if node.kind() == "parameter_expansion" => {
          let Some(parameter) = node.child_by_field_name("parameter") else {
            return Ok(None);
          };
          let Some(value) =
            self.conditional_variable(parameter, env, frame, true, reads)?
          else {
            return Ok(None);
          };
          if let Some(operator) = node.child_by_field_name("operator") {
            if operator.kind() != "parameter_length_operator" {
              return Ok(None);
            }
            let Some(width) = env.alternatives.expressions().ascii_width(value)
            else {
              return Ok(None);
            };
            env
              .alternatives
              .expressions()
              .literal(Arc::new(Value::Literal(width.to_string().into_bytes())))
          } else {
            value
          }
        }
        _ => return Ok(None),
      };
      let mut values = env.alternatives.expressions();
      let Some(combined) = values.concat(result, next) else {
        return Ok(None);
      };
      if values
        .maximum(combined)
        .is_none_or(|size| size > self.result.request.limits.value_bytes)
      {
        return Ok(None);
      }
      result = combined;
    }
    Ok(Some(result))
  }

  fn conditional_arithmetic(
    &self,
    root: Node<'a>,
    env: &Environment,
    frame: Frame,
    reads: &mut BTreeSet<OccurrenceId>,
  ) -> Result<Option<ValueId>, Error> {
    let source = self.workspace.source(frame.snapshot.document);
    let mut pending = vec![(root, false)];
    let mut computed = BTreeMap::new();
    while let Some((node, visited)) = pending.pop() {
      check_cancelled(self.cancelled)?;
      let result = match node.kind() {
        "arithmetic_number" | "arithmetic_source_fragment" => env
          .alternatives
          .expressions()
          .literal(Arc::new(value(integer(&Value::Literal(word::lexical(
            node, source,
          )))))),
        "arithmetic_variable" => {
          let Some(input) =
            self.conditional_variable(node, env, frame, false, reads)?
          else {
            return Ok(None);
          };
          env.alternatives.expressions().apply(
            input,
            input,
            |a, _| value(integer(a)),
            self.cancelled,
          )?
        }
        "arithmetic_expansion"
        | "parenthesized_arithmetic"
        | "arithmetic_unary_expression" => {
          let unary_operator = node.kind() == "arithmetic_unary_expression";
          let Some(child) = node.child_by_field_name(if unary_operator {
            "operand"
          } else {
            "expression"
          }) else {
            return Ok(None);
          };
          if !visited {
            pending.extend([(node, true), (child, false)]);
            continue;
          }
          let input = computed[&child.id()];
          if unary_operator {
            let Some(operator) = node.child_by_field_name("operator") else {
              return Ok(None);
            };
            let operator = word::lexical(operator, source);
            env.alternatives.expressions().apply(
              input,
              input,
              |a, _| value(unary(&operator, integer(a))),
              self.cancelled,
            )?
          } else {
            input
          }
        }
        "arithmetic_binary_expression" => {
          let (Some(left), Some(right), Some(operator)) = (
            node.child_by_field_name("left"),
            node.child_by_field_name("right"),
            node.child_by_field_name("operator"),
          ) else {
            return Ok(None);
          };
          let operator = word::lexical(operator, source);
          if matches!(operator.as_slice(), b"&&" | b"||") {
            return Ok(None);
          }
          if !visited {
            pending.extend([(node, true), (right, false), (left, false)]);
            continue;
          }
          env.alternatives.expressions().apply(
            computed[&left.id()],
            computed[&right.id()],
            |a, b| value(binary(&operator, integer(a), integer(b))),
            self.cancelled,
          )?
        }
        _ => return Ok(None),
      };
      if env.alternatives.expressions().maximum(result).is_none() {
        return Ok(None);
      }
      computed.insert(node.id(), result);
    }
    Ok(Some(computed[&root.id()]))
  }
}
