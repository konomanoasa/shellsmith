use crate::interface::application::engine::workspace::derived::syntax_resolution;
use std::collections::{BTreeMap, BTreeSet};

use crate::interface::application::engine::{
  Access, Alternative, Analysis, ArgumentKind, AttributeState, Classification,
  Document, Location, OccurrenceKind, Resolution, UseSite, Value,
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
    analysis: Option<&Analysis>,
  ) -> Vec<SemanticSpan> {
    let analysis =
      analysis.filter(|result| result.snapshots().contains(&self.snapshot()));
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
      if site.kind == OccurrenceKind::Command
        && matches!(site.name, Value::Unknown(_) | Value::Partial(_))
      {
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
        let resolution = syntax_resolution(self, site, analysis);
        kind = command_kind(resolution.as_ref(), &mut modifiers);
      }
      let readonly = analysis
        .map_or(AttributeState::Unknown, |result| result.readonly(site.id));
      if readonly == AttributeState::Known(true) {
        modifiers.insert(Modifier::Readonly);
      }
      if kind == SemanticKind::Variable
        && analysis.is_some_and(|result| result.is_environment(site.id))
      {
        modifiers.insert(Modifier::Environment);
      }
      let location = site.location.clone();
      spans.insert(
        (location.bytes.start, location.bytes.end),
        SemanticSpan {
          location,
          kind,
          modifiers,
          readonly,
          semantic: true,
        },
      );
    }
    for span in self.argument_spans(analysis) {
      spans.insert((span.location.bytes.start, span.location.bytes.end), span);
    }
    spans.into_values().collect()
  }

  fn argument_spans(&self, analysis: Option<&Analysis>) -> Vec<SemanticSpan> {
    let mut spans = Vec::new();
    let mut arguments = analysis.map_or_else(BTreeMap::new, |analysis| {
      analysis.argument_summaries(self.snapshot())
    });
    for role in self.argument_roles() {
      arguments
        .entry((role.location.bytes.start, role.location.bytes.end))
        .or_insert_with(|| role.clone());
    }
    for role in arguments.into_values() {
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
          let resolution = analysis.and_then(|analysis| {
            analysis
              .summarize(UseSite::argument(&role.location))
              .resolution
          });
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
