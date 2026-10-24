use super::result::{argument_resolution, join_resolution, syntax_resolution};
use std::collections::{BTreeMap, BTreeSet};

use crate::interface::application::engine::{
  Access, Alternative, Analysis, ArgumentKind, ArgumentRole, AttributeState,
  Classification, Document, Location, OccurrenceId, OccurrenceKind, Resolution,
  UseSite, syntax_spelling,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SemanticKind {
  Comment,
  Keyword,
  Operator,
  String,
  Number,
  Variable,
  Parameter,
  Function,
  Command,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Modifier {
  Declaration,
  Definition,
  Modification,
  Readonly,
  DefaultLibrary,
  Environment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticSpan {
  pub location: Location,
  pub kind: SemanticKind,
  pub modifiers: BTreeSet<Modifier>,
  pub readonly: AttributeState,
  pub semantic: bool,
}

impl Document {
  #[must_use]
  pub(in crate::interface::application::engine) fn semantic_spans(
    &self,
    inputs: &[(&Document, &Analysis)],
  ) -> Vec<SemanticSpan> {
    let mut spans = BTreeMap::new();
    for classification in self.classifications() {
      let Some(kind) = self.semantic_kind(classification) else {
        continue;
      };
      let location = classification.location.clone();
      spans.insert(
        (location.bytes.start, location.bytes.end),
        SemanticSpan {
          location,
          kind,
          modifiers: BTreeSet::new(),
          readonly: AttributeState::Unknown,
          semantic: false,
        },
      );
    }
    for site in self.occurrences() {
      let spelling = syntax_spelling(self, site);
      if spelling.is_empty() {
        continue;
      }
      let mut kind = match site.kind {
        OccurrenceKind::FunctionDefinition => SemanticKind::Function,
        OccurrenceKind::Command => SemanticKind::Command,
        OccurrenceKind::PositionalParameter
        | OccurrenceKind::SpecialParameter => SemanticKind::Parameter,
        _ => SemanticKind::Variable,
      };
      let mut modifiers = BTreeSet::new();
      match site.access {
        Access::Definition => {
          modifiers.insert(Modifier::Definition);
        }
        Access::Write | Access::ReadWrite | Access::ReadConditionalWrite => {
          modifiers.extend([Modifier::Definition, Modifier::Modification]);
        }
        _ => {}
      }
      if site.kind == OccurrenceKind::DeclarationAssignment {
        modifiers.insert(Modifier::Declaration);
      }
      if site.kind == OccurrenceKind::Command {
        let resolution = syntax_resolution(self, site, None)
          .or_else(|| resolved(inputs, site.id.index));
        kind = command_kind(resolution.as_ref(), &mut modifiers);
      }
      let readonly = inputs
        .iter()
        .flat_map(|(document, analysis)| {
          analysis
            .readonly_contexts(OccurrenceId {
              snapshot: document.snapshot(),
              index: site.id.index,
            })
            .map(|(_, state)| state)
        })
        .reduce(AttributeState::join)
        .unwrap_or(AttributeState::Unknown);
      if readonly == AttributeState::Known(true) {
        modifiers.insert(Modifier::Readonly);
      }
      if kind == SemanticKind::Variable
        && resolved(inputs, site.id.index)
          .is_some_and(|resolution| resolution.is_environment())
      {
        modifiers.insert(Modifier::Environment);
      }
      for location in spelling {
        spans.insert(
          (location.bytes.start, location.bytes.end),
          SemanticSpan {
            location,
            kind,
            modifiers: modifiers.clone(),
            readonly,
            semantic: true,
          },
        );
      }
    }
    for span in self.argument_spans(inputs) {
      spans.insert((span.location.bytes.start, span.location.bytes.end), span);
    }
    spans.into_values().collect()
  }

  fn argument_spans(
    &self,
    inputs: &[(&Document, &Analysis)],
  ) -> Vec<SemanticSpan> {
    let mut roles: BTreeMap<_, (ArgumentRole, Option<Resolution>)> =
      BTreeMap::new();
    for (document, analysis) in inputs {
      for (range, mut role) in analysis.argument_summaries(document.snapshot())
      {
        let resolution = argument_resolution(Some(analysis), &role);
        role.location.snapshot = self.snapshot();
        for location in &mut role.spelling {
          location.snapshot = self.snapshot();
        }
        roles
          .entry(range)
          .and_modify(|(old, previous)| {
            old.join(&role);
            join_resolution(previous, resolution.as_ref());
          })
          .or_insert((role, resolution));
      }
    }
    for role in self.argument_roles() {
      roles
        .entry((role.location.bytes.start, role.location.bytes.end))
        .or_insert_with(|| (role.clone(), None));
    }
    let mut spans = Vec::new();
    for (role, resolution) in roles.into_values() {
      let Some(kind) = role.kind else {
        continue;
      };
      let mut modifiers = BTreeSet::new();
      let mut readonly = AttributeState::Unknown;
      let kind = match kind {
        ArgumentKind::ShellBody => continue,
        ArgumentKind::Number => SemanticKind::Number,
        ArgumentKind::Function { .. } => SemanticKind::Function,
        ArgumentKind::Variable {
          access,
          declaration,
          readonly: attribute,
          ..
        } => {
          if declaration {
            modifiers.insert(Modifier::Declaration);
          }
          if access == Access::Write {
            modifiers.extend([Modifier::Definition, Modifier::Modification]);
          }
          readonly = attribute;
          if readonly == AttributeState::Known(true) {
            modifiers.insert(Modifier::Readonly);
          }
          SemanticKind::Variable
        }
        ArgumentKind::Command { .. } => {
          command_kind(resolution.as_ref(), &mut modifiers)
        }
      };
      for location in role.spelling {
        spans.push(SemanticSpan {
          location,
          kind,
          modifiers: modifiers.clone(),
          readonly,
          semantic: true,
        });
      }
    }
    spans
  }

  fn semantic_kind(
    &self,
    classification: &Classification,
  ) -> Option<SemanticKind> {
    Some(match classification.capture {
      "comment" => SemanticKind::Comment,
      "keyword" => SemanticKind::Keyword,
      "operator" => SemanticKind::Operator,
      "number" => SemanticKind::Number,
      "variable" | "variable.builtin" => {
        let bytes = &classification.location.bytes;
        if self
          .tree
          .root_node()
          .descendant_for_byte_range(bytes.start, bytes.end)
          .is_some_and(|node| node.kind() == "$")
        {
          return None;
        }
        SemanticKind::Variable
      }
      "variable.parameter" => SemanticKind::Parameter,
      "function" => SemanticKind::Function,
      "function.call" => SemanticKind::Command,
      name if name.starts_with("string") || name.starts_with("character") => {
        SemanticKind::String
      }
      _ => return None,
    })
  }
}

fn resolved(
  inputs: &[(&Document, &Analysis)],
  index: usize,
) -> Option<Resolution> {
  inputs
    .iter()
    .filter_map(|(document, analysis)| {
      analysis
        .summarize(UseSite::Syntax(OccurrenceId {
          snapshot: document.snapshot(),
          index,
        }))
        .resolution
    })
    .reduce(|mut left, right| {
      left.join(&right);
      left
    })
}

fn command_kind(
  resolution: Option<&Resolution>,
  modifiers: &mut BTreeSet<Modifier>,
) -> SemanticKind {
  let Some(resolution) = resolution.filter(|value| value.issues.is_empty())
  else {
    return SemanticKind::Command;
  };
  if !resolution.candidates.is_empty() && resolution.alternatives.is_empty() {
    return SemanticKind::Function;
  }
  if resolution.candidates.is_empty()
    && resolution.alternatives.len() == 1
    && resolution.alternatives.contains(&Alternative::Builtin)
  {
    modifiers.insert(Modifier::DefaultLibrary);
  }
  SemanticKind::Command
}
