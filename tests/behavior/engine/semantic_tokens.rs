mod classification {
  use crate::support;

  use shellsmith::interface::application::engine::{
    AnalysisRequest, ArgumentKind, AttributeState, DocumentId, ExecutionKind,
    InitialEnvironment, Modifier, SemanticKind, Workspace,
  };
  use std::sync::atomic::AtomicBool;

  fn workspace(source: &str) -> Workspace {
    let mut workspace = Workspace::default();
    let document = support::parse(1, 1, source);
    assert!(!document.has_syntax_errors(), "{source}");
    let mut request = AnalysisRequest::new(document.snapshot());
    request.initial = InitialEnvironment::empty();
    workspace.insert(document).unwrap();
    let analysis = workspace
      .analyze(
        request,
        &mut support::Inputs::default(),
        &AtomicBool::new(false),
      )
      .unwrap();
    workspace.publish_analysis(analysis).unwrap();
    workspace
  }

  #[test]
  fn entry_aggregation_preserves_reached_roles() {
    for (source, first, second, spelling, agreed, mixed) in [
      (
        "a() { :; }; b() { a; }",
        ". /lib; b",
        ". /lib; unset -f a; b",
        "a;",
        SemanticKind::Function,
        SemanticKind::Command,
      ),
      (
        "a() { :; }; b() { command -v a; }",
        ". /lib; b",
        ". /lib; unset -f a; b",
        "a;",
        SemanticKind::Function,
        SemanticKind::Command,
      ),
      (
        "b() { \"$1\" 7; }",
        ". /lib; b shift",
        ". /lib; b echo",
        "7",
        SemanticKind::Number,
        SemanticKind::String,
      ),
      (
        "a() { :; }; b() { \"$1\" -v a; }",
        ". /lib; b command",
        ". /lib; b echo",
        "a;",
        SemanticKind::Function,
        SemanticKind::String,
      ),
    ] {
      assert_entry_classification(
        source, first, second, spelling, agreed, mixed,
      );
    }
  }

  #[test]
  fn entry_aggregation_projects_embedded_bodies() {
    for (source, first, second, spelling, agreed, mixed) in [
      (
        "a() { :; }; b() { trap 'a' EXIT; }",
        ". /lib; b",
        ". /lib; unset -f a; b",
        "a'",
        SemanticKind::Function,
        SemanticKind::Command,
      ),
      (
        "a() { :; }; b() { \"$1\" 'a' EXIT; }",
        ". /lib; b trap",
        ". /lib; b echo",
        "a'",
        SemanticKind::Function,
        SemanticKind::String,
      ),
      (
        "a() { :; }; b() { trap \"trap 'a' EXIT\" EXIT; }",
        ". /lib; b",
        ". /lib; unset -f a; b",
        "a'",
        SemanticKind::Function,
        SemanticKind::Command,
      ),
      (
        "a() { :; }; b() { trap '\"$cmd\" \"a\" EXIT' EXIT; }",
        ". /lib; cmd=trap; b",
        ". /lib; cmd=echo; b",
        "a\"",
        SemanticKind::Function,
        SemanticKind::String,
      ),
      (
        "a() { :; }; b() { trap \"a$1\" EXIT; }",
        ". /lib; b ''",
        ". /lib; read unknown; b \"$unknown\"",
        "a$1",
        SemanticKind::Function,
        SemanticKind::String,
      ),
    ] {
      assert_entry_classification(
        source, first, second, spelling, agreed, mixed,
      );
    }
  }

  fn assert_entry_classification(
    source: &str,
    first: &str,
    second: &str,
    spelling: &str,
    agreed: SemanticKind,
    mixed: SemanticKind,
  ) {
    let base = workspace(source);
    let entry = |id, text| {
      let mut entry = Workspace::default();
      let caller = support::parse(id, 1, text);
      let mut request = AnalysisRequest::new(caller.snapshot());
      request.initial = InitialEnvironment::empty();
      entry.insert(caller).unwrap();
      let mut inputs =
        support::Inputs::sources([("/lib", support::parse(1, 1, source))]);
      let analysis = entry
        .analyze(request, &mut inputs, &AtomicBool::new(false))
        .unwrap();
      entry.publish_analysis(analysis).unwrap();
      entry
    };
    let first = entry(2, first);
    let second = entry(3, second);
    let offset = source.rfind(spelling).unwrap();
    for (entries, expected) in [
      (vec![&first], agreed),
      (vec![&first, &second], mixed),
      (vec![&second, &first], mixed),
    ] {
      let spans = base.semantic_spans_with(DocumentId(1), &entries).unwrap();
      let actual = spans
        .iter()
        .rev()
        .filter(|span| span.location.bytes.contains(&offset))
        .min_by_key(|span| (span.location.bytes.len(), !span.semantic))
        .unwrap();
      assert_eq!(actual.kind, expected, "{source}: {spans:?}");
    }
    let mut revised = base;
    revised.insert(support::parse(1, 2, source)).unwrap();
    assert_eq!(
      revised
        .semantic_spans_with(DocumentId(1), &[&first, &second])
        .unwrap(),
      revised.semantic_spans(DocumentId(1)).unwrap()
    );
  }

  #[test]
  fn only_required_posix_utilities_receive_default_library() {
    let required = [
      ":", ".", "break", "continue", "eval", "exec", "exit", "export",
      "readonly", "return", "set", "shift", "times", "trap", "unset", "alias",
      "bg", "cd", "command", "fc", "fg", "getopts", "hash", "jobs", "kill",
      "read", "type", "ulimit", "umask", "unalias", "wait",
    ];
    for name in required {
      let source = format!("{name} argument\n");
      let workspace = workspace(&source);
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      let command = spans
        .iter()
        .find(|span| span.location.bytes.start == 0 && span.semantic)
        .unwrap();
      assert_eq!(command.kind, SemanticKind::Command, "{name}");
      assert!(
        command.modifiers.contains(&Modifier::DefaultLibrary),
        "{name}"
      );
    }
    for name in [
      "test", "[", "echo", "printf", "pwd", "true", "false", "newgrp", "local",
      "source", "cat", "/bin/cd",
    ] {
      let workspace = workspace(&format!("{name} argument\n"));
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      let command = spans
        .iter()
        .find(|span| span.location.bytes.start == 0 && span.semantic)
        .unwrap();
      assert_eq!(command.kind, SemanticKind::Command, "{name}");
      assert!(
        !command.modifiers.contains(&Modifier::DefaultLibrary),
        "{name}"
      );
    }
  }

  #[test]
  fn functions_shadow_intrinsics_but_uncertain_calls_remain_commands() {
    let source = "cd() { :; }; cd\nif condition; then f() { :; }; fi\nf\n";
    let workspace = workspace(source);
    let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
    let definition = spans
      .iter()
      .find(|span| span.location.bytes == (0..2))
      .unwrap();
    assert_eq!(definition.kind, SemanticKind::Function);
    assert!(definition.modifiers.contains(&Modifier::Definition));
    let call = spans
      .iter()
      .find(|span| span.location.bytes == (13..15))
      .unwrap();
    assert_eq!(call.kind, SemanticKind::Function);
    assert!(!call.modifiers.contains(&Modifier::DefaultLibrary));
    let call = spans
      .iter()
      .find(|span| span.location.bytes.start == source.rfind("f\n").unwrap())
      .unwrap();
    assert_eq!(call.kind, SemanticKind::Command);
  }

  #[test]
  fn function_uses_and_name_operands_require_a_calling_context() {
    for body in ["a() { :; }", "if :; then a() { :; }; else a() { :; }; fi"] {
      for (command, syntax_kind) in [
        ("", SemanticKind::Command),
        ("unset -f ", SemanticKind::Function),
        ("command -v ", SemanticKind::String),
      ] {
        for call in ["", "idle\n"] {
          let source =
            format!("idle() {{ {body}; {command}a # target\n}}\n{call}");
          let workspace = workspace(&source);
          let snapshot = workspace.document(DocumentId(1)).unwrap().snapshot();
          let offset = source.find("a # target").unwrap();
          let resolution = workspace.resolution_at(snapshot, offset).unwrap();
          if call.is_empty() {
            assert_eq!(resolution, None, "{source}");
          } else {
            let resolution = resolution.unwrap();
            assert_eq!(
              resolution.candidates,
              workspace
                .symbols_named(b"a")
                .map(|symbol| symbol.id)
                .collect(),
              "{source}"
            );
            assert!(resolution.is_complete(), "{source}");
          }
          let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
          assert_eq!(
            spans
              .iter()
              .find(|span| span.location.bytes.start == offset)
              .unwrap()
              .kind,
            if call.is_empty() {
              syntax_kind
            } else {
              SemanticKind::Function
            },
            "{source}"
          );
          let names =
            workspace.name_relations(&AtomicBool::new(false)).unwrap();
          let sites = names.at(snapshot, offset);
          if call.is_empty() {
            assert!(
              sites.iter().all(|(_, site)| site.context.is_none()
                && site.resolution.is_none()
                && site.identity.is_none()),
              "{source}"
            );
            assert_eq!(workspace.analysis().unwrap().contexts().len(), 1);
          } else {
            assert!(!sites.is_empty(), "{source}");
            assert!(
              sites.iter().all(|(_, site)| site.context.is_some()),
              "{source}"
            );
          }
        }
      }
    }
  }

  #[test]
  fn independent_command_facts_do_not_require_a_unique_called_definition() {
    let source = "f() { :; readonly x=one; cd; }; if condition; then g() { :; }; else g() { :; }; fi; g";
    let workspace = workspace(source);
    let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
    for (start, expected_kind, library) in [
      (source.find(':').unwrap(), SemanticKind::Command, true),
      (
        source.find("readonly").unwrap(),
        SemanticKind::Command,
        true,
      ),
      (source.find("cd").unwrap(), SemanticKind::Command, false),
      (source.rfind('g').unwrap(), SemanticKind::Function, false),
    ] {
      let span = spans
        .iter()
        .find(|span| span.location.bytes.start == start && span.semantic)
        .unwrap();
      assert_eq!(span.kind, expected_kind);
      assert_eq!(span.modifiers.contains(&Modifier::DefaultLibrary), library);
    }
  }

  #[test]
  fn command_names_are_classified_for_calls_and_queries_when_roles_agree() {
    for (source, expected) in [
      ("command :", true),
      ("command -p command :", true),
      ("command -v :", true),
      ("command() { :; }; command :", false),
      ("f() { \"$1\" :; }; f command; f printf", false),
    ] {
      let workspace = workspace(source);
      let start = source.rfind(':').unwrap();
      assert_eq!(
        workspace
          .semantic_spans(DocumentId(1))
          .unwrap()
          .iter()
          .any(|span| span.location.bytes.start == start
            && span.modifiers.contains(&Modifier::DefaultLibrary)),
        expected,
        "{source}"
      );
    }
    for (source, expected) in [
      ("f() { :; }; command -v f", SemanticKind::Function),
      ("f() { :; }; command -V f", SemanticKind::Function),
      ("f() { :; }; command f", SemanticKind::Command),
      ("f() { :; }; exec f", SemanticKind::Command),
      (
        "if condition; then f() { :; }; fi; command -v f",
        SemanticKind::Command,
      ),
    ] {
      let workspace = workspace(source);
      let start = source.rfind('f').unwrap();
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      let span = spans
        .iter()
        .find(|span| span.location.bytes.start == start && span.semantic)
        .unwrap();
      assert_eq!(span.kind, expected, "{source}");
      assert!(
        !span.modifiers.contains(&Modifier::DefaultLibrary),
        "{source}"
      );
    }
  }

  #[test]
  fn expansion_markers_are_excluded_but_parameter_bodies_are_preserved() {
    let source =
      ": $name ${name} \"$name\" \"${name}\" $1 ${1} $? ${?} $$ ${$}\n";
    let workspace = workspace(source);
    let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
    let parameters: Vec<_> = spans
      .iter()
      .filter(|span| {
        matches!(span.kind, SemanticKind::Variable | SemanticKind::Parameter)
      })
      .map(|span| (span.location.bytes.clone(), span.kind))
      .collect();
    assert_eq!(
      parameters,
      [
        (3..7, SemanticKind::Variable),
        (10..14, SemanticKind::Variable),
        (18..22, SemanticKind::Variable),
        (27..31, SemanticKind::Variable),
        (35..36, SemanticKind::Parameter),
        (39..40, SemanticKind::Parameter),
        (43..44, SemanticKind::Parameter),
        (47..48, SemanticKind::Parameter),
        (51..52, SemanticKind::Parameter),
        (55..56, SemanticKind::Parameter),
      ]
    );
  }

  #[test]
  fn numeric_operands_follow_builtin_roles_and_preserve_quotes_and_expansions()
  {
    for (source, expected) in [
      ("exit 7", vec!["7"]),
      ("exit 256", vec!["256"]),
      ("f() { return 255; }; f", vec!["255"]),
      ("set -- a b; shift 2", vec!["2"]),
      ("while condition; do break 2; done", vec!["2"]),
      ("while condition; do continue 1; done", vec!["1"]),
      ("command exit '7'", vec!["7"]),
      ("command -p command return \"12\"", vec!["12"]),
      ("exit '1'\"2\"", vec!["1", "2"]),
      ("exit -1", vec![]),
      ("exit 0x10", vec![]),
      ("exit 1+2", vec![]),
      ("exit ''", vec![]),
      ("exit 1 2", vec![]),
      ("shift 1 2", vec![]),
      ("echo 7; printf 7; f() { :; }; f 7", vec![]),
      ("status=7; exit \"$status\"", vec![]),
      ("exit \"$(external)\"", vec![]),
      ("exit \"$((7))\"", vec![]),
      ("cmd=exit; \"$cmd\" 7", vec!["7"]),
      (
        "if condition; then cmd=exit; else cmd=echo; fi; \"$cmd\" 7",
        vec![],
      ),
      ("f() { \"$1\" 7; }; f exit; f echo", vec![]),
      ("f() { \"$1\" 7; }; f exit; f shift", vec!["7"]),
    ] {
      let workspace = workspace(source);
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      let actual: Vec<_> = spans
        .iter()
        .filter(|span| span.semantic && span.kind == SemanticKind::Number)
        .map(|span| &source[span.location.bytes.clone()])
        .collect();
      assert_eq!(actual, expected, "{source}");
    }
  }

  #[test]
  fn named_operands_use_shared_roles_without_fabricating_definitions() {
    for (source, text, kind, modifiers) in [
      (
        "export name",
        "name",
        SemanticKind::Variable,
        vec![Modifier::Declaration],
      ),
      (
        "readonly 'name'",
        "name",
        SemanticKind::Variable,
        vec![Modifier::Declaration, Modifier::Readonly],
      ),
      (
        "readonly 'name=value'",
        "name",
        SemanticKind::Variable,
        vec![
          Modifier::Declaration,
          Modifier::Definition,
          Modifier::Modification,
          Modifier::Readonly,
        ],
      ),
      (
        "read name",
        "name",
        SemanticKind::Variable,
        vec![Modifier::Definition, Modifier::Modification],
      ),
      (
        "getopts ab name",
        "name",
        SemanticKind::Variable,
        vec![Modifier::Definition, Modifier::Modification],
      ),
      ("unset -v name", "name", SemanticKind::Variable, vec![]),
      ("unset -f name", "name", SemanticKind::Function, vec![]),
      (
        "readonly name; unset -v name",
        "name",
        SemanticKind::Variable,
        vec![Modifier::Readonly],
      ),
      (
        "command read name",
        "name",
        SemanticKind::Variable,
        vec![Modifier::Definition, Modifier::Modification],
      ),
    ] {
      let workspace = workspace(source);
      let start = source.rfind(text).unwrap();
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      let actual: Vec<_> = spans
        .iter()
        .filter(|span| {
          span.semantic && span.location.bytes == (start..start + text.len())
        })
        .map(|span| (span.kind, span.modifiers.clone()))
        .collect();
      assert_eq!(
        actual,
        [(kind, modifiers.into_iter().collect())],
        "{source}"
      );
    }
  }

  #[test]
  fn uncertain_or_shadowable_named_operands_keep_their_syntax_classification() {
    for source in [
      "f() { \"$1\" name; }; f read; f echo",
      "f() { \"$1\" name; }; f echo; f read",
      "read() { :; }; read name",
      "name=target; read \"$name\"",
      "unset -z name",
      "getopts ab 123",
      "f() { read name; }",
      "f() { getopts ab name; }",
      "f() { command read name; }",
      "f() { unset -z name; }",
      "f() { unset $options name; }",
    ] {
      let workspace = workspace(source);
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      assert!(
        !spans.iter().any(|span| span.semantic
          && span.kind == SemanticKind::Variable
          && matches!(&source[span.location.bytes.clone()], "target" | "123")),
        "{source}: {spans:?}"
      );
      if let Some(start) = source.find(" name") {
        assert!(
          !spans.iter().any(|span| span.semantic
            && span.location.bytes == (start + 1..start + 5)),
          "{source}: {spans:?}"
        );
      }
    }
  }

  #[test]
  fn exec_targets_are_external_commands_and_do_not_invoke_shell_functions() {
    for source in [
      "exec cat file",
      "exec -- 'cat' file",
      "command exec cat file",
      "cat() { hidden=1; }; exec cat file",
      "f() { exec cat file; }",
    ] {
      let workspace = workspace(source);
      let start = source.rfind("cat").unwrap();
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      let target = spans
        .iter()
        .find(|span| span.semantic && span.location.bytes == (start..start + 3))
        .unwrap();
      assert_eq!(target.kind, SemanticKind::Command, "{source}");
      assert_eq!(
        target.modifiers,
        std::collections::BTreeSet::new(),
        "{source}"
      );
    }
    for source in [
      "exec -z cat",
      "exec -a name cat",
      "read options; exec $options cat",
      "target=cat; exec \"$target\"",
      "f() { exec -z cat; }",
      "f() { exec $options cat; }",
    ] {
      let workspace = workspace(source);
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      assert!(
        !spans.iter().any(|span| span.semantic
          && span.kind == SemanticKind::Command
          && &source[span.location.bytes.clone()] == "cat"),
        "{source}"
      );
    }
  }

  #[test]
  fn invariant_named_operands_do_not_require_a_function_call() {
    for (source, text, kind, modifiers) in [
      (
        "f() { export name; }",
        "name",
        SemanticKind::Variable,
        vec![Modifier::Declaration],
      ),
      (
        "f() { readonly 'name'; }",
        "name",
        SemanticKind::Variable,
        vec![Modifier::Declaration, Modifier::Readonly],
      ),
      (
        "f() { readonly 'name=value'; }",
        "name",
        SemanticKind::Variable,
        vec![
          Modifier::Declaration,
          Modifier::Definition,
          Modifier::Modification,
          Modifier::Readonly,
        ],
      ),
      (
        "f() { unset -v name; }",
        "name",
        SemanticKind::Variable,
        vec![],
      ),
      (
        "f() { unset -f name; }",
        "name",
        SemanticKind::Function,
        vec![],
      ),
    ] {
      let workspace = workspace(source);
      let start = source.rfind(text).unwrap();
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      let actual: Vec<_> = spans
        .iter()
        .filter(|span| {
          span.semantic && span.location.bytes == (start..start + text.len())
        })
        .map(|span| (span.kind, span.modifiers.clone()))
        .collect();
      assert_eq!(
        actual,
        [(kind, modifiers.into_iter().collect())],
        "{source}"
      );
    }
  }

  #[test]
  fn invariant_numeric_arguments_are_classified_without_executing_function_bodies()
   {
    for source in [
      "f() { exit 7; }",
      "f() { return 7; }",
      "f() { shift 7; }",
      "f() { break 7; }",
      "f() { continue 7; }",
    ] {
      let workspace = workspace(source);
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      let start = source.find('7').unwrap();
      assert!(
        spans.iter().any(|span| span.semantic
          && span.kind == SemanticKind::Number
          && span.location.bytes == (start..start + 1)),
        "{source}: {spans:?}"
      );
      let document = workspace.document(DocumentId(1)).unwrap();
      assert!(
        document
          .argument_roles()
          .iter()
          .any(|role| role.context.is_none()
            && role.kind == Some(ArgumentKind::Number))
      );
      assert!(
        !workspace
          .analysis()
          .unwrap()
          .contexts()
          .iter()
          .any(|context| matches!(context.kind, ExecutionKind::Function(_)))
      );
    }
  }

  #[test]
  fn trap_actions_are_shell_bodies_only_when_no_option_may_precede_them() {
    for (action, body) in [
      ("\"echo $x\" INT", Some("\"echo $x\"")),
      ("-- \"-$x\" INT", Some("\"-$x\"")),
      ("\"-$x\" INT", None),
      ("\"$x\" INT", None),
    ] {
      let source = format!("x=$(external); trap {action}");
      let workspace = workspace(&source);
      let bodies: Vec<_> = workspace
        .analysis()
        .unwrap()
        .argument_roles()
        .filter(|role| role.kind == Some(ArgumentKind::ShellBody))
        .map(|role| &source[role.location.bytes.clone()])
        .collect();
      assert_eq!(bodies, Vec::from_iter(body), "{source}");
    }
  }

  #[test]
  fn declaration_argument_roles_never_color_assignment_values_as_variable_names()
   {
    for source in [
      "readonly name=value",
      "export name=value",
      "readonly name='value'",
      "readonly 'name=value'",
    ] {
      let workspace = workspace(source);
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      let name = source.find("name").unwrap();
      let variables: Vec<_> = spans
        .iter()
        .filter(|span| span.semantic && span.kind == SemanticKind::Variable)
        .map(|span| (span.location.bytes.start, span.location.bytes.end))
        .collect();
      assert_eq!(variables, [(name, name + 4)], "{source}");
    }
  }

  #[test]
  fn readonly_tracks_declaration_time_and_conditional_paths() {
    let source = "x=1; echo \"$x\"; readonly x; echo \"$x\"; readonly y=2; echo \"$y\"; if condition; then readonly z=3; fi; echo \"$z\"";
    let workspace = workspace(source);
    let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
    let variables: Vec<_> = spans
      .iter()
      .filter(|span| span.semantic && span.kind == SemanticKind::Variable)
      .collect();
    let states: Vec<_> = variables
      .iter()
      .map(|span| span.modifiers.contains(&Modifier::Readonly))
      .collect();
    assert_eq!(states, [false, false, true, true, true, true, true, false]);
    assert_eq!(
      variables.last().unwrap().readonly,
      AttributeState::Conditional
    );
  }

  #[test]
  fn one_occurrence_preserves_different_readonly_call_contexts() {
    let source = "x=1; f() { echo \"$x\"; }; f; readonly x; f";
    let workspace = workspace(source);
    let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
    let read = spans
      .iter()
      .find(|span| span.location.bytes.start == source.find("$x").unwrap() + 1)
      .unwrap();
    assert_eq!(read.readonly, AttributeState::Conditional);
    assert!(!read.modifiers.contains(&Modifier::Readonly));
    let occurrence = workspace
      .document(DocumentId(1))
      .unwrap()
      .occurrences()
      .iter()
      .find(|site| site.location == read.location)
      .unwrap();
    assert_eq!(
      workspace
        .analysis()
        .unwrap()
        .readonly_contexts(occurrence.id)
        .count(),
      2
    );
  }

  #[test]
  fn clearing_analysis_discards_projected_argument_claims() {
    let mut workspace = workspace("read x");
    assert!(workspace.semantic_spans(DocumentId(1)).unwrap().iter().any(
      |span| {
        span.location.bytes == (5..6)
          && span.modifiers.contains(&Modifier::Definition)
      }
    ));
    workspace.clear_analysis();
    assert!(
      !workspace
        .semantic_spans(DocumentId(1))
        .unwrap()
        .iter()
        .any(|span| {
          span.location.bytes == (5..6)
            && span.modifiers.contains(&Modifier::Definition)
        })
    );
  }

  #[test]
  fn document_updates_discard_old_semantic_claims_but_keep_syntax() {
    let mut workspace = workspace("f() { :; }; f; readonly x=1; echo \"$x\"");
    let source = "# comment\nf() { :; }; f\n";
    workspace.insert(support::parse(1, 2, source)).unwrap();
    let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
    assert!(spans.iter().any(|span| span.kind == SemanticKind::Comment));
    assert_eq!(
      spans
        .iter()
        .filter(|span| span.kind == SemanticKind::Function)
        .count(),
      1
    );
    assert_eq!(
      spans
        .iter()
        .filter(|span| span.modifiers.contains(&Modifier::DefaultLibrary))
        .map(|span| &source[span.location.bytes.clone()])
        .collect::<Vec<_>>(),
      [":"]
    );
  }

  #[test]
  fn syntax_resolutions_do_not_depend_on_earlier_queries() {
    let source = "f() { :; }\nf\n";
    let document = support::parse(1, 1, source);
    let snapshot = document.snapshot();
    let mut workspace = Workspace::default();
    workspace.insert(document).unwrap();
    let at = source.rfind('f').unwrap();
    let before = workspace.resolution_at(snapshot, at).unwrap();
    workspace.semantic_spans(DocumentId(1)).unwrap();
    let after = workspace.resolution_at(snapshot, at).unwrap();
    for resolution in [before, after] {
      assert_eq!(resolution, None);
    }
    let mut request = AnalysisRequest::new(snapshot);
    request.initial = InitialEnvironment::empty();
    let analysis = workspace
      .analyze(
        request,
        &mut support::Inputs::default(),
        &AtomicBool::new(false),
      )
      .unwrap();
    workspace.publish_analysis(analysis).unwrap();
    assert_eq!(
      workspace
        .resolution_at(snapshot, at)
        .unwrap()
        .unwrap()
        .candidates,
      workspace
        .symbols_named(b"f")
        .map(|symbol| symbol.id)
        .collect()
    );
  }

  #[test]
  fn recovery_preserves_prior_calls_and_independent_command_facts() {
    let source = "before() { :; }; before; :; )\nafter() { :; }; after\n";
    let document = support::parse(1, 1, source);
    assert!(document.has_syntax_errors());
    let mut request = AnalysisRequest::new(document.snapshot());
    request.initial = InitialEnvironment::empty();
    let mut workspace = Workspace::default();
    workspace.insert(document).unwrap();
    let analysis = workspace
      .analyze(
        request,
        &mut support::Inputs::default(),
        &AtomicBool::new(false),
      )
      .unwrap();
    assert!(analysis.gaps().iter().any(|gap| {
      gap.issue
        == shellsmith::interface::application::engine::ResolutionIssue::Recovery
    }));
    workspace.publish_analysis(analysis).unwrap();
    let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
    for (name, kind) in [
      ("before", SemanticKind::Function),
      ("after", SemanticKind::Function),
    ] {
      let start = source.rfind(name).unwrap();
      let span = spans
        .iter()
        .find(|span| span.location.bytes.start == start && span.semantic)
        .unwrap();
      assert_eq!(span.kind, kind, "{name}");
    }
    let builtins: Vec<_> = spans
      .iter()
      .filter(|span| span.modifiers.contains(&Modifier::DefaultLibrary))
      .map(|span| span.location.bytes.start)
      .collect();
    assert_eq!(
      builtins,
      source
        .match_indices(':')
        .map(|(start, _)| start)
        .collect::<Vec<_>>()
    );
  }

  #[test]
  fn excluded_eval_alias_and_trap_effects_keep_symbols_and_semantic_modifiers()
  {
    for call in [
      "eval 'hidden=1; f() { :; }; . ./hidden.sh; trap exit INT'",
      "eval \"$(brew shellenv)\"",
      "command eval 'unset -f f; unset -v kept'",
      "cmd=eval; \"$cmd\" 'unset -f f'",
      "alias f='other'; unalias f",
      "trap 'hidden=1; unset -f f; unset -v kept; . ./hidden.sh' INT EXIT",
      "command trap \"$unknown\" INT",
    ] {
      let source =
        format!("readonly kept=value; f() {{ :; }}; {call}; f; : \"$kept\"");
      let workspace = workspace(&source);
      let analysis = workspace.analysis().unwrap();
      assert_eq!(
        analysis
          .source_inputs()
          .filter(|(_, context, _)| context
            .is_some_and(|context| !support::deferred(analysis, context)))
          .count(),
        0,
        "{call}"
      );
      assert!(!analysis.gaps().is_empty(), "{call}");
      assert!(
        analysis
          .gaps()
          .iter()
          .filter(|gap| !support::deferred(analysis, gap.context))
          .all(|gap| gap.issue == shellsmith::interface::application::engine::ResolutionIssue::ExcludedEffect),
        "{call}"
      );
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      for (text, kind, modifier) in [
        ("f; :", SemanticKind::Function, None),
        (
          ": \"$kept\"",
          SemanticKind::Command,
          Some(Modifier::DefaultLibrary),
        ),
        ("kept\"", SemanticKind::Variable, Some(Modifier::Readonly)),
      ] {
        let start = source.rfind(text).unwrap();
        let span = spans
          .iter()
          .find(|span| span.location.bytes.start == start && span.semantic)
          .unwrap();
        assert_eq!(span.kind, kind, "{call}: {text}");
        if let Some(modifier) = modifier {
          assert!(span.modifiers.contains(&modifier), "{call}: {text}");
        }
      }
    }
  }

  #[test]
  fn trap_bodies_classify_syntax_and_preserve_registration_time_expansions() {
    let source = "action='x=changed; : \"$x\"'; trap 'hidden() { :; }; x=1' INT; trap \"$action\" EXIT";
    let (mut workspace, analysis) = support::analyze_with(
      source,
      InitialEnvironment {
        other_variables:
          shellsmith::interface::application::engine::InitialBinding::Unknown,
        ..InitialEnvironment::empty()
      },
    );
    let symbol = workspace.symbols_named(b"action").next().unwrap().id;
    assert_eq!(analysis.references(symbol).count(), 1);
    assert_eq!(analysis.contexts().len(), 3);
    let generated = workspace
      .documents()
      .find(|document| document.source() == "x=changed; : \"$x\"")
      .unwrap();
    let resolutions: Vec<_> = analysis
      .document_resolutions(generated.snapshot())
      .filter(|site| site.site.syntax().is_some())
      .collect();
    assert_eq!(resolutions.len(), 2);
    assert!(resolutions.iter().all(|site| site.resolution.is_some()));
    workspace.publish_analysis(analysis).unwrap();
    let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
    for (text, width, kind, modifier) in [
      ("hidden", 6, SemanticKind::Function, Modifier::Definition),
      ("x=1", 1, SemanticKind::Variable, Modifier::Modification),
    ] {
      let start = source.find(text).unwrap();
      assert!(
        spans
          .iter()
          .any(|span| span.location.bytes == (start..start + width)
            && span.kind == kind
            && span.modifiers.contains(&modifier)),
        "{text}"
      );
    }
    let variable = source.rfind("action").unwrap();
    assert!(
      spans
        .iter()
        .any(|span| span.location.bytes == (variable..variable + 6)
          && span.kind == SemanticKind::Variable)
    );
    for (start, _) in source.match_indices("trap") {
      assert!(
        spans
          .iter()
          .any(|span| span.location.bytes == (start..start + 4)
            && span.kind == SemanticKind::Command
            && span.modifiers.contains(&Modifier::DefaultLibrary))
      );
    }
  }

  #[test]
  fn trap_tokens_keep_invariant_attributes_but_not_redefined_functions() {
    for call in [
      r#"trap 'f; : "$late"; exit 1' EXIT"#,
      r#"trap -- 'f; : "$late"; exit 1' INT"#,
      r#"command -p trap 'f; : "$late"; exit 1' TERM"#,
      r#"cmd=trap; "$cmd" 'f; : "$late"; exit 1' EXIT"#,
      r#"idle() { trap 'f; : "$late"; exit 1' EXIT; }"#,
      r#"trap "f; : \"\$late\"; exit 1" EXIT"#,
      r#"trap 'trap '\''f; : "$late"; exit 1'\'' EXIT' TERM"#,
    ] {
      let source =
        format!("readonly late=value; f() {{ :; }}; {call}; f() {{ :; }}; f");
      let workspace = workspace(&source);
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      for (text, length, kind, modifiers) in [
        ("f; :", 1, SemanticKind::Command, vec![]),
        (
          "late",
          4,
          SemanticKind::Variable,
          if call.starts_with("idle()") {
            vec![]
          } else {
            vec![Modifier::Readonly]
          },
        ),
        (
          "exit 1",
          4,
          SemanticKind::Command,
          vec![Modifier::DefaultLibrary],
        ),
        ("1", 1, SemanticKind::Number, vec![]),
      ] {
        let start = source.find(call).unwrap() + call.find(text).unwrap();
        let span = spans
          .iter()
          .find(|span| {
            span.semantic && span.location.bytes == (start..start + length)
          })
          .unwrap_or_else(|| panic!("{call}: {text}: {spans:?}"));
        assert_eq!(span.kind, kind, "{call}: {text}");
        assert_eq!(
          span.modifiers,
          modifiers.into_iter().collect(),
          "{call}: {text}"
        );
      }
      let snapshot = workspace.document(DocumentId(1)).unwrap().snapshot();
      let at = source.find(call).unwrap() + call.find("f; :").unwrap();
      assert!(
        workspace
          .resolution_at(snapshot, at)
          .unwrap()
          .is_none_or(|resolution| !resolution.is_complete()),
        "{call}"
      );
      let analysis = workspace.analysis().unwrap();
      assert_eq!(
        analysis
          .contexts()
          .iter()
          .filter(|context| !support::deferred(analysis, context.id))
          .count(),
        2,
        "{call}"
      );
    }
  }

  #[test]
  fn trap_body_variants_require_agreement_and_updates_remove_old_bodies() {
    for source in [
      "register() { trap \"$prefix\"'f' EXIT; }; prefix=''; register; read prefix; register",
      "register() { trap \"$prefix\"'f' EXIT; }; prefix=''; register; prefix='#'; register",
      "register() { trap \"$prefix\"'f' EXIT; }; prefix=''; register; prefix='-'; register",
      "register() { \"$cmd\" 'f' EXIT; }; cmd=trap; register; cmd=echo; register",
      r#"register() { trap "$prefix""trap 'f' EXIT" INT; }; prefix=''; register; prefix='#'; register"#,
      r#"register() { trap "$prefix""trap 'f' EXIT" INT; }; prefix=''; register; read prefix; register"#,
    ] {
      let workspace = workspace(source);
      let start = source.find("'f'").unwrap() + 1;
      assert!(
        !workspace.semantic_spans(DocumentId(1)).unwrap().iter().any(
          |span| span.semantic && span.location.bytes == (start..start + 1)
        ),
        "{source}"
      );
    }
    let mut workspace = workspace("trap 'old_name() { :; }' EXIT");
    let snapshot = support::parse(1, 2, "trap 'new_name() { :; }' EXIT");
    let mut request = AnalysisRequest::new(snapshot.snapshot());
    request.initial = InitialEnvironment::empty();
    workspace.insert(snapshot).unwrap();
    let analysis = workspace
      .analyze(
        request,
        &mut support::Inputs::default(),
        &AtomicBool::new(false),
      )
      .unwrap();
    workspace.publish_analysis(analysis).unwrap();
    assert_eq!(workspace.documents().count(), 2);
    assert_eq!(workspace.symbols_named(b"old_name").count(), 0);
    assert_eq!(workspace.symbols_named(b"new_name").count(), 1);
  }

  #[test]
  fn recursive_calls_preserve_independently_resolved_semantic_information() {
    let workspace =
      workspace("# comment\nf() { f; }; f; readonly x=1; echo \"$x\"");
    assert!(workspace.analysis().unwrap().has_recursive_calls());
    let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
    assert!(spans.iter().any(|span| span.kind == SemanticKind::Comment));
    assert_eq!(
      spans
        .iter()
        .filter(|span| span.kind == SemanticKind::Function)
        .count(),
      3
    );
    assert_eq!(
      spans
        .iter()
        .filter(|span| span.modifiers.contains(&Modifier::Readonly))
        .count(),
      2
    );
    assert!(
      spans
        .iter()
        .any(|span| span.modifiers.contains(&Modifier::DefaultLibrary))
    );
  }

  #[test]
  fn expanded_arithmetic_preserves_readonly_only_with_complete_source_roles() {
    for (source, selected, expected) in [
      ("readonly x=1; n=2; : $((x+$n))", "x+$n", true),
      ("readonly x=1; f() { : $((x+$1)); }; f 1; f 2", "x+$1", true),
      (
        "readonly x=1; f() { : $((x $1 2)); }; f +; read op; f \"$op\"",
        "x $1",
        false,
      ),
      (
        "readonly x=1; xy=2; f() { : $((x$1)); }; f ''; f y",
        "x$1",
        false,
      ),
    ] {
      let workspace = workspace(source);
      let start = source.find(selected).unwrap();
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      assert_eq!(
        spans
          .iter()
          .any(|span| span.location.bytes == (start..start + 1)
            && span.kind == SemanticKind::Variable
            && span.modifiers.contains(&Modifier::Readonly)),
        expected,
        "{source}"
      );
    }
  }

  #[test]
  fn utility_write_tokens_keep_readonly_and_only_project_spelled_variable_names()
   {
    for source in ["readonly x; read x", "readonly 'x=value'"] {
      let workspace = workspace(source);
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      let start = source.rfind('x').unwrap();
      assert!(
        spans
          .iter()
          .any(|span| span.location.bytes == (start..start + 1)
            && span.kind == SemanticKind::Variable
            && span.modifiers.contains(&Modifier::Readonly)),
        "{source}: {spans:?}"
      );
    }
    let workspace = workspace("name=x; read \"$name\"");
    assert_eq!(
      workspace
        .projected_symbols(DocumentId(1))
        .filter(|(symbol, _)| symbol.name == b"x")
        .count(),
      0
    );
  }
}

mod environment {
  use crate::support;
  use shellsmith::interface::application::engine::{
    Access, AnalysisRequest, DocumentId, InitialEnvironment, Modifier, Value,
    Workspace,
  };
  use std::sync::atomic::AtomicBool;
  fn environment_modifiers(source: &str) -> Vec<bool> {
    let document = support::parse(1, 1, source);
    assert!(!document.has_syntax_errors(), "{source}");
    let mut request = AnalysisRequest::new(document.snapshot());
    request.initial = InitialEnvironment::empty();
    request
      .initial
      .environment
      .insert(b"HOME".to_vec(), b"/initial".to_vec());
    let mut workspace = Workspace::default();
    workspace.insert(document).unwrap();
    let analysis = workspace
      .analyze(
        request,
        &mut support::Inputs::default(),
        &AtomicBool::new(false),
      )
      .unwrap();
    workspace.publish_analysis(analysis).unwrap();
    let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
    workspace
      .document(DocumentId(1))
      .unwrap()
      .occurrences()
      .iter()
      .filter(|site| {
        site.name == Value::Literal(b"HOME".to_vec())
          && site.access == Access::Read
      })
      .map(|site| {
        spans
          .iter()
          .find(|span| span.location == site.location)
          .unwrap()
          .modifiers
          .contains(&Modifier::Environment)
      })
      .collect()
  }
  #[test]
  fn environment_modifier_tracks_origin_instead_of_names_or_export_attributes()
  {
    for (source, expected) in [
      (": \"$HOME\"; HOME=/other; : \"${HOME}\"", vec![true, false]),
      (
        "export HOME; : \"$HOME\"; unset HOME; : \"$HOME\"",
        vec![true, false],
      ),
      ("readonly HOME; : \"$HOME\"", vec![true]),
      ("set -eu; false; : \"$HOME\"", vec![true]),
      ("set +eu; exit; : \"$HOME\"", vec![true]),
      ("set -o nounset; exec replacement; : \"$HOME\"", vec![true]),
      (
        "if condition; then HOME=/other; fi; : \"$HOME\"",
        vec![false],
      ),
      ("(HOME=/other); : \"$HOME\"", vec![true]),
      ("HOME=/other echo argument; : \"$HOME\"", vec![true]),
      ("read HOME; : \"$HOME\"", vec![false]),
      ("eval \"$(unknown)\"; : \"$HOME\"", vec![true]),
      ("eval 'HOME=/other'; : \"$HOME\"", vec![true]),
      ("alias next='HOME=/other'; next; : \"$HOME\"", vec![true]),
      (". unknown; : \"$HOME\"", vec![false]),
      ("f() { : \"$HOME\"; }; f; HOME=/other; f", vec![false]),
      ("f() { HOME=/other; }; f; : \"$HOME\"", vec![false]),
      (
        ": \"$HOME\"; for HOME in /other; do :; done; : \"$HOME\"",
        vec![true, false],
      ),
    ] {
      assert_eq!(environment_modifiers(source), expected, "{source}");
    }
  }
}
