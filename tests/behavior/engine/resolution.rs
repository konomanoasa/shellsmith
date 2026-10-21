mod bindings {
  use crate::support;

  use shellsmith::interface::application::engine::{
    Access, Analysis, AnalysisRequest, OccurrenceKind, ResolvedUse, Value,
    Workspace,
  };
  use shellsmith::interface::application::engine::{
    Alternative, InitialBinding, InitialEnvironment, Resolution,
    ResolutionIssue, ResolutionStatus,
  };
  use shellsmith::interface::application::engine::{
    DependencyState, DocumentId, Error, ExecutionKind,
  };
  use std::sync::atomic::AtomicBool;

  use support::analyze;

  fn reads<'a>(
    workspace: &Workspace,
    analysis: &'a Analysis,
    name: &[u8],
  ) -> Vec<&'a ResolvedUse> {
    analysis
      .uses()
      .filter(|site| {
        let Some(occurrence) =
          site.site.syntax().and_then(|id| workspace.occurrence(id))
        else {
          return false;
        };
        occurrence.kind != OccurrenceKind::Command
          && !matches!(occurrence.access, Access::Definition | Access::Write)
          && occurrence.name == Value::Literal(name.to_vec())
      })
      .collect()
  }

  #[test]
  fn named_arguments_share_reverse_references_without_turning_queries_into_calls()
   {
    let (workspace, analysis) = analyze(
      "x=1; export x; readonly x; unset -v x; f() { x=2; }; command -v f; command -V f; unset -f f; : \"$x\"",
    );
    let x = workspace.symbols_named(b"x").next().unwrap().id;
    let f = workspace.symbols_named(b"f").next().unwrap().id;
    let variable_references: Vec<_> = analysis.references(x).collect();
    assert_eq!(variable_references.len(), 4);
    assert_eq!(
      variable_references
        .iter()
        .filter(|site| site.site.syntax().is_none())
        .count(),
      3
    );
    assert_eq!(analysis.references(f).count(), 3);
    assert_eq!(analysis.contexts().len(), 1);
    let read = reads(&workspace, &analysis, b"x")[0];
    assert_eq!(
      analysis.value(read.site.syntax().unwrap(), read.context),
      Some(&Value::Literal(b"1".to_vec()))
    );
    assert!(
      analysis
        .references(f)
        .all(|site| site.site.syntax().is_none())
    );
  }

  #[test]
  fn sequential_reads_and_assignment_rhs_use_the_preceding_definition() {
    let (workspace, analysis) = analyze("x=1; : \"$x\"; x=$x; : \"$x\"");
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let statuses: Vec<_> = reads(&workspace, &analysis, b"x")
      .iter()
      .map(|site| site.resolution.status())
      .collect();
    assert_eq!(
      statuses,
      vec![
        ResolutionStatus::Resolved(ids[0]),
        ResolutionStatus::Resolved(ids[0]),
        ResolutionStatus::Resolved(ids[1])
      ]
    );
    assert_eq!(analysis.references(ids[0]).count(), 2);
    assert_eq!(analysis.references(ids[1]).count(), 1);
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn trap_calls_start_a_new_call_stack_and_stop_cyclic_registrations() {
    let source =
      "x=1; register() { trap 'register; : \"$x\"' EXIT; }; register";
    let (workspace, analysis) = analyze(source);
    let function = workspace.symbols_named(b"register").next().unwrap().id;
    assert_eq!(
      analysis
        .contexts()
        .iter()
        .filter(|context| context.kind == ExecutionKind::Function(function))
        .count(),
      2
    );
    assert_eq!(
      analysis
        .contexts()
        .iter()
        .filter(|context| matches!(context.kind, ExecutionKind::Deferred(_)))
        .count(),
      1
    );
    assert_eq!(analysis.unknown_effects().count(), 0);
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(uses.len(), 1);
    assert!(uses[0].resolution.is_complete());
    assert_eq!(
      analysis.value(uses[0].site.syntax().unwrap(), uses[0].context),
      Some(&Value::Literal(b"1".to_vec()))
    );
    let recursion: Vec<_> = analysis
      .gaps()
      .iter()
      .filter(|gap| gap.issue == ResolutionIssue::Recursion)
      .collect();
    assert_eq!(recursion.len(), 1);
    assert_eq!(recursion[0].location.snapshot.document, DocumentId(1));
    assert_eq!(
      &source[recursion[0].location.bytes.clone()],
      "trap 'register; : \"$x\"' EXIT"
    );
  }

  #[test]
  fn nested_traps_keep_distinct_registrations_in_generated_bodies() {
    for source in [
      "f() { :; }; trap \"trap 'f' EXIT\" TERM",
      "f() { :; }; action=\"trap 'f' EXIT\"; trap \"$action\" TERM",
      "f() { :; }; action=f; trap 'trap \"$action\" EXIT' TERM",
      "f() { :; }; next=f; action='trap \"$next\" EXIT'; trap \"$action\" TERM",
    ] {
      let (workspace, analysis) = analyze(source);
      let function = workspace.symbols_named(b"f").next().unwrap().id;
      assert_eq!(analysis.references(function).count(), 1, "{source}");
      assert_eq!(
        analysis
          .contexts()
          .iter()
          .filter(|context| matches!(context.kind, ExecutionKind::Deferred(_)))
          .count(),
        2,
        "{source}"
      );
      assert!(
        analysis
          .gaps()
          .iter()
          .all(|gap| gap.issue != ResolutionIssue::Recursion),
        "{source}"
      );
    }
  }

  #[test]
  fn trap_invariants_keep_unrelated_names_outside_patterned_changes() {
    for (variable, function, known_variable, known_function) in [
      ("other_", "other_", true, true),
      ("x", "other_", false, true),
      ("other_", "util", true, false),
      ("x", "util", false, false),
    ] {
      let source = format!(
        "trap 'utility' INT; trap '. \"${{x:-./lib.sh}}\"' EXIT; read suffix; read \"{variable}$suffix\"; unset -f \"{function}$suffix\""
      );
      let (workspace, analysis) = analyze(&source);
      let inputs: Vec<_> = analysis.source_inputs().collect();
      assert_eq!(inputs.len(), 1, "{source}");
      assert_eq!(
        inputs[0].2.operand,
        if known_variable {
          Value::Literal(b"./lib.sh".to_vec())
        } else {
          Value::Unknown(
            shellsmith::interface::application::engine::ValueIssue::Dynamic,
          )
        },
        "{source}"
      );
      let utility = analysis
        .uses()
        .find(|site| {
          site
            .site
            .syntax()
            .and_then(|id| workspace.occurrence(id))
            .is_some_and(|site| {
              site.name == Value::Literal(b"utility".to_vec())
            })
        })
        .unwrap();
      assert_eq!(
        utility.resolution.issues.is_empty(),
        known_function,
        "{source}"
      );
      if known_function {
        assert_eq!(
          utility.resolution.alternatives,
          [Alternative::ExternalCommand].into()
        );
      }
    }
  }

  #[test]
  fn trap_path_inputs_separate_working_directory_from_directory_variables() {
    for change in [
      "PWD=/elsewhere",
      "OLDPWD=/elsewhere",
      "read PWD",
      "unset PWD",
    ] {
      for (body, known) in
        [(". ./lib.sh", true), ("cd ./next; . ./lib.sh", false)]
      {
        let source = format!("trap '{body}' EXIT; {change}");
        let (_, analysis) = support::analyze_with(
          &source,
          InitialEnvironment {
            cwd: Some(b"/workspace".to_vec()),
            ..InitialEnvironment::empty()
          },
        );
        let inputs: Vec<_> = analysis.source_inputs().collect();
        assert_eq!(inputs.len(), 1, "{source}");
        assert_eq!(
          inputs[0].2.operand,
          Value::Literal(b"./lib.sh".to_vec()),
          "{source}"
        );
        assert_eq!(
          inputs[0].2.cwd,
          Some(if known {
            Value::Literal(b"/workspace".to_vec())
          } else {
            Value::Unknown(
              shellsmith::interface::application::engine::ValueIssue::Dynamic,
            )
          }),
          "{source}"
        );
      }
    }
  }

  #[test]
  fn branches_keep_both_definitions_and_an_unwritten_path_keeps_unset() {
    let (workspace, analysis) = analyze(
      "if condition; then x=1; else x=2; fi; : \"$x\"; if condition; then y=1; fi; : \"$y\"",
    );
    let x = reads(&workspace, &analysis, b"x")[0];
    assert_eq!(
      x.resolution.candidates,
      workspace
        .symbols_named(b"x")
        .map(|symbol| symbol.id)
        .collect()
    );
    assert!(x.resolution.alternatives.is_empty());
    assert!(x.resolution.is_complete());
    let y = reads(&workspace, &analysis, b"y")[0];
    assert_eq!(y.resolution.candidates.len(), 1);
    assert_eq!(y.resolution.alternatives, [Alternative::Unset].into());
  }

  #[test]
  fn function_reads_are_resolved_in_each_calling_environment() {
    let (workspace, analysis) = analyze("f() { : \"$x\"; }; x=1; f; x=2; f");
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(uses.len(), 2);
    assert_ne!(uses[0].context, uses[1].context);
    assert_eq!(
      uses[0].resolution.status(),
      ResolutionStatus::Resolved(ids[0])
    );
    assert_eq!(
      uses[1].resolution.status(),
      ResolutionStatus::Resolved(ids[1])
    );
    let function = workspace.symbols_named(b"f").next().unwrap().id;
    let calls: Vec<_> = analysis.references(function).collect();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(
      |site| site.resolution.status() == ResolutionStatus::Resolved(function)
    ));
  }

  #[test]
  fn conditional_assignments_and_scoped_unknown_effects_recover_precision() {
    let (workspace, analysis) = analyze(
      "x=1; : ${x=2}; : \"$x\"; : ${y=1}; : \"$y\"; read x; : \"$x\"; x=3; : \"$x\"",
    );
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(
      uses[0].resolution.status(),
      ResolutionStatus::Resolved(ids[0])
    );
    assert_eq!(
      uses[1].resolution.status(),
      ResolutionStatus::Resolved(ids[0])
    );
    let read_write = workspace
      .symbols_named(b"x")
      .find(|symbol| {
        symbol.id.snapshot.document
          != shellsmith::interface::application::engine::DocumentId(1)
      })
      .unwrap()
      .id;
    assert_eq!(
      uses[2].resolution.status(),
      ResolutionStatus::Resolved(read_write)
    );
    assert!(matches!(
      analysis.value(uses[2].site.syntax().unwrap(), uses[2].context),
      Some(Value::Unknown(_))
    ));
    assert_eq!(
      uses[3].resolution.status(),
      ResolutionStatus::Resolved(ids[2])
    );
    let y = workspace.symbols_named(b"y").next().unwrap().id;
    assert_eq!(
      reads(&workspace, &analysis, b"y")[1].resolution.status(),
      ResolutionStatus::Resolved(y)
    );
  }

  #[test]
  fn temporary_assignments_command_suppression_and_unset_keep_namespaces_separate()
   {
    let (workspace, analysis) = analyze(
      "x=1; x=2 printf '%s' \"$x\"; : \"$x\"; x=3 :; : \"$x\"; f() { x=4; }; command f; : \"$x\"; unset -v x; : \"$x\"; unset -f f; f",
    );
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let statuses: Vec<_> = reads(&workspace, &analysis, b"x")
      .iter()
      .map(|site| site.resolution.status())
      .collect();
    assert_eq!(
      statuses,
      vec![
        ResolutionStatus::Resolved(ids[0]),
        ResolutionStatus::Resolved(ids[0]),
        ResolutionStatus::Resolved(ids[2]),
        ResolutionStatus::Resolved(ids[2]),
        ResolutionStatus::Unresolved
      ]
    );
    let f = workspace.symbols_named(b"f").next().unwrap().id;
    assert_eq!(analysis.references(f).count(), 1);
  }

  #[test]
  fn function_definitions_are_not_hoisted_and_command_queries_do_not_invoke_them()
   {
    let (workspace, analysis) = analyze(
      "f; f() { x=1; }; command -pv f; command -V f; command command f; : \"$x\"; f; : \"$x\"",
    );
    let f = workspace.symbols_named(b"f").next().unwrap().id;
    assert_eq!(analysis.references(f).count(), 3);
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(uses[0].resolution.alternatives, [Alternative::Unset].into());
    let x = workspace.symbols_named(b"x").next().unwrap().id;
    assert_eq!(uses[1].resolution.status(), ResolutionStatus::Resolved(x));
  }

  #[test]
  fn special_builtins_precede_functions_and_function_prefixes_have_uncertain_lifetime()
   {
    let (workspace, analysis) = analyze(
      "readonly() { x=9; }; readonly x=1; f() { : \"$x\"; }; x=2 f; : \"$x\"",
    );
    let function = workspace.symbols_named(b"readonly").next().unwrap().id;
    assert_eq!(analysis.references(function).count(), 0);
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    for site in reads(&workspace, &analysis, b"x") {
      assert!(!site.resolution.candidates.contains(&ids[0]));
      assert!(site.resolution.is_complete());
    }
    let (workspace, analysis) =
      analyze("x=0; f() { : \"$x\"; }; x=1 f; : \"$x\"");
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(
      uses[0].resolution.status(),
      ResolutionStatus::Resolved(ids[1])
    );
    assert_eq!(uses[1].resolution.candidates, ids.into_iter().collect());
    assert!(
      uses[1]
        .resolution
        .issues
        .contains(&ResolutionIssue::EvaluationOrder)
    );
  }

  #[test]
  fn unmodelled_builtins_and_unspecified_utility_names_do_not_hide_effects() {
    for source in [
      "PWD=old; cd somewhere; : \"$PWD\"",
      "x=0; local x=1; : \"$x\"",
      "x=0; [[ -n x ]]; : \"$x\"",
      "x=0; if select; then :; fi; : \"$x\"",
      "x=0; label: operand; : \"$x\"",
      "x=0; time() { :; }; time; : \"$x\"",
      "x=0; command \"$(unknown)\"; x=1; : \"$x\"",
      "x=0; fc -s; x=1; : \"$x\"",
      "x=0; command -v -x g; : \"$x\"",
      "x=0; f() { if c; then f; : \"$x\"; fi; command -v -x g; }; f",
    ] {
      let (workspace, analysis) = analyze(source);
      let name = if source.starts_with("PWD") {
        b"PWD".as_slice()
      } else {
        b"x".as_slice()
      };
      assert!(
        !reads(&workspace, &analysis, name)
          .last()
          .unwrap()
          .resolution
          .is_complete(),
        "{source}"
      );
    }
  }

  #[test]
  fn excluded_effects_preserve_definitions_and_allow_later_writes() {
    for call in [
      "eval 'x=body'",
      "eval \"$(unknown)\"",
      "alias next='x=body'; next",
    ] {
      let source = format!("x=before; {call}; : \"$x\"; x=after; : \"$x\"");
      let (workspace, analysis) = analyze(&source);
      let definitions: Vec<_> =
        workspace.symbols_named(b"x").map(|s| s.id).collect();
      let resolved: Vec<_> = reads(&workspace, &analysis, b"x")
        .iter()
        .map(|site| site.resolution.status())
        .collect();
      assert_eq!(
        resolved,
        [
          ResolutionStatus::Resolved(definitions[0]),
          ResolutionStatus::Resolved(definitions[1])
        ],
        "{call}"
      );
    }
  }

  #[test]
  fn regular_utility_implementations_remain_distinct_from_intrinsic_builtins() {
    let (_, analysis) = analyze("echo; tool; command");
    assert_eq!(
      analysis
        .uses()
        .map(|site| site.resolution.alternatives.clone())
        .collect::<Vec<_>>(),
      vec![
        [Alternative::Builtin, Alternative::ExternalCommand].into(),
        [Alternative::ExternalCommand].into(),
        [Alternative::Builtin].into(),
      ]
    );
  }

  #[test]
  fn unordered_variable_writes_leave_special_and_positional_parameters_ordered()
  {
    let parameter = Resolution {
      alternatives: [Alternative::SpecialParameter].into(),
      ..Resolution::default()
    };
    let variable = Resolution {
      alternatives: [Alternative::Unset].into(),
      issues: [ResolutionIssue::EvaluationOrder].into(),
      ..Resolution::default()
    };
    for (name, expected) in [
      ("1", &parameter),
      ("?", &parameter),
      ("#", &parameter),
      ("x", &variable),
    ] {
      let source = format!("set -- p; n=y; v=$(( $n = 1 )) : >\"${name}\"");
      let (workspace, analysis) = analyze(&source);
      let resolutions: Vec<_> = reads(&workspace, &analysis, name.as_bytes())
        .iter()
        .map(|site| &site.resolution)
        .collect();
      assert_eq!(resolutions, [expected], "{source}");
    }
  }

  #[test]
  fn redirections_without_commands_are_isolated_and_special_order_is_uncertain()
  {
    let (workspace, analysis) =
      analyze("x=0; >$((x=1)); : \"$x\"; x=2 : >\"$x\"");
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(
      uses[0].resolution.status(),
      ResolutionStatus::Resolved(ids[0])
    );
    assert!(
      uses[1]
        .resolution
        .issues
        .contains(&ResolutionIssue::EvaluationOrder)
    );
    let (workspace, analysis) =
      analyze("x=0; x=3 external >$((x=2)); : \"$x\"");
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    assert_eq!(
      reads(&workspace, &analysis, b"x")[0].resolution.status(),
      ResolutionStatus::Resolved(ids[2])
    );
  }

  #[test]
  fn subshell_redirection_expansions_do_not_write_to_the_calling_environment() {
    let (workspace, analysis) = analyze(
      "x=0; (: \"$x\") >$((x=1)); : \"$x\"; f() (: \"$x\") >$((x=2)); f; : \"$x\"",
    );
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    assert_eq!(
      reads(&workspace, &analysis, b"x")
        .iter()
        .map(|site| site.resolution.status())
        .collect::<Vec<_>>(),
      vec![
        ResolutionStatus::Resolved(ids[1]),
        ResolutionStatus::Resolved(ids[0]),
        ResolutionStatus::Resolved(ids[2]),
        ResolutionStatus::Resolved(ids[0])
      ]
    );
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn readonly_bindings_are_not_removed_by_unset() {
    let (workspace, analysis) = analyze("readonly x=1; unset -v x; : \"$x\"");
    let x = workspace.symbols_named(b"x").next().unwrap().id;
    assert_eq!(
      reads(&workspace, &analysis, b"x")[0].resolution.status(),
      ResolutionStatus::Resolved(x)
    );
  }

  #[test]
  fn conditional_readonly_preserves_both_possible_assignment_and_unset_results()
  {
    for (source, unset) in [
      (
        "x=1; if condition; then readonly x; fi; x=2; : \"$x\"",
        false,
      ),
      (
        "x=1; if condition; then readonly x; fi; unset -v x; : \"$x\"",
        true,
      ),
    ] {
      let (workspace, analysis) = analyze(source);
      let result = &reads(&workspace, &analysis, b"x")[0].resolution;
      assert_eq!(
        result.candidates,
        workspace
          .symbols_named(b"x")
          .map(|symbol| symbol.id)
          .collect()
      );
      assert_eq!(result.alternatives.contains(&Alternative::Unset), unset);
      assert_eq!(result.status(), ResolutionStatus::Candidates);
      assert_eq!(
        result.issues.contains(&ResolutionIssue::UnknownEffect),
        unset
      );
    }
    let (workspace, analysis) = analyze("x=1; readonly x; x=2; : \"$x\"");
    let first = workspace.symbols_named(b"x").next().unwrap().id;
    let result = &reads(&workspace, &analysis, b"x")[0].resolution;
    assert_eq!(result.candidates, [first].into());
    assert!(result.is_complete());
  }

  #[test]
  fn case_short_circuit_and_braces_merge_paths_without_creating_environments() {
    let (workspace, analysis) = analyze(
      "x=0; case word in a) x=1 ;& b) : \"$x\" ;; esac; condition && { x=2; }; : \"$x\"",
    );
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(uses[0].resolution.candidates, [ids[0], ids[1]].into());
    assert_eq!(uses[1].resolution.candidates, ids.into_iter().collect());
    assert!(uses.iter().all(|site| site.resolution.is_complete()));
    assert_eq!(analysis.contexts().len(), 1);
  }

  #[test]
  fn later_case_clauses_follow_expanded_patterns_and_pure_alternatives_stay_ordered()
   {
    let (workspace, analysis) =
      analyze("case a in ${x=1}) ;; *|b) : \"$x\";; esac");
    let x = workspace.symbols_named(b"x").next().unwrap().id;
    assert_eq!(
      reads(&workspace, &analysis, b"x")[1].resolution.status(),
      ResolutionStatus::Resolved(x)
    );
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn substitutions_are_isolated_and_pipelines_keep_environment_uncertainty() {
    let (workspace, analysis) =
      analyze("x=1; : $(x=2; : \"$x\"); : \"$x\"; { x=3; } | cat; : \"$x\"");
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(
      uses[0].resolution.status(),
      ResolutionStatus::Resolved(ids[1])
    );
    assert_eq!(
      uses[1].resolution.status(),
      ResolutionStatus::Resolved(ids[0])
    );
    assert_eq!(uses[2].resolution.candidates, [ids[0], ids[2]].into());
    assert!(uses[2].resolution.is_complete());
    assert!(
      analysis
        .gaps()
        .iter()
        .any(|gap| gap.issue == ResolutionIssue::PipelineEnvironment)
    );
    let substitution = analysis.context(uses[0].context).unwrap();
    assert_eq!(substitution.kind, ExecutionKind::Substitution);
    assert_ne!(substitution.environment, uses[1].context);
  }

  #[test]
  fn nested_loop_transfers_and_condition_breaks_reach_the_right_exit() {
    let (workspace, analysis) = analyze(
      "x=0; while condition; do while condition; do x=1; break 2; x=2; done; x=3; done; : \"$x\"",
    );
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    assert_eq!(
      reads(&workspace, &analysis, b"x")[0].resolution.candidates,
      [ids[0], ids[1], ids[3]].into()
    );
    let (workspace, analysis) =
      analyze("x=0; while x=1; break; do x=2; done; : \"$x\"");
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    assert_eq!(
      reads(&workspace, &analysis, b"x")[0].resolution.status(),
      ResolutionStatus::Resolved(ids[1])
    );
    let (workspace, analysis) = analyze("x=0; return; : \"$x\"");
    assert!(
      reads(&workspace, &analysis, b"x")[0]
        .resolution
        .issues
        .contains(&ResolutionIssue::UnsupportedControl)
    );
  }

  #[test]
  fn merges_preserve_distinct_definitions_and_unset_paths_with_equal_values() {
    for body in [
      "read unknown; : \"${unknown:-default}\"",
      "read unknown; : \"$((unknown ? 1 : 2))\"",
      "readonly locked; read locked",
      "case word in x) : ;& *) : ;; esac",
      "while condition; do unrelated=1; done",
      ": | :",
    ] {
      for other in ["x=1", "unset x"] {
        let source =
          format!("if condition; then x=1; else {other}; fi; {body}; : \"$x\"");
        let (workspace, analysis) = analyze(&source);
        let ids: Vec<_> = workspace
          .symbols_named(b"x")
          .map(|symbol| symbol.id)
          .collect();
        let uses = reads(&workspace, &analysis, b"x");
        let resolution = &uses.last().unwrap().resolution;
        assert_eq!(
          resolution.candidates,
          ids.into_iter().collect(),
          "{source}"
        );
        let alternatives = if other == "unset x" {
          [Alternative::Unset].into()
        } else {
          [].into()
        };
        assert_eq!(resolution.alternatives, alternatives, "{source}");
        assert!(resolution.issues.is_empty(), "{source}");
      }
    }
  }

  #[test]
  fn heredoc_expansions_run_with_redirections_and_quoted_bodies_are_inert() {
    let (workspace, analysis) = analyze(
      "x=1\nf() { x=2; }\nf <<EOF\n$x\nEOF\n: \"$x\"\ncat <<'EOF'\n$x\nEOF\n",
    );
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(uses.len(), 2);
    assert_eq!(
      uses[0].resolution.status(),
      ResolutionStatus::Resolved(ids[0])
    );
    assert_eq!(
      uses[1].resolution.status(),
      ResolutionStatus::Resolved(ids[1])
    );
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn multiple_and_nested_heredocs_keep_their_redirection_owner() {
    let (workspace, analysis) = analyze(
      "x=0\nf() { x=1; }\nf <<A <<B\n$x\nA\n$x\nB\n: \"$(cat <<C\n$x\nC\n)\"\n: \"$x\"\n",
    );
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(
      uses
        .iter()
        .map(|site| site.resolution.status())
        .collect::<Vec<_>>(),
      vec![
        ResolutionStatus::Resolved(ids[0]),
        ResolutionStatus::Resolved(ids[0]),
        ResolutionStatus::Resolved(ids[1]),
        ResolutionStatus::Resolved(ids[1])
      ]
    );
    assert_eq!(
      analysis.context(uses[2].context).unwrap().kind,
      ExecutionKind::Substitution
    );
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn unspecified_evaluation_order_never_claims_a_single_reaching_write() {
    for source in [
      "x=0; x=1 y=$x external",
      "x=0; : $(( (x=1) + x ))",
      "case a in a|${x=1}) : \"$x\";; esac",
    ] {
      let (workspace, analysis) = analyze(source);
      let uses = reads(&workspace, &analysis, b"x");
      assert_ne!(uses, [] as [&ResolvedUse; 0]);
      assert!(uses.iter().all(|site| {
        site
          .resolution
          .issues
          .contains(&ResolutionIssue::EvaluationOrder)
      }));
    }
  }

  #[test]
  fn redirection_expansions_follow_all_command_argument_expansions() {
    let (workspace, analysis) = analyze("x=1; : >${x:=2} \"$x\"");
    let x = workspace.symbols_named(b"x").next().unwrap().id;
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(uses[1].resolution.status(), ResolutionStatus::Resolved(x));
  }

  #[test]
  fn subshell_changes_do_not_replace_outer_bindings() {
    let (workspace, analysis) = analyze("x=1; (x=2; : \"$x\"); : \"$x\"");
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(
      uses[0].resolution.status(),
      ResolutionStatus::Resolved(ids[1])
    );
    assert_eq!(
      uses[1].resolution.status(),
      ResolutionStatus::Resolved(ids[0])
    );
  }

  #[test]
  fn repeated_source_reads_use_separate_contexts_and_return_their_writes() {
    let root = support::parse(1, 1, "x=1; . /lib; x=2; . /lib; : \"$x\"");
    let library = support::parse(2, 1, ": \"$x\"; x=3");
    let root_definitions: Vec<_> =
      root.symbols().iter().map(|symbol| symbol.id).collect();
    let library_definition = library.symbols()[0].id;
    let mut request = AnalysisRequest::new(root.snapshot());
    request.initial = InitialEnvironment::empty();
    let mut workspace = Workspace::default();
    workspace.insert(root).unwrap();
    let mut inputs = support::Inputs::sources([("/lib", library)]);
    let analysis = workspace
      .analyze(request, &mut inputs, &AtomicBool::new(false))
      .unwrap();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(
      uses
        .iter()
        .map(|site| site.resolution.status())
        .collect::<Vec<_>>(),
      vec![
        ResolutionStatus::Resolved(library_definition),
        ResolutionStatus::Resolved(root_definitions[0]),
        ResolutionStatus::Resolved(root_definitions[1]),
      ]
    );
    assert_ne!(uses[1].context, uses[2].context);
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn sourced_definitions_and_returns_follow_function_call_contexts() {
    let root = support::parse(
      1,
      1,
      "load() { . /lib; }; x=1; load; x=2; load; f; : \"$x\"",
    );
    let library = support::parse(2, 1, ": \"$x\"; f() { x=3; }; return; x=4");
    let mut request = AnalysisRequest::new(root.snapshot());
    request.initial = InitialEnvironment::empty();
    let mut workspace = Workspace::default();
    workspace.insert(root).unwrap();
    let mut inputs = support::Inputs::sources([("/lib", library)]);
    let analysis = workspace
      .analyze(request, &mut inputs, &AtomicBool::new(false))
      .unwrap();
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(
      uses
        .iter()
        .map(|site| site.resolution.status())
        .collect::<Vec<_>>(),
      vec![
        ResolutionStatus::Resolved(ids[2]),
        ResolutionStatus::Resolved(ids[0]),
        ResolutionStatus::Resolved(ids[1])
      ]
    );
    assert_ne!(uses[1].context, uses[2].context);
    for site in &uses[1..] {
      let source = analysis.context(site.context).unwrap();
      assert!(matches!(source.kind, ExecutionKind::Source(_)));
      assert!(matches!(
        analysis.context(source.parent.unwrap()).unwrap().kind,
        ExecutionKind::Function(_)
      ));
    }
    assert_eq!(analysis.references(ids[3]).count(), 0);
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn sourced_files_start_with_the_loading_status_and_empty_files_complete_with_zero()
   {
    for (entry, body, expected) in [
      ("(exit 3); . /lib; out=$?; : \"$out\"", "# comment\n", "0"),
      ("(exit 3); . /lib", "out=$?; : \"$out\"", "3"),
      (". /lib; out=$?; : \"$out\"", "(exit 3)", "3"),
    ] {
      let root = support::parse(1, 1, entry);
      let library = support::parse(2, 1, body);
      let mut request = AnalysisRequest::new(root.snapshot());
      request.initial = InitialEnvironment::empty();
      let mut workspace = Workspace::default();
      workspace.insert(root).unwrap();
      let mut inputs = support::Inputs::sources([("/lib", library)]);
      let analysis = workspace
        .analyze(request, &mut inputs, &AtomicBool::new(false))
        .unwrap();
      let read = reads(&workspace, &analysis, b"out")[0];
      assert_eq!(
        analysis.value(read.site.syntax().unwrap(), read.context),
        Some(&Value::Literal(expected.as_bytes().to_vec())),
        "{entry} with {body:?}"
      );
    }
  }

  #[test]
  fn source_cycles_failures_and_stale_snapshots_preserve_unknown_effects() {
    use shellsmith::interface::application::engine::SourceDocument;

    let root = support::parse(1, 1, "x=1; . /lib; x=2; : \"$x\"");
    let library = support::parse(2, 1, "again() { . /root; }; again");
    let entry = root.snapshot();
    let target = library.snapshot();
    let mut request = AnalysisRequest::new(entry);
    request.initial = InitialEnvironment::empty();
    let mut workspace = Workspace::default();
    workspace.insert(root).unwrap();
    let mut inputs = support::Inputs::sources([
      ("/lib", library),
      ("/root", support::parse(1, 1, "x=1; . /lib; x=2; : \"$x\"")),
    ]);
    let cyclic = workspace
      .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
      .unwrap();
    assert!(
      reads(&workspace, &cyclic, b"x")[0]
        .resolution
        .issues
        .contains(&ResolutionIssue::Recursion)
    );
    let input = inputs.source_queries[0].clone();
    let library = inputs.sources.remove(&input).unwrap();
    for state in [
      SourceDocument::Unresolved("dynamic target".into()),
      SourceDocument::Failed("read failed".into()),
    ] {
      inputs.sources.insert(input.clone(), state);
      let failed = workspace
        .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
        .unwrap();
      assert!(
        reads(&workspace, &failed, b"x")[0]
          .resolution
          .issues
          .contains(&ResolutionIssue::UnresolvedSource)
      );
    }
    inputs.sources.insert(input, library);
    workspace.insert(support::parse(2, 2, "x=3")).unwrap();
    assert!(matches!(
      workspace.analyze(request, &mut inputs, &AtomicBool::new(false)),
      Err(Error::StaleSnapshot(snapshot)) if snapshot == target
    ));
  }

  #[test]
  fn source_and_function_calls_do_not_make_caller_loops_lexical() {
    let root = support::parse(
      1,
      1,
      "x=0; while condition; do . /lib; x=1; done; : \"$x\"",
    );
    let library = support::parse(2, 1, "break");
    let mut request = AnalysisRequest::new(root.snapshot());
    request.initial = InitialEnvironment::empty();
    let mut workspace = Workspace::default();
    workspace.insert(root).unwrap();
    let mut inputs = support::Inputs::sources([("/lib", library)]);
    let analysis = workspace
      .analyze(request, &mut inputs, &AtomicBool::new(false))
      .unwrap();
    assert!(
      reads(&workspace, &analysis, b"x")[0]
        .resolution
        .issues
        .contains(&ResolutionIssue::UnsupportedControl)
    );
    let (workspace, analysis) = analyze(
      "x=0; f() { while condition; do break 2; done; }; while condition; do f; x=1; done; : \"$x\"",
    );
    assert!(
      reads(&workspace, &analysis, b"x")[0]
        .resolution
        .issues
        .contains(&ResolutionIssue::UnsupportedControl)
    );
  }

  #[test]
  fn loops_keep_entry_and_repeated_writes_and_honor_break_and_continue() {
    let (workspace, analysis) = analyze(
      "x=0; for i in a b; do : \"$x\"; x=1; continue; x=2; done; : \"$x\"",
    );
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    for site in reads(&workspace, &analysis, b"x") {
      assert_eq!(site.resolution.candidates, [ids[0], ids[1]].into());
      assert!(!site.resolution.candidates.contains(&ids[2]));
    }
    let (workspace, analysis) =
      analyze("x=0; while condition; do x=1; break; x=2; done; : \"$x\"");
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    assert_eq!(
      reads(&workspace, &analysis, b"x")[0].resolution.candidates,
      [ids[0], ids[1]].into()
    );
  }

  #[test]
  fn background_assignments_and_function_returns_preserve_control_boundaries() {
    let (workspace, analysis) =
      analyze("x=1; x=2 & : \"$x\"; f() { x=3; return; x=4; }; f; : \"$x\"");
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(
      uses[0].resolution.status(),
      ResolutionStatus::Resolved(ids[0])
    );
    assert_eq!(
      uses[1].resolution.status(),
      ResolutionStatus::Resolved(ids[2])
    );
    assert!(
      analysis
        .contexts()
        .iter()
        .any(|context| context.kind == ExecutionKind::Asynchronous)
    );
  }

  #[test]
  fn unknown_sources_and_recursive_calls_do_not_claim_one_definition() {
    for source in [
      "x=1; . missing; : \"$x\"",
      "x=1; f() { x=2; f; }; f; : \"$x\"",
    ] {
      let (workspace, analysis) = analyze(source);
      let result = &reads(&workspace, &analysis, b"x")[0].resolution;
      assert_eq!(result.status(), ResolutionStatus::Candidates);
      assert!(!result.is_complete());
      assert_ne!(analysis.gaps(), []);
    }
  }

  #[test]
  fn input_changes_invalidate_published_results_and_reject_stale_publication() {
    let (mut workspace, old) = analyze("x=1; : \"$x\"");
    let analysis = workspace
      .analyze(
        old.request().clone(),
        &mut support::Inputs::default(),
        &AtomicBool::new(false),
      )
      .unwrap();
    workspace.publish_analysis(analysis).unwrap();
    assert!(workspace.analysis().is_some());
    workspace
      .insert(support::parse(1, 2, "x=2; : \"$x\""))
      .unwrap();
    assert!(workspace.analysis().is_none());
    assert_eq!(workspace.publish_analysis(old), Err(Error::StaleAnalysis));
    let entry = workspace.document(DocumentId(1)).unwrap().snapshot();
    let mut request = AnalysisRequest::new(entry);
    request.initial = InitialEnvironment::empty();
    let old = workspace
      .analyze(
        request.clone(),
        &mut support::Inputs::default(),
        &AtomicBool::new(false),
      )
      .unwrap();
    request
      .initial
      .variables
      .insert(b"external".to_vec(), InitialBinding::External);
    workspace
      .analyze(
        request,
        &mut support::Inputs::default(),
        &AtomicBool::new(false),
      )
      .unwrap();
    assert_eq!(workspace.publish_analysis(old), Err(Error::StaleAnalysis));
  }

  #[test]
  fn one_candidate_is_resolved_only_without_other_possibilities_or_gaps() {
    let document = support::parse(1, 1, "x=1; x=2");
    let first = document.symbols()[0].id;
    let second = document.symbols()[1].id;
    let mut result = Resolution::default();
    assert_eq!(result.status(), ResolutionStatus::Unresolved);
    result.candidates.insert(first);
    assert_eq!(result.status(), ResolutionStatus::Resolved(first));
    result.alternatives.insert(Alternative::Unset);
    assert_eq!(result.status(), ResolutionStatus::Candidates);
    assert!(result.is_complete());
    result.alternatives.clear();
    result.issues.insert(ResolutionIssue::UnknownEffect);
    assert_eq!(result.status(), ResolutionStatus::Candidates);
    assert!(!result.is_complete());
    result.issues.clear();
    result.candidates.insert(second);
    assert_eq!(result.status(), ResolutionStatus::Candidates);
    assert!(result.is_complete());
  }

  #[test]
  fn unspecified_initial_conditions_are_not_treated_as_empty() {
    let unknown = InitialEnvironment::default();
    assert_eq!(unknown.other_variables, InitialBinding::Unknown);
    assert_eq!(unknown.other_functions, InitialBinding::Unknown);
    let empty = InitialEnvironment::empty();
    assert_eq!(empty.other_variables, InitialBinding::Absent);
    assert_eq!(empty.other_functions, InitialBinding::Absent);
  }

  #[test]
  fn initial_external_bindings_and_special_parameters_are_not_source_definitions()
   {
    let (mut workspace, report) =
      analyze(": \"$x\" \"$1\" \"$?\"; x=1; : \"$x\"");
    let mut request = report.request().clone();
    request
      .initial
      .variables
      .insert(b"x".to_vec(), InitialBinding::External);
    let analysis = workspace
      .analyze(
        request,
        &mut support::Inputs::default(),
        &AtomicBool::new(false),
      )
      .unwrap();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(
      uses[0].resolution.alternatives,
      [Alternative::External].into()
    );
    assert!(uses[0].resolution.is_complete());
    let x = workspace.symbols_named(b"x").next().unwrap().id;
    assert_eq!(uses[1].resolution.status(), ResolutionStatus::Resolved(x));
    for name in [b"1".as_slice(), b"?".as_slice()] {
      assert_eq!(
        reads(&workspace, &analysis, name)[0]
          .resolution
          .alternatives,
        [Alternative::SpecialParameter].into()
      );
    }
    let mut request = AnalysisRequest::new(analysis.request().entry);
    request.initial = InitialEnvironment::default();
    let unknown = workspace
      .analyze(
        request,
        &mut support::Inputs::default(),
        &AtomicBool::new(false),
      )
      .unwrap();
    let uses = reads(&workspace, &unknown, b"x");
    assert_eq!(
      uses[0].resolution.issues,
      [ResolutionIssue::InitialState].into()
    );
    assert_eq!(uses[1].resolution.status(), ResolutionStatus::Resolved(x));
  }

  #[test]
  fn arithmetic_reads_writes_and_conditional_paths_keep_definition_identity() {
    let (workspace, analysis) = analyze(
      "read condition; x=1; : $((x+=2)); : \"$x\"; : $((condition ? (x=3) : (x=4))); : \"$x\"",
    );
    let ids: Vec<_> = workspace
      .symbols_named(b"x")
      .map(|symbol| symbol.id)
      .collect();
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(
      uses[0].resolution.status(),
      ResolutionStatus::Resolved(ids[0])
    );
    assert_eq!(
      uses[1].resolution.status(),
      ResolutionStatus::Resolved(ids[1])
    );
    assert_eq!(uses[2].resolution.candidates, [ids[2], ids[3]].into());
    assert!(uses[2].resolution.is_complete());
    let (workspace, analysis) =
      analyze("read expression; x=1; : $(( $expression )); : \"$x\"");
    assert!(
      !reads(&workspace, &analysis, b"x")[0]
        .resolution
        .is_complete()
    );
  }

  #[test]
  fn runtime_termination_keeps_resolving_later_definitions_and_reads() {
    for termination in ["exit", "exit 1", "exec", "exec replacement"] {
      let source = format!(
        "x=0; (x=1; {termination}; x=2; : \"$x\"); : \"$x\"; {termination}; x=3; : \"$x\""
      );
      let (workspace, analysis) = analyze(&source);
      let ids: Vec<_> = workspace
        .symbols_named(b"x")
        .map(|symbol| symbol.id)
        .collect();
      let statuses: Vec<_> = reads(&workspace, &analysis, b"x")
        .iter()
        .map(|site| site.resolution.status())
        .collect();
      assert_eq!(
        statuses,
        vec![
          ResolutionStatus::Resolved(ids[2]),
          ResolutionStatus::Resolved(ids[0]),
          ResolutionStatus::Resolved(ids[3]),
        ],
        "{source}"
      );
      assert_eq!(analysis.gaps(), [], "{source}");
    }
  }

  #[test]
  fn function_returns_stop_subshell_bodies_and_keep_pipeline_return_paths() {
    let (workspace, analysis) =
      analyze("x=0; f() { (return; : \"$x\"); : \"$x\"; }; f");
    let uses = reads(&workspace, &analysis, b"x");
    assert_eq!(uses.len(), 1);
    assert!(uses[0].resolution.is_complete());
    assert_eq!(analysis.gaps(), []);
    let (workspace, analysis) =
      analyze("x=0; f() { return | cat; x=1; }; f; : \"$x\"");
    let result = &reads(&workspace, &analysis, b"x")[0].resolution;
    assert_eq!(
      result.candidates,
      workspace
        .symbols_named(b"x")
        .map(|symbol| symbol.id)
        .collect()
    );
    assert!(result.is_complete());
    assert!(
      analysis
        .gaps()
        .iter()
        .any(|gap| gap.issue == ResolutionIssue::PipelineEnvironment)
    );
  }
  #[test]
  fn source_inputs_use_the_provider_conditions_instead_of_registered_dependencies()
   {
    use shellsmith::interface::application::engine::{
      SourceDocument, SourceInput,
    };
    use std::sync::Arc;

    for (operand, path, cwd, search_path) in [
      ("\"$HOME/lib.sh\"", "/actual/lib.sh", None, None),
      ("./lib.sh", "./lib.sh", Some("/actual"), None),
      ("lib.sh", "lib.sh", None, Some("/actual")),
    ] {
      let input = SourceInput {
        operand: Value::Literal(path.as_bytes().to_vec()),
        cwd: cwd.map(|path| Value::Literal(path.as_bytes().to_vec())),
        search_path: search_path
          .map(|path| Value::Literal(path.as_bytes().to_vec())),
        annotation: None,
      };
      for supplied in ["missing", "mismatched", "matching"] {
        let root = support::parse(1, 1, &format!(". {operand}; : \"$loaded\""));
        let reference = root.references()[0].id;
        let library = support::parse(2, 1, "loaded=registered");
        let registered = library.symbols()[0].id;
        let target = library.snapshot();
        let current = Arc::new(support::parse(3, 1, "loaded=current"));
        let definition = current.symbols()[0].id;
        let mut request = AnalysisRequest::new(root.snapshot());
        request.initial = InitialEnvironment::empty();
        request.initial.cwd = Some(b"/actual".to_vec());
        request
          .initial
          .values
          .insert(b"PATH".to_vec(), b"/actual".to_vec());
        request
          .initial
          .values
          .insert(b"HOME".to_vec(), b"/actual".to_vec());
        let mut workspace = Workspace::default();
        workspace.insert(root).unwrap();
        workspace.insert(library).unwrap();
        workspace
          .set_reference(reference, DependencyState::Resolved(target))
          .unwrap();
        let mut inputs = support::Inputs::default();
        if supplied != "missing" {
          let mut key = input.clone();
          if supplied == "mismatched" {
            if key.cwd.is_some() {
              key.cwd = Some(Value::Literal(b"/other".to_vec()));
            } else if key.search_path.is_some() {
              key.search_path = Some(Value::Literal(b"/other".to_vec()));
            } else {
              key.operand = Value::Literal(b"/other/lib.sh".to_vec());
            }
          }
          inputs
            .sources
            .insert(key, SourceDocument::Resolved(current.clone()));
        }
        let result = workspace
          .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
          .unwrap();
        assert_eq!(
          inputs.source_queries.as_slice(),
          std::slice::from_ref(&input),
          "{operand}: {supplied}"
        );
        assert_eq!(
          result.references(registered).count(),
          0,
          "{operand}: {supplied}"
        );
        assert_eq!(
          result.references(definition).count(),
          usize::from(supplied == "matching"),
          "{operand}: {supplied}"
        );
        workspace.publish_analysis(result).unwrap();
        let targets: Vec<_> = workspace
          .source_links(DocumentId(1))
          .into_iter()
          .map(|(_, target)| target)
          .collect();
        assert_eq!(
          targets,
          if supplied == "matching" {
            vec![current.snapshot()]
          } else {
            Vec::new()
          }
        );
        inputs.sources.clear();
        let again = workspace
          .analyze(request, &mut inputs, &AtomicBool::new(false))
          .unwrap();
        assert_eq!(again.references(definition).count(), 0);
        workspace.publish_analysis(again).unwrap();
        assert_eq!(workspace.source_links(DocumentId(1)), []);
      }
    }
  }
}

mod updates {
  use crate::support;

  use shellsmith::interface::application::engine::{
    Analysis, AnalysisRequest, DependencyState, DocumentId, Error,
    InitialEnvironment, OccurrenceKind, ResolutionIssue, ResolutionStatus,
    SourceDocument, SourceInput, Value, Workspace,
  };
  use std::sync::Arc;
  use std::sync::atomic::AtomicBool;

  fn workspace(
    target: u64,
    reverse: bool,
  ) -> (Workspace, AnalysisRequest, support::Inputs) {
    let root =
      support::parse(1, 1, ". /lib; : \"$x\"; dormant() { : \"$y\"; }");
    let first = support::parse(2, 1, "x=1");
    let second = support::parse(3, 1, "x=2");
    let reference = root.references()[0].id;
    let mut request = AnalysisRequest::new(root.snapshot());
    request.initial = InitialEnvironment::empty();
    let mut documents: Vec<_> =
      [root, first, second].into_iter().map(Arc::new).collect();
    let mut inputs = support::Inputs::default();
    inputs.sources.insert(
      SourceInput {
        operand: Value::Literal(b"/lib".to_vec()),
        cwd: None,
        search_path: None,
        annotation: None,
      },
      SourceDocument::Resolved(
        documents[usize::try_from(target - 1).unwrap()].clone(),
      ),
    );
    if reverse {
      documents.reverse();
    }
    let mut workspace = Workspace::default();
    for document in documents {
      workspace.insert_shared(document).unwrap();
    }
    let target = workspace.document(DocumentId(target)).unwrap().snapshot();
    workspace
      .set_reference(reference, DependencyState::Resolved(target))
      .unwrap();
    (workspace, request, inputs)
  }

  fn equivalent(left: &Analysis, right: &Analysis) {
    assert_eq!(left.request(), right.request());
    assert_eq!(left.snapshots(), right.snapshots());
    assert_eq!(left.source_bindings(), right.source_bindings());
    assert_eq!(left.contexts(), right.contexts());
    assert_eq!(
      left.uses().collect::<Vec<_>>(),
      right.uses().collect::<Vec<_>>()
    );
    assert_eq!(left.gaps(), right.gaps());
  }

  #[test]
  fn rebinding_without_text_changes_rejects_old_results_and_matches_fresh_analysis()
   {
    let (mut updated, request, mut inputs) = workspace(2, false);
    let old = updated
      .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
      .unwrap();
    let published = updated
      .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
      .unwrap();
    updated.publish_analysis(published).unwrap();
    let root = updated.document(DocumentId(1)).unwrap();
    let reference = root.references()[0].id;
    let target = updated.document(DocumentId(3)).unwrap().snapshot();
    updated
      .set_reference(reference, DependencyState::Resolved(target))
      .unwrap();
    assert!(updated.analysis().is_none());
    assert_eq!(updated.publish_analysis(old), Err(Error::StaleAnalysis));
    *inputs.sources.values_mut().next().unwrap() =
      SourceDocument::Resolved(Arc::new(support::parse(3, 1, "x=2")));
    let new = updated
      .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
      .unwrap();
    let (mut fresh, fresh_request, mut fresh_inputs) = workspace(3, true);
    let fresh_analysis = fresh
      .analyze(fresh_request, &mut fresh_inputs, &AtomicBool::new(false))
      .unwrap();
    equivalent(&new, &fresh_analysis);
    let previous = updated.symbols_named(b"x").next().unwrap().id;
    let current = updated.document(DocumentId(3)).unwrap().symbols()[0].id;
    assert_eq!(new.references(previous).count(), 0);
    assert_eq!(new.references(current).count(), 1);
    assert_eq!(
      updated.publish_analysis(fresh_analysis),
      Err(Error::StaleAnalysis)
    );
  }

  #[test]
  fn removal_and_reinsertion_require_current_bindings_and_keep_snapshot_provenance()
   {
    let (mut workspace, request, mut inputs) = workspace(2, false);
    let old = workspace
      .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
      .unwrap();
    let target = workspace.document(DocumentId(2)).unwrap().snapshot();
    let reference =
      workspace.document(DocumentId(1)).unwrap().references()[0].id;
    workspace.remove(target).unwrap();
    let input = inputs.sources.keys().next().unwrap().clone();
    inputs.sources.clear();
    let missing = workspace
      .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
      .unwrap();
    assert!(
      missing
        .gaps()
        .iter()
        .any(|gap| gap.issue == ResolutionIssue::UnresolvedSource)
    );
    assert!(old.snapshots().contains(&target));
    assert!(!missing.snapshots().contains(&target));
    assert_eq!(workspace.publish_analysis(old), Err(Error::StaleAnalysis));
    workspace.insert(support::parse(2, 2, "x=3")).unwrap();
    assert_eq!(
      workspace.set_reference(reference, DependencyState::Resolved(target)),
      Err(Error::StaleSnapshot(target))
    );
    let current = workspace.document(DocumentId(2)).unwrap().snapshot();
    workspace
      .set_reference(reference, DependencyState::Resolved(current))
      .unwrap();
    inputs.sources.insert(
      input,
      SourceDocument::Resolved(Arc::new(support::parse(2, 2, "x=3"))),
    );
    let analysis = workspace
      .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
      .unwrap();
    assert_eq!(
      analysis.source_bindings()[&reference],
      DependencyState::Resolved(current)
    );
    let definition = workspace.document(DocumentId(2)).unwrap().symbols()[0].id;
    assert_eq!(
      analysis
        .references(definition)
        .next()
        .unwrap()
        .resolution
        .status(),
      ResolutionStatus::Resolved(definition)
    );
  }

  #[test]
  fn document_summaries_preserve_call_contexts_and_unattempted_occurrences() {
    let document = support::parse(
      1,
      1,
      "f() { : \"$x\"; }; unused() { : \"$y\"; }; x=1; f; x=2; f",
    );
    let snapshot = document.snapshot();
    let x = document
      .occurrences()
      .iter()
      .find(|site| {
        site.kind == OccurrenceKind::VariableExpansion
          && site.name == Value::Literal(b"x".to_vec())
      })
      .unwrap()
      .id;
    let y = document
      .occurrences()
      .iter()
      .find(|site| site.name == Value::Literal(b"y".to_vec()))
      .unwrap()
      .id;
    let mut request = AnalysisRequest::new(snapshot);
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
    let summary = analysis.summarize(
      shellsmith::interface::application::engine::UseSite::Syntax(x),
    );
    assert_eq!(summary.contexts.len(), 2);
    assert_eq!(summary.status(), ResolutionStatus::Candidates);
    assert!(
      analysis
        .resolutions(
          shellsmith::interface::application::engine::UseSite::Syntax(x)
        )
        .all(|site| matches!(
          site.resolution.status(),
          ResolutionStatus::Resolved(_)
        ))
    );
    assert_eq!(
      analysis
        .summarize(shellsmith::interface::application::engine::UseSite::Syntax(
          y
        ))
        .status(),
      ResolutionStatus::NotAttempted
    );
    assert!(analysis.document_resolutions(snapshot).any(|site| site.site
      == shellsmith::interface::application::engine::UseSite::Syntax(y)
      && site.status() == ResolutionStatus::NotAttempted));
    assert_eq!(
      analysis.status(
        shellsmith::interface::application::engine::UseSite::Syntax(x),
        analysis.contexts()[0].id
      ),
      ResolutionStatus::NotAttempted
    );
    assert_eq!(workspace.search_symbols(b"unused").count(), 1);
  }

  #[test]
  fn cancellation_and_rejected_requests_leave_the_published_result_intact() {
    let (mut workspace, request, mut inputs) = workspace(2, false);
    let report = workspace
      .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
      .unwrap();
    let mut invalid = report.request().clone();
    invalid.limits.value_bytes = 0;
    workspace.publish_analysis(report).unwrap();
    assert!(matches!(
      workspace.analyze(invalid, &mut inputs, &AtomicBool::new(false)),
      Err(Error::InvalidAnalysis(_))
    ));
    assert!(matches!(
      workspace.analyze(request.clone(), &mut inputs, &AtomicBool::new(true)),
      Err(Error::Cancelled)
    ));
    assert!(workspace.analysis().is_some());
    let old = workspace
      .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
      .unwrap();
    let mut changed = old.request().clone();
    changed.limits.value_bytes += 1;
    workspace
      .analyze(changed, &mut inputs, &AtomicBool::new(false))
      .unwrap();
    assert!(workspace.analysis().is_none());
    assert_eq!(workspace.publish_analysis(old), Err(Error::StaleAnalysis));
    let old = workspace
      .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
      .unwrap();
    let mut changed = old.request().clone();
    changed.entry = workspace.document(DocumentId(3)).unwrap().snapshot();
    workspace
      .analyze(changed, &mut inputs, &AtomicBool::new(false))
      .unwrap();
    assert_eq!(workspace.publish_analysis(old), Err(Error::StaleAnalysis));
  }

  #[test]
  fn broken_syntax_records_gaps_and_repair_uses_only_the_new_revision() {
    for source in [
      "x=1; echo >; : \"$x\"",
      "x=1; if ; then x=2; fi; : \"$x\"",
      "x=1; : \"$x\" | ; : \"$x\"",
    ] {
      let document = support::parse(1, 1, source);
      assert!(document.has_syntax_errors());
      let mut request = AnalysisRequest::new(document.snapshot());
      request.initial = InitialEnvironment::empty();
      let mut workspace = Workspace::default();
      workspace.insert(document).unwrap();
      let damaged = workspace
        .analyze(
          request.clone(),
          &mut support::Inputs::default(),
          &AtomicBool::new(false),
        )
        .unwrap();
      assert!(
        damaged
          .gaps()
          .iter()
          .any(|gap| gap.issue == ResolutionIssue::Recovery),
        "{source}: {:?}",
        damaged.gaps()
      );
      workspace
        .insert(support::parse(1, 2, "x=1; : \"$x\""))
        .unwrap();
      assert_eq!(
        workspace.publish_analysis(damaged),
        Err(Error::StaleAnalysis)
      );
      request.entry = workspace.document(DocumentId(1)).unwrap().snapshot();
      let repaired = workspace
        .analyze(
          request,
          &mut support::Inputs::default(),
          &AtomicBool::new(false),
        )
        .unwrap();
      assert_eq!(repaired.gaps(), []);
      let definition = workspace.symbols_named(b"x").next().unwrap().id;
      assert_eq!(
        repaired
          .references(definition)
          .next()
          .unwrap()
          .resolution
          .status(),
        ResolutionStatus::Resolved(definition)
      );
    }
  }

  #[test]
  fn cancellation_during_analysis_never_publishes_a_partial_result() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    let source = format!("f() {{ :; }}\n{}", "f\n".repeat(5000));
    let document = support::parse(1, 1, &source);
    let mut request = AnalysisRequest::new(document.snapshot());
    request.initial = InitialEnvironment::empty();
    let mut workspace = Workspace::default();
    workspace.insert(document).unwrap();
    let cancelled = AtomicBool::new(false);
    std::thread::scope(|scope| {
      scope.spawn(|| {
        std::thread::sleep(Duration::from_millis(1));
        cancelled.store(true, Ordering::Relaxed);
      });
      assert!(matches!(
        workspace.analyze(request, &mut support::Inputs::default(), &cancelled),
        Err(Error::Cancelled)
      ));
    });
    assert!(workspace.analysis().is_none());
  }
}

mod stack {
  use crate::support;

  use shellsmith::interface::application::engine::{
    Analysis, AnalysisLimits, AnalysisRequest, DocumentId, InitialEnvironment,
    OccurrenceKind, ResolutionStatus, Value, Workspace,
  };
  use std::fmt::Write;
  use std::sync::atomic::AtomicBool;

  fn analyze_on_small_stack(
    mut workspace: Workspace,
    limits: AnalysisLimits,
    mut inputs: support::Inputs,
  ) -> (Workspace, Analysis) {
    let entry = workspace.document(DocumentId(1)).unwrap().snapshot();
    let mut request = AnalysisRequest::new(entry);
    request.initial = InitialEnvironment::empty();
    request.limits = limits;
    std::thread::Builder::new()
      .stack_size(256 * 1024)
      .spawn(move || {
        let analysis = workspace
          .analyze(request, &mut inputs, &AtomicBool::new(false))
          .unwrap();
        (workspace, analysis)
      })
      .unwrap()
      .join()
      .unwrap()
  }

  fn workspace(source: &str) -> Workspace {
    let document = support::parse(1, 1, source);
    assert!(!document.has_syntax_errors());
    let mut workspace = Workspace::default();
    workspace.insert(document).unwrap();
    workspace
  }

  fn resolved_reads(
    workspace: &Workspace,
    analysis: &Analysis,
    name: &[u8],
  ) -> Vec<ResolutionStatus> {
    analysis
      .uses()
      .filter_map(|site| {
        let occurrence = workspace.occurrence(site.site.syntax()?)?;
        (occurrence.kind != OccurrenceKind::Command
          && occurrence.name == Value::Literal(name.to_vec()))
        .then(|| site.resolution.status())
      })
      .collect()
  }

  #[test]
  fn deeply_nested_groups_preserve_writes_and_isolated_environments() {
    let depth = 1024;
    for (opening, closing, expected_indices) in
      [("{ ", "; }", [1, 1]), ("( ", " )", [1, 0])]
    {
      let source = format!(
        "x=0; {}x=1; : \"$x\"{}; : \"$x\"",
        opening.repeat(depth),
        closing.repeat(depth),
      );
      let (workspace, analysis) = analyze_on_small_stack(
        workspace(&source),
        AnalysisLimits::default(),
        support::Inputs::default(),
      );
      let definitions: Vec<_> = workspace
        .symbols_named(b"x")
        .map(|symbol| symbol.id)
        .collect();
      assert_eq!(
        resolved_reads(&workspace, &analysis, b"x"),
        expected_indices
          .map(|index| ResolutionStatus::Resolved(definitions[index])),
      );
      assert_eq!(analysis.gaps(), []);
    }
  }

  #[test]
  fn nested_parameter_and_arithmetic_expressions_preserve_read_write_order() {
    let depth = 256;
    for expression in [
      format!("{}$x{}", "${missing:-".repeat(depth), "}".repeat(depth)),
      format!("$(({}x{}))", "(".repeat(depth), ")".repeat(depth)),
    ] {
      let source = format!("x=0; x={expression}; : \"$x\"");
      let (workspace, analysis) = analyze_on_small_stack(
        workspace(&source),
        AnalysisLimits::default(),
        support::Inputs::default(),
      );
      let definitions: Vec<_> = workspace
        .symbols_named(b"x")
        .map(|symbol| symbol.id)
        .collect();
      assert_eq!(
        resolved_reads(&workspace, &analysis, b"x"),
        [
          ResolutionStatus::Resolved(definitions[0]),
          ResolutionStatus::Resolved(definitions[1]),
        ]
      );
      assert_eq!(analysis.gaps(), []);
    }
  }

  #[test]
  fn long_function_chains_return_to_callers_without_losing_effects() {
    let calls = 256;
    let mut source = String::from("x=0; f0() { x=1; return; x=2; }\n");
    for index in 1..calls {
      writeln!(source, "f{index}() {{ f{}; return; x=3; }}", index - 1)
        .unwrap();
    }
    writeln!(source, "f{}; : \"$x\"", calls - 1).unwrap();
    let (workspace, analysis) = analyze_on_small_stack(
      workspace(&source),
      AnalysisLimits::default(),
      support::Inputs::default(),
    );
    let assignment = workspace.symbols_named(b"x").nth(1).unwrap().id;
    assert_eq!(
      resolved_reads(&workspace, &analysis, b"x"),
      [ResolutionStatus::Resolved(assignment)]
    );
    assert_eq!(analysis.contexts().len(), calls + 1);
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn long_source_chains_restore_active_sources_and_propagate_returns() {
    let documents = 256;
    let mut workspace = Workspace::default();
    workspace
      .insert(support::parse(
        1,
        1,
        ". /lib2; : \"$x\"; . /lib2; : \"$x\"\n",
      ))
      .unwrap();
    let mut libraries = Vec::new();
    for id in 2..=documents {
      let source = if id == documents {
        "x=1; return; x=2\n".to_owned()
      } else {
        format!(". /lib{}; return; x=3\n", id + 1)
      };
      libraries.push((format!("/lib{id}"), support::parse(id, 1, &source)));
    }
    let (paths, bodies): (Vec<_>, Vec<_>) = libraries.into_iter().unzip();
    let inputs =
      support::Inputs::sources(paths.iter().map(String::as_str).zip(bodies));
    let (workspace, analysis) =
      analyze_on_small_stack(workspace, AnalysisLimits::default(), inputs);
    let assignment =
      workspace.document(DocumentId(documents)).unwrap().symbols()[0].id;
    assert_eq!(
      resolved_reads(&workspace, &analysis, b"x"),
      [ResolutionStatus::Resolved(assignment); 2]
    );
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn nested_command_forwarding_reaches_the_utility() {
    let source = format!("{}readonly x=1; : \"$x\"", "command ".repeat(1024));
    let (workspace, analysis) = analyze_on_small_stack(
      workspace(&source),
      AnalysisLimits::default(),
      support::Inputs::default(),
    );
    let assignment = workspace.symbols_named(b"x").next().unwrap().id;
    assert_eq!(
      resolved_reads(&workspace, &analysis, b"x"),
      [ResolutionStatus::Resolved(assignment)]
    );
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn long_choice_chains_are_released_without_recursive_stack_growth() {
    let mut source = String::from("x=0\n");
    for value in 1..=1024 {
      writeln!(source, "if condition; then x={value}; fi").unwrap();
    }
    source.push_str(": \"$x\"\n");
    let (workspace, analysis) = analyze_on_small_stack(
      workspace(&source),
      AnalysisLimits::default(),
      support::Inputs::default(),
    );
    let read = analysis
      .uses()
      .find(|site| {
        workspace
          .occurrence(site.site.syntax().unwrap())
          .is_some_and(|site| site.kind == OccurrenceKind::VariableExpansion)
      })
      .unwrap();
    assert_eq!(read.resolution.candidates.len(), 1025);
    let Some(Value::Partial(value)) =
      analysis.value(read.site.syntax().unwrap(), read.context)
    else {
      panic!("choice bounds");
    };
    assert_eq!(
      value
        .choices
        .iter()
        .map(|choice| choice.prefix.clone())
        .collect::<Vec<_>>(),
      (b'0'..=b'9').map(|byte| vec![byte]).collect::<Vec<_>>()
    );
  }
}

mod convergence {
  use crate::support;

  use shellsmith::interface::application::engine::{
    DocumentId, OccurrenceKind, Value, ValueIssue,
  };
  use std::fmt::Write;

  use support::analyze;

  #[test]
  fn thirty_thousand_commands_are_all_resolved_without_a_step_budget() {
    let source = String::from("x=1\n") + &": \"$x\"\n".repeat(30_000);
    let (workspace, analysis) = analyze(&source);
    let definition = workspace.symbols_named(b"x").next().unwrap().id;
    assert_eq!(analysis.references(definition).count(), 30_000);
    for site in analysis.references(definition) {
      assert_eq!(site.resolution.candidates, [definition].into());
      assert_eq!(
        analysis.value(site.site.syntax().unwrap(), site.context),
        Some(&Value::Literal(b"1".to_vec()))
      );
    }
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn forty_variable_loop_chain_converges_after_more_than_thirty_two_iterations()
  {
    let mut source = String::from("x0=seed\n");
    for index in 1..=40 {
      writeln!(source, "x{index}=''").unwrap();
    }
    source.push_str("while condition; do\n");
    for index in (1..=40).rev() {
      writeln!(source, "x{index}=\"$x{}\"", index - 1).unwrap();
    }
    source.push_str("done\n: \"$x40\"\nx40=restored\n: \"$x40\"\n");
    let (workspace, analysis) = analyze(&source);
    let document = workspace.document(DocumentId(1)).unwrap();
    let reads: Vec<_> = document
      .occurrences()
      .iter()
      .filter(|site| {
        site.kind == OccurrenceKind::VariableExpansion
          && site.name == Value::Literal(b"x40".to_vec())
      })
      .collect();
    assert_eq!(reads.len(), 2);
    let context = analysis.contexts()[0].id;
    let Some(Value::Partial(value)) = analysis.value(reads[0].id, context)
    else {
      panic!("loop values");
    };
    assert_eq!(value.issue, ValueIssue::Dynamic);
    assert!(value.choices.iter().all(|choice| choice.exact));
    assert_eq!(
      value
        .choices
        .iter()
        .map(|choice| choice.prefix.as_slice())
        .collect::<Vec<_>>(),
      [b"".as_slice(), b"seed"]
    );
    assert_eq!(
      analysis.value(reads[1].id, context),
      Some(&Value::Literal(b"restored".to_vec()))
    );
    assert_eq!(analysis.gaps(), []);
  }

  #[test]
  fn trap_bodies_are_analyzed_once_per_registration_not_per_signal_or_statement()
   {
    let source = String::from("x=1\n")
      + &"trap 'x=2; trap \"$unknown\" EXIT' HUP INT QUIT TERM USR1 USR2 CHLD ALRM PIPE\n: \"$x\"\n".repeat(4_097);
    let (workspace, analysis) = analyze(&source);
    assert_eq!(workspace.documents().count(), 4_098);
    assert_eq!(analysis.contexts().len(), 4_098);
    assert_eq!(
      analysis
        .contexts()
        .iter()
        .filter(|context| support::deferred(&analysis, context.id))
        .count(),
      4_097
    );
    let definition = workspace.symbols_named(b"x").next().unwrap().id;
    assert_eq!(analysis.references(definition).count(), 4_097);
    for site in analysis.references(definition) {
      assert_eq!(
        analysis.value(site.site.syntax().unwrap(), site.context),
        Some(&Value::Literal(b"1".to_vec()))
      );
    }
  }

  #[test]
  fn independent_arguments_and_nested_branch_joins_keep_shared_choices() {
    let mut source = String::from("if outer; then\n");
    for index in 0..32 {
      writeln!(source, "if condition; then v{index}=1; else v{index}=2; fi")
        .unwrap();
    }
    source.push_str("else\n");
    for index in 0..32 {
      writeln!(source, "v{index}=3").unwrap();
    }
    source.push_str("fi\nif c; then cmd=printf; else cmd=echo; fi\n");
    for command in [
      ":",
      "printf '%s'",
      "/usr/bin/printf '%s'",
      "command -p -- printf '%s'",
      "command command printf '%s'",
      "command \"$cmd\"",
      "\"$cmd\"",
    ] {
      source.push_str(command);
      for index in 0..32 {
        write!(source, " \"$v{index}\"").unwrap();
      }
      source.push('\n');
    }
    source.push_str("result=$((v0-v0)); : \"$result\"\n");
    let (workspace, analysis) = analyze(&source);
    let document = workspace.document(DocumentId(1)).unwrap();
    let last = document
      .occurrences()
      .iter()
      .rev()
      .find(|site| {
        site.kind == OccurrenceKind::VariableExpansion
          && site.name == Value::Literal(b"result".to_vec())
      })
      .unwrap();
    assert_eq!(
      analysis
        .values(last.id)
        .map(|(_, value)| value.clone())
        .collect::<Vec<_>>(),
      [Value::Literal(b"0".to_vec())]
    );
    for index in 0..32 {
      let name = format!("v{index}");
      let definitions: std::collections::BTreeSet<_> = workspace
        .symbols_named(name.as_bytes())
        .map(|symbol| symbol.id)
        .collect();
      assert_eq!(definitions.len(), 3);
      for site in document.occurrences().iter().filter(|site| {
        site.kind == OccurrenceKind::VariableExpansion
          && site.name == Value::Literal(name.as_bytes().to_vec())
      }) {
        for usage in analysis.uses().filter(|usage| {
          usage.site
            == shellsmith::interface::application::engine::UseSite::Syntax(
              site.id,
            )
        }) {
          assert_eq!(usage.resolution.candidates, definitions);
        }
      }
    }
    assert_eq!(analysis.gaps(), []);
  }
}

mod inputs {
  use crate::support;
  use shellsmith::interface::application::engine::{
    AnalysisInputs, AnalysisRequest, Document, Error, InitialEnvironment,
    InputQuery, InputResult, Occurrence, OccurrenceKind, SourceDocument,
    SourceInput, UseSite, Value, Workspace,
  };
  use std::collections::BTreeMap;
  use std::sync::Arc;
  use std::sync::atomic::{AtomicBool, Ordering};

  #[test]
  fn changed_provider_results_reject_old_analysis_without_document_changes() {
    let root = support::parse(1, 1, "value=~example; : \"$value\"");
    let read = root
      .occurrences()
      .iter()
      .find(|site| site.kind == OccurrenceKind::VariableExpansion)
      .unwrap()
      .id;
    let mut request = AnalysisRequest::new(root.snapshot());
    request.initial = InitialEnvironment::empty();
    let mut workspace = Workspace::default();
    workspace.insert(root).unwrap();
    let mut inputs = support::Inputs::default();
    let query = InputQuery::Home(b"example".to_vec());
    inputs
      .evaluated
      .insert(query.clone(), InputResult::Path(b"/first".to_vec()));
    let old = workspace
      .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
      .unwrap();
    inputs
      .evaluated
      .insert(query, InputResult::Path(b"/second".to_vec()));
    let current = workspace
      .analyze(request, &mut inputs, &AtomicBool::new(false))
      .unwrap();
    assert_eq!(
      old.values(read).next().unwrap().1,
      &Value::Literal(b"/first".to_vec())
    );
    assert_eq!(
      current.values(read).next().unwrap().1,
      &Value::Literal(b"/second".to_vec())
    );
    assert_eq!(old.snapshots(), current.snapshots());
    workspace.publish_analysis(current).unwrap();
    let published = std::ptr::from_ref(workspace.analysis().unwrap());
    assert_eq!(workspace.publish_analysis(old), Err(Error::StaleAnalysis));
    assert_eq!(std::ptr::from_ref(workspace.analysis().unwrap()), published);
  }

  #[test]
  fn source_contexts_use_current_search_conditions_and_require_agreement_for_links()
   {
    use shellsmith::interface::application::engine::{
      DependencyState, DocumentId,
    };

    for (operand, changes) in [
      ("./lib", ["cd /a", "cd /b"]),
      ("lib", ["PATH=/a", "PATH=/b"]),
    ] {
      let root = support::parse(
        1,
        1,
        &format!(
          "load() {{ . {operand}; : \"$value\"; }}; {}; load; {}; load",
          changes[0], changes[1]
        ),
      );
      let reference = root.references()[0].id;
      let mut request = AnalysisRequest::new(root.snapshot());
      request.initial = InitialEnvironment::empty();
      let mut workspace = Workspace::default();
      workspace.insert(root).unwrap();
      let first = Arc::new(support::parse(2, 1, "value=first"));
      let second = Arc::new(support::parse(3, 1, "value=second"));
      let mut inputs = support::Inputs::default();
      let mut expected = Vec::new();
      for (path, document) in [
        (b"/a".to_vec(), first.clone()),
        (b"/b".to_vec(), second.clone()),
      ] {
        inputs.evaluated.insert(
          InputQuery::Directory {
            cwd: Vec::new(),
            path: path.clone(),
            physical: false,
          },
          InputResult::Path(path.clone()),
        );
        let input = SourceInput {
          operand: Value::Literal(operand.as_bytes().to_vec()),
          cwd: (operand == "./lib").then(|| Value::Literal(path.clone())),
          search_path: (operand == "lib").then_some(Value::Literal(path)),
          annotation: None,
        };
        expected.push(input.clone());
        inputs
          .sources
          .insert(input, SourceDocument::Resolved(document));
      }
      let analysis = workspace
        .analyze(request.clone(), &mut inputs, &AtomicBool::new(false))
        .unwrap();
      assert_eq!(inputs.source_queries, expected);
      for document in [&first, &second] {
        assert_eq!(analysis.references(document.symbols()[0].id).count(), 1);
      }
      assert!(matches!(
        analysis.source_bindings()[&reference],
        DependencyState::Unresolved(_)
      ));
      workspace.publish_analysis(analysis).unwrap();
      assert_eq!(workspace.source_links(DocumentId(1)), []);
      inputs
        .sources
        .insert(expected[1].clone(), SourceDocument::Resolved(first.clone()));
      let analysis = workspace
        .analyze(request, &mut inputs, &AtomicBool::new(false))
        .unwrap();
      assert_eq!(analysis.references(first.symbols()[0].id).count(), 2);
      assert_eq!(analysis.references(second.symbols()[0].id).count(), 0);
      assert_eq!(
        analysis.source_bindings()[&reference],
        DependencyState::Resolved(first.snapshot())
      );
      workspace.publish_analysis(analysis).unwrap();
      let links = workspace.source_links(DocumentId(1));
      assert_eq!(links.len(), 1);
      assert_eq!(links[0].1, first.snapshot());
    }
  }

  struct Inputs<'a> {
    documents: BTreeMap<Vec<u8>, Arc<Document>>,
    sources: Vec<Vec<u8>>,
    queries: Vec<InputQuery>,
    stop: Option<&'a AtomicBool>,
    fail: bool,
  }

  impl AnalysisInputs for Inputs<'_> {
    fn source(&mut self, input: &SourceInput) -> Result<SourceDocument, Error> {
      let Value::Literal(path) = &input.operand else {
        panic!("expected a known path: {input:?}")
      };
      self.sources.push(path.clone());
      if self.sources.len() == 2 {
        if self.fail {
          return Err(Error::Input("Input provider failed".into()));
        }
        if let Some(flag) = self.stop {
          flag.store(true, Ordering::Relaxed);
        }
      }
      Ok(SourceDocument::Resolved(self.documents[path].clone()))
    }

    fn evaluate(&mut self, query: &InputQuery) -> Result<InputResult, Error> {
      self.queries.push(query.clone());
      assert_eq!(
        query,
        &InputQuery::Directory {
          cwd: Vec::new(),
          path: b"/next".to_vec(),
          physical: false
        }
      );
      if self.fail {
        return Err(Error::Input("Input provider failed".into()));
      }
      if let Some(flag) = self.stop {
        flag.store(true, Ordering::Relaxed);
      }
      Ok(InputResult::Path(b"/next".to_vec()))
    }
  }

  #[test]
  fn source_chain_continues_once_per_call_and_returns_values_to_its_entry() {
    let root = support::parse(1, 1, "cd /next; . ./lib0; : \"$value\"");
    let snapshot = root.snapshot();
    let read = root
      .occurrences()
      .iter()
      .find(|site| site.name == Value::Literal(b"value".to_vec()))
      .unwrap()
      .id;
    let mut request = AnalysisRequest::new(snapshot);
    request.initial = InitialEnvironment::empty();
    request.initial.cwd = Some(b"/entry".to_vec());
    let mut inputs = Inputs {
      documents: BTreeMap::new(),
      sources: Vec::new(),
      queries: Vec::new(),
      stop: None,
      fail: false,
    };
    let count = 80;
    for index in 0..count {
      let body = if index + 1 == count {
        "value=finished; return; value=unreachable".into()
      } else {
        format!("next=./lib{}; . \"$next\"", index + 1)
      };
      inputs.documents.insert(
        format!("./lib{index}").into_bytes(),
        Arc::new(support::parse(index + 2, 1, &body)),
      );
    }
    let mut workspace = Workspace::default();
    workspace.insert(root).unwrap();
    let result = workspace
      .analyze(request, &mut inputs, &AtomicBool::new(false))
      .unwrap();
    assert_eq!(
      inputs.sources,
      (0..count)
        .map(|index| format!("./lib{index}").into_bytes())
        .collect::<Vec<_>>()
    );
    assert_eq!(inputs.queries.len(), 1);
    assert_eq!(workspace.documents().count(), 81);
    assert_eq!(
      result
        .values(read)
        .map(|(_, value)| value.clone())
        .collect::<Vec<_>>(),
      [Value::Literal(b"finished".to_vec())]
    );
    assert_eq!(result.source_inputs().count(), 80);
    workspace.publish_analysis(result).unwrap();
  }

  #[test]
  fn cancellation_and_provider_failure_discard_documents_loaded_during_analysis()
   {
    for source in [". /a; . /b", ". /a; cd /next"] {
      for cancel in [false, true] {
        let flag = AtomicBool::new(false);
        let root = support::parse(1, 1, source);
        let mut request = AnalysisRequest::new(root.snapshot());
        request.initial = InitialEnvironment::empty();
        let mut workspace = Workspace::default();
        workspace.insert(root).unwrap();
        let previous = workspace
          .analyze(request.clone(), &mut support::Inputs::default(), &flag)
          .unwrap();
        workspace.publish_analysis(previous).unwrap();
        let published = std::ptr::from_ref(workspace.analysis().unwrap());
        let mut inputs = Inputs {
          documents: BTreeMap::from([
            (b"/a".to_vec(), Arc::new(support::parse(2, 1, "a=1"))),
            (b"/b".to_vec(), Arc::new(support::parse(3, 1, "b=1"))),
          ]),
          sources: Vec::new(),
          queries: Vec::new(),
          stop: cancel.then_some(&flag),
          fail: !cancel,
        };
        let error = workspace.analyze(request, &mut inputs, &flag).unwrap_err();
        assert_eq!(
          error,
          if cancel {
            Error::Cancelled
          } else {
            Error::Input("Input provider failed".into())
          }
        );
        assert_eq!(workspace.documents().count(), 1);
        assert_eq!(
          std::ptr::from_ref(workspace.analysis().unwrap()),
          published
        );
      }
    }
  }

  #[test]
  fn sources_indexed_during_analysis_keep_the_analysis_derived_documents() {
    for (entry, name, reader, definitions) in [
      (
        "read x\n. /lib\n: \"$x\"\n",
        b"x".as_slice(),
        "read x\n. /lib\n: \"$x\"\n",
        vec!["x="],
      ),
      (
        "cmd=': \"$y\"'\ntrap \"$cmd\" EXIT\n. /lib\ny=set\n",
        b"y",
        ": \"$y\"",
        Vec::new(),
      ),
    ] {
      let root = support::parse(1, 1, entry);
      let mut request = AnalysisRequest::new(root.snapshot());
      request.initial = InitialEnvironment::empty();
      let mut inputs = Inputs {
        documents: BTreeMap::from([(
          b"/lib".to_vec(),
          Arc::new(support::parse(2, 1, "command -v true\n")),
        )]),
        sources: Vec::new(),
        queries: Vec::new(),
        stop: None,
        fail: false,
      };
      let mut workspace = Workspace::default();
      workspace.insert(root).unwrap();
      let result = workspace
        .analyze(request, &mut inputs, &AtomicBool::new(false))
        .unwrap();
      let text = |site: &Occurrence| {
        let document = workspace.document(site.location.snapshot.document);
        document.unwrap().source().to_owned()
      };
      let read = workspace
        .documents()
        .flat_map(|document| document.occurrences().iter())
        .find(|site| {
          site.kind == OccurrenceKind::VariableExpansion
            && site.name == Value::Literal(name.to_vec())
        })
        .unwrap();
      assert_eq!(text(read), reader, "{entry}");
      let actual: Vec<_> = result
        .resolutions(UseSite::Syntax(read.id))
        .flat_map(|use_site| use_site.resolution.candidates.iter())
        .map(|id| {
          let symbol = workspace.symbol(*id).expect("defined symbol");
          text(workspace.occurrence(symbol.definition).unwrap())
        })
        .collect();
      assert_eq!(actual, definitions, "{entry}");
      workspace.publish_analysis(result).unwrap();
    }
  }
}

mod names {
  use crate::support;
  use shellsmith::interface::application::engine::{
    EffectTarget, NameEnvironment, NameSpelling, SymbolKind, Value,
  };
  use std::sync::atomic::AtomicBool;

  #[test]
  fn trap_bodies_keep_redefined_functions_unknown_and_reuse_body_snapshots() {
    let source = r#"x=before; cleanup() { :; }; register() { trap 'cleanup; : "$x"; read target; exit 1' INT EXIT; }; register; register; cleanup() { :; }; cleanup; : "$x""#;
    let (mut workspace, analysis) = support::analyze(source);
    let contexts = analysis.contexts().to_vec();
    assert_eq!(contexts.len(), 6);
    assert_eq!(
      contexts
        .iter()
        .filter(|context| support::deferred(&analysis, context.id))
        .count(),
      2
    );
    assert_eq!(workspace.documents().count(), 3);
    let body = workspace
      .documents()
      .find(|document| document.source().starts_with("cleanup;"))
      .unwrap();
    let snapshot = body.snapshot();
    let cleanup = body
      .occurrences()
      .iter()
      .find(|site| site.name == Value::Literal(b"cleanup".to_vec()))
      .unwrap()
      .id;
    let uses: Vec<_> = analysis
      .resolutions(shellsmith::interface::application::engine::UseSite::Syntax(
        cleanup,
      ))
      .collect();
    assert_eq!(uses.len(), 2);
    for site in uses {
      assert!(!site.resolution.is_complete());
      assert!(site.resolution.candidates.is_empty());
    }
    workspace.publish_analysis(analysis).unwrap();
    let names = workspace.name_relations(&AtomicBool::new(false)).unwrap();
    let uses: Vec<_> = names
      .occurrences
      .iter()
      .filter(|site| site.site.syntax() == Some(cleanup))
      .collect();
    assert_eq!(uses.len(), 2);
    for site in uses {
      assert!(!site.resolution.as_ref().unwrap().is_complete());
      assert_eq!(
        &source[site.spelling.editable().unwrap().bytes.clone()],
        "cleanup"
      );
    }
    assert_eq!(workspace.analysis().unwrap().contexts(), contexts);
    let request = workspace.analysis().unwrap().request().clone();
    let again = workspace
      .analyze(
        request,
        &mut support::Inputs::default(),
        &AtomicBool::new(false),
      )
      .unwrap();
    assert_eq!(workspace.documents().count(), 3);
    assert_eq!(again.contexts(), contexts);
    assert!(workspace.document(snapshot.document).is_some());
  }

  #[test]
  fn deferred_calls_separate_name_dependencies_from_environment_changes() {
    for body in [
      "uname",
      "(uname)",
      "{ uname; } &",
      "value=$(uname)",
      "local_fn() { uname; }; local_fn",
    ] {
      let source = format!("f() {{ :; }}; idle() {{ {body}\n }}; f");
      let (mut workspace, analysis) = support::analyze(&source);
      let contexts = analysis.contexts().to_vec();
      workspace.publish_analysis(analysis).unwrap();
      let names = workspace.name_relations(&AtomicBool::new(false)).unwrap();
      let target = names.names.iter().find(|name| name.text == b"f").unwrap();
      assert!(
        names
          .effects
          .iter()
          .any(|effect| effect.targets.contains(&EffectTarget::Functions)),
        "{body}"
      );
      assert!(
        names.effects.iter().all(
          |effect| !names.may_inherit(effect.environment, target.environment)
        ),
        "{body}"
      );
      assert!(
        names.dependencies.iter().any(|dependency| {
          dependency.targets
            == [EffectTarget::Function(b"uname".to_vec())].into()
            && names.may_depend(dependency.environment, target.environment)
        }),
        "{body}"
      );
      assert!(
        names.dependencies.iter().all(|dependency| !dependency
          .targets
          .contains(&EffectTarget::Functions)),
        "{body}"
      );
      assert_eq!(workspace.analysis().unwrap().contexts(), contexts);
    }
  }

  #[test]
  fn uncalled_functions_keep_syntax_dependencies_without_resolving_local_calls()
  {
    for body in [
      "unset -f other",
      "other() { :; }",
      "unset -f \"$target\"",
      "(unset -f other)",
    ] {
      let source = format!("idle() {{ (clean() {{ {body}; }}; clean); }}");
      let (mut workspace, analysis) = support::analyze(&source);
      workspace.publish_analysis(analysis).unwrap();
      let names = workspace.name_relations(&AtomicBool::new(false)).unwrap();
      let offset = source.rfind("clean)").unwrap();
      let site = names
        .occurrences
        .iter()
        .find(|site| site.location.bytes.start == offset)
        .unwrap();
      assert_eq!(site.context, None);
      assert_eq!(site.resolution, None);
      assert_eq!(site.identity, None);
      assert!(names.dependencies.iter().any(|dependency| {
        dependency.location.bytes.start == offset
          && dependency
            .targets
            .contains(&EffectTarget::Function(b"clean".to_vec()))
      }));
      assert_eq!(workspace.analysis().unwrap().contexts().len(), 1);
    }
  }

  #[test]
  fn uncalled_argument_names_are_reported_once() {
    let source = "idle() { readonly x; unset -f helper; }";
    let (mut workspace, analysis) = support::analyze(source);
    workspace.publish_analysis(analysis).unwrap();
    let names = workspace.name_relations(&AtomicBool::new(false)).unwrap();
    let arguments: Vec<_> = names
      .occurrences
      .iter()
      .filter(|site| site.site.syntax().is_none())
      .map(|site| &source[site.location.bytes.clone()])
      .collect();
    assert_eq!(arguments, ["x", "helper"]);
  }

  #[test]
  fn uncalled_functions_keep_uses_unattempted_in_all_control_flows() {
    use shellsmith::interface::application::engine::{DocumentId, UseSite};
    for body in [
      "a() { :; }",
      "if :; then a() { :; }; else a() { :; }; fi",
      "if :; then a() { :; }; fi",
      "a() { :; }; while :; do a() { :; }; done",
      "a() { :; }; unset -f a",
      "a() { :; }; a() { :; }",
      "a() { :; }; (a() { :; })",
      "a() { :; }; value=$(a() { :; })",
      "a() { :; }; { unset -f a; } &",
      "install() { a() { :; }; }; install",
      "a() { :; }; clean() { unset -f a; }; clean",
      "a() { :; }; case $x in one) a() { :; };; esac",
    ] {
      let source = format!("idle() {{ {body}\na # target\n}}");
      let (mut workspace, analysis) = support::analyze(&source);
      let document = workspace.document(DocumentId(1)).unwrap();
      assert!(!document.has_syntax_errors(), "{body}");
      let snapshot = document.snapshot();
      let offset = source.find("a # target").unwrap();
      let occurrence = document
        .occurrences()
        .iter()
        .find(|site| site.location.bytes.start == offset)
        .unwrap()
        .id;
      assert_eq!(
        analysis.resolutions(UseSite::Syntax(occurrence)).count(),
        0,
        "{body}"
      );
      assert_eq!(analysis.contexts().len(), 1, "{body}");
      workspace.publish_analysis(analysis).unwrap();
      assert_eq!(
        workspace.resolution_at(snapshot, offset).unwrap(),
        None,
        "{body}"
      );
      let names = workspace.name_relations(&AtomicBool::new(false)).unwrap();
      let sites = names.at(snapshot, offset);
      assert_eq!(sites.len(), 1, "{body}");
      assert_eq!(sites[0].1.context, None, "{body}");
      assert_eq!(sites[0].1.resolution, None, "{body}");
      assert_eq!(sites[0].1.identity, None, "{body}");
      assert_eq!(workspace.analysis().unwrap().contexts().len(), 1, "{body}");
    }
  }

  #[test]
  fn uncalled_bodies_do_not_inherit_definitions_or_change_the_entry() {
    use shellsmith::interface::application::engine::DocumentId;
    let source = "a() { :; }; value=entry; idle() { a; value=body; unset -f a; }; a; : \"$value\"";
    let (mut workspace, analysis) = support::analyze(source);
    let snapshot = workspace.document(DocumentId(1)).unwrap().snapshot();
    let contexts = analysis.contexts().to_vec();
    let definition = workspace.symbols_named(b"a").next().unwrap().id;
    workspace.publish_analysis(analysis).unwrap();
    let inside = workspace
      .resolution_at(snapshot, source.find("{ a;").unwrap() + 2)
      .unwrap();
    assert_eq!(inside, None);
    let after = workspace
      .resolution_at(snapshot, source.rfind("a;").unwrap())
      .unwrap()
      .unwrap();
    assert_eq!(after.candidates, [definition].into());
    assert!(after.issues.is_empty());
    let read = workspace
      .document(DocumentId(1))
      .unwrap()
      .occurrences()
      .iter()
      .find(|site| site.location.bytes.start == source.rfind("value").unwrap())
      .unwrap()
      .id;
    assert!(
      workspace
        .analysis()
        .unwrap()
        .values(read)
        .all(|(_, value)| value == &Value::Literal(b"entry".to_vec()))
    );
    workspace.name_relations(&AtomicBool::new(false)).unwrap();
    assert_eq!(workspace.analysis().unwrap().contexts(), contexts);
  }

  #[test]
  fn uncalled_names_keep_caller_dependencies_separate_from_closed_scopes() {
    let source = "x=outer; idle() { : $x; (helper() { : $x; }; helper); }";
    let (mut workspace, analysis) = support::analyze(source);
    let contexts = analysis.contexts().to_vec();
    workspace.publish_analysis(analysis).unwrap();
    let names = workspace.name_relations(&AtomicBool::new(false)).unwrap();
    assert_eq!(workspace.analysis().unwrap().contexts(), contexts);
    let x: Vec<_> = names
      .names
      .iter()
      .filter(|name| name.text == b"x")
      .collect();
    assert_eq!(x.len(), 3);
    assert_eq!(
      x.iter()
        .filter(|name| matches!(
          name.environment,
          NameEnvironment::Execution(_)
        ))
        .count(),
      1
    );
    assert_eq!(
      x.iter()
        .filter(|name| matches!(name.environment, NameEnvironment::Caller(_)))
        .count(),
      2
    );
    let helper = names
      .names
      .iter()
      .find(|name| name.text == b"helper")
      .unwrap();
    assert!(matches!(helper.environment, NameEnvironment::Isolated(_)));
    assert_eq!(helper.definitions.len(), 1);
    let sites: Vec<_> = names
      .occurrences
      .iter()
      .filter(|site| site.names.contains(&helper.id))
      .collect();
    assert_eq!(sites.len(), 2);
    assert!(sites.iter().all(|site| site.context.is_none()
      && site.resolution.is_none()
      && site.identity.is_none()));
    assert!(
      names
        .occurrences
        .iter()
        .filter(|site| site.context.is_none()
          && site.names.iter().any(|id| names.names[id.0].text == b"x"))
        .all(|site| site.resolution.is_none() && site.identity.is_none())
    );
  }

  #[test]
  fn names_group_redefinitions_but_distinguish_subshell_and_temporary_environments()
   {
    let (mut workspace, analysis) =
      support::analyze("x=1; x=2; unset x; x=3; : $x; (x=4; : $x); x=5 true");
    workspace.publish_analysis(analysis).unwrap();
    let names = workspace.name_relations(&AtomicBool::new(false)).unwrap();
    let mut definitions: Vec<_> = names
      .names
      .iter()
      .filter(|name| name.kind == SymbolKind::Variable && name.text == b"x")
      .map(|name| name.definitions.len())
      .collect();
    definitions.sort_unstable();
    assert_eq!(definitions, [1, 1, 3]);
    assert_eq!(
      names
        .names
        .iter()
        .filter(|name| name.text == b"x" && name.temporary.is_some())
        .count(),
      1
    );
    assert!(
      names
        .occurrences
        .iter()
        .filter(|site| site
          .names
          .iter()
          .any(|id| names.names[id.0].text == b"x"))
        .all(
          |site| site.context.is_some() && site.spelling.editable().is_some()
        )
    );
  }

  #[test]
  fn prefixes_are_temporary_names_only_when_no_special_builtin_may_run() {
    for (source, temporary) in [
      ("y=$(external); x=1 ex\"$y\"t", 0),
      ("y=$(external); x=1 tr\"$y\"e", 1),
    ] {
      let (mut workspace, analysis) = support::analyze(source);
      workspace.publish_analysis(analysis).unwrap();
      let names = workspace.name_relations(&AtomicBool::new(false)).unwrap();
      assert_eq!(
        names
          .names
          .iter()
          .filter(|name| name.text == b"x" && name.temporary.is_some())
          .count(),
        temporary,
        "{source}"
      );
    }
  }

  #[test]
  fn confirmed_name_identities_do_not_merge_partially_matching_candidates() {
    let source = "x=1; (: $x); (if condition; then x=2; fi; : $x)";
    let (mut workspace, analysis) = support::analyze(source);
    workspace.publish_analysis(analysis).unwrap();
    let names = workspace.name_relations(&AtomicBool::new(false)).unwrap();
    let at = |offset| {
      names
        .occurrences
        .iter()
        .find(|site| site.location.bytes.start == offset)
        .unwrap()
    };
    let outer = at(0).identity.unwrap();
    let inherited = at(source.find("$x").unwrap() + 1);
    assert_ne!(inherited.name, Some(outer));
    assert_eq!(inherited.identity, Some(outer));
    let uncertain = at(source.rfind("$x").unwrap() + 1);
    assert!(uncertain.names.contains(&outer));
    assert!(uncertain.names.len() > 1);
    assert_eq!(uncertain.identity, None);
    let local = at(source.find("x=2").unwrap()).identity.unwrap();
    assert_ne!(local, outer);
  }

  #[test]
  fn name_spellings_distinguish_literal_words_from_generated_command_names() {
    let source = "fun() { :; }; f'u'n; command_name=fun; \"$command_name\"";
    let (mut workspace, analysis) = support::analyze(source);
    workspace.publish_analysis(analysis).unwrap();
    let names = workspace.name_relations(&AtomicBool::new(false)).unwrap();
    let function = names
      .names
      .iter()
      .find(|name| name.kind == SymbolKind::Function && name.text == b"fun")
      .unwrap();
    let sites: Vec<_> = names
      .occurrences
      .iter()
      .filter(|site| site.names.contains(&function.id))
      .collect();
    assert_eq!(sites.len(), 3);
    assert!(sites.iter().any(|site| matches!(&site.spelling, NameSpelling::Exact(location) if source[location.bytes.clone()] == *"fun")));
    assert!(sites.iter().any(|site| matches!(&site.spelling, NameSpelling::LiteralWord(location) if source[location.bytes.clone()] == *"f'u'n")));
    assert!(
      sites
        .iter()
        .any(|site| site.spelling == NameSpelling::Generated)
    );
    let navigation: Vec<_> = sites
      .iter()
      .filter_map(|site| site.navigation())
      .map(|location| &source[location.bytes])
      .collect();
    assert_eq!(navigation, ["fun", "f'u'n"]);
  }
}
