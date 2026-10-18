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
      "x=0; command \"$(unknown)\"; x=1; : \"$x\"",
      "x=0; fc -s; x=1; : \"$x\"",
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
    for source in ["x=0; x=1 y=$x external", "x=0; : $(( (x=1) + x ))"] {
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
    let root = support::parse(1, 1, "x=1; . lib; x=2; . lib; : \"$x\"");
    let library = support::parse(2, 1, ": \"$x\"; x=3");
    let root_definitions: Vec<_> =
      root.symbols().iter().map(|symbol| symbol.id).collect();
    let library_definition = library.symbols()[0].id;
    let target = library.snapshot();
    let references: Vec<_> =
      root.references().iter().map(|site| site.id).collect();
    let mut request = AnalysisRequest::new(root.snapshot());
    request.initial = InitialEnvironment::empty();
    let mut workspace = Workspace::default();
    workspace.insert(root).unwrap();
    workspace.insert(library).unwrap();
    for reference in references {
      workspace
        .set_reference(reference, DependencyState::Resolved(target))
        .unwrap();
    }
    workspace.set_analysis_request(request).unwrap();
    let analysis = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
      "load() { . lib; }; x=1; load; x=2; load; f; : \"$x\"",
    );
    let library = support::parse(2, 1, ": \"$x\"; f() { x=3; }; return; x=4");
    let mut request = AnalysisRequest::new(root.snapshot());
    request.initial = InitialEnvironment::empty();
    let reference = root.references()[0].id;
    let target = library.snapshot();
    let mut workspace = Workspace::default();
    workspace.insert(root).unwrap();
    workspace.insert(library).unwrap();
    workspace
      .set_reference(reference, DependencyState::Resolved(target))
      .unwrap();
    workspace.set_analysis_request(request).unwrap();
    let analysis = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
  fn source_cycles_and_failed_or_invalidated_bindings_preserve_unknown_effects()
  {
    let root = support::parse(1, 1, "x=1; . lib; x=2; : \"$x\"");
    let library = support::parse(2, 1, "again() { . root; }; again");
    let entry = root.snapshot();
    let target = library.snapshot();
    let root_reference = root.references()[0].id;
    let library_reference = library.references()[0].id;
    let mut request = AnalysisRequest::new(entry);
    request.initial = InitialEnvironment::empty();
    let mut workspace = Workspace::default();
    workspace.insert(root).unwrap();
    workspace.insert(library).unwrap();
    workspace.set_analysis_request(request).unwrap();
    workspace
      .set_reference(root_reference, DependencyState::Resolved(target))
      .unwrap();
    workspace
      .set_reference(library_reference, DependencyState::Resolved(entry))
      .unwrap();
    let cyclic = workspace.analyze(&AtomicBool::new(false)).unwrap();
    assert!(
      reads(&workspace, &cyclic, b"x")[0]
        .resolution
        .issues
        .contains(&ResolutionIssue::Recursion)
    );
    for state in [
      DependencyState::Unresolved("dynamic target".into()),
      DependencyState::Failed("read failed".into()),
    ] {
      workspace.set_reference(root_reference, state).unwrap();
      let failed = workspace.analyze(&AtomicBool::new(false)).unwrap();
      assert!(
        reads(&workspace, &failed, b"x")[0]
          .resolution
          .issues
          .contains(&ResolutionIssue::UnresolvedSource)
      );
    }
    workspace
      .set_reference(root_reference, DependencyState::Resolved(target))
      .unwrap();
    workspace.insert(support::parse(2, 2, "x=3")).unwrap();
    let invalidated = workspace.analyze(&AtomicBool::new(false)).unwrap();
    assert!(
      reads(&workspace, &invalidated, b"x")[0]
        .resolution
        .issues
        .contains(&ResolutionIssue::UnresolvedSource)
    );
  }

  #[test]
  fn source_and_function_calls_do_not_make_caller_loops_lexical() {
    let root = support::parse(
      1,
      1,
      "x=0; while condition; do . lib; x=1; done; : \"$x\"",
    );
    let library = support::parse(2, 1, "break");
    let mut request = AnalysisRequest::new(root.snapshot());
    request.initial = InitialEnvironment::empty();
    let reference = root.references()[0].id;
    let target = library.snapshot();
    let mut workspace = Workspace::default();
    workspace.insert(root).unwrap();
    workspace.insert(library).unwrap();
    workspace
      .set_reference(reference, DependencyState::Resolved(target))
      .unwrap();
    workspace.set_analysis_request(request).unwrap();
    let analysis = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
    let (mut workspace, analysis) = analyze("x=1; : \"$x\"");
    let old = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
    workspace.set_analysis_request(request.clone()).unwrap();
    let old = workspace.analyze(&AtomicBool::new(false)).unwrap();
    request
      .initial
      .variables
      .insert(b"external".to_vec(), InitialBinding::External);
    workspace.set_analysis_request(request).unwrap();
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
    workspace.set_analysis_request(request).unwrap();
    let analysis = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
    workspace.set_analysis_request(request).unwrap();
    let unknown = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
  fn dynamic_source_bindings_require_matching_values_from_the_current_analysis()
  {
    let document = support::parse(1, 1, ". \"$HOME/lib.sh\"; : \"$loaded\"\n");
    let reference = document.references()[0].id;
    let mut request = AnalysisRequest::new(document.snapshot());
    request.initial = InitialEnvironment::empty();
    request
      .initial
      .environment
      .insert(b"HOME".to_vec(), b"/initial".to_vec());
    let library = support::parse(2, 1, "loaded=1\n");
    let definition = library.symbols()[0].id;
    let target = library.snapshot();
    let mut workspace = Workspace::default();
    workspace.insert(document).unwrap();
    workspace.insert(library).unwrap();
    workspace
      .set_reference(reference, DependencyState::Resolved(target))
      .unwrap();
    for (value, resolves) in [
      (None, false),
      (Some(b"/initial/lib.sh".to_vec()), true),
      (Some(b"/other/lib.sh".to_vec()), false),
    ] {
      request.source_inputs.clear();
      if let Some(value) = value {
        request.source_inputs.insert(
          reference,
          shellsmith::interface::application::engine::SourceInput {
            operand: Value::Literal(value),
            annotation: None,
            cwd: None,
            search_path: None,
          },
        );
      }
      workspace.set_analysis_request(request.clone()).unwrap();
      let result = workspace.analyze(&AtomicBool::new(false)).unwrap();
      assert_eq!(result.references(definition).count(), usize::from(resolves));
    }
  }
}

mod updates {
  use crate::support;

  use shellsmith::interface::application::engine::{
    Analysis, AnalysisRequest, DependencyState, DocumentId, Error,
    InitialEnvironment, OccurrenceKind, ResolutionIssue, ResolutionStatus,
    Value, Workspace,
  };
  use std::sync::atomic::AtomicBool;

  fn workspace(target: u64, reverse: bool) -> Workspace {
    let root = support::parse(1, 1, ". lib; : \"$x\"; dormant() { : \"$y\"; }");
    let first = support::parse(2, 1, "x=1");
    let second = support::parse(3, 1, "x=2");
    let reference = root.references()[0].id;
    let mut request = AnalysisRequest::new(root.snapshot());
    request.initial = InitialEnvironment::empty();
    let mut documents = vec![root, first, second];
    if reverse {
      documents.reverse();
    }
    let mut workspace = Workspace::default();
    for document in documents {
      workspace.insert(document).unwrap();
    }
    let target = workspace.document(DocumentId(target)).unwrap().snapshot();
    workspace
      .set_reference(reference, DependencyState::Resolved(target))
      .unwrap();
    workspace.set_analysis_request(request).unwrap();
    workspace
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
    let mut updated = workspace(2, false);
    let old = updated.analyze(&AtomicBool::new(false)).unwrap();
    let published = updated.analyze(&AtomicBool::new(false)).unwrap();
    updated.publish_analysis(published).unwrap();
    let root = updated.document(DocumentId(1)).unwrap();
    let reference = root.references()[0].id;
    let target = updated.document(DocumentId(3)).unwrap().snapshot();
    updated
      .set_reference(reference, DependencyState::Resolved(target))
      .unwrap();
    assert!(updated.analysis().is_none());
    assert_eq!(updated.publish_analysis(old), Err(Error::StaleAnalysis));
    let new = updated.analyze(&AtomicBool::new(false)).unwrap();
    let mut fresh = workspace(3, true);
    let fresh_analysis = fresh.analyze(&AtomicBool::new(false)).unwrap();
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
    let mut workspace = workspace(2, false);
    let old = workspace.analyze(&AtomicBool::new(false)).unwrap();
    let target = workspace.document(DocumentId(2)).unwrap().snapshot();
    let reference =
      workspace.document(DocumentId(1)).unwrap().references()[0].id;
    workspace.remove(target).unwrap();
    let missing = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
    let analysis = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
    workspace.set_analysis_request(request).unwrap();
    let analysis = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
    let mut workspace = workspace(2, false);
    let report = workspace.analyze(&AtomicBool::new(false)).unwrap();
    let mut invalid = report.request().clone();
    invalid.limits.value_bytes = 0;
    workspace.publish_analysis(report).unwrap();
    assert!(matches!(
      workspace.set_analysis_request(invalid),
      Err(Error::InvalidAnalysis(_))
    ));
    assert!(matches!(
      workspace.analyze(&AtomicBool::new(true)),
      Err(Error::Cancelled)
    ));
    assert!(workspace.analysis().is_some());
    let old = workspace.analyze(&AtomicBool::new(false)).unwrap();
    let mut changed = old.request().clone();
    changed.limits.value_bytes += 1;
    workspace.set_analysis_request(changed).unwrap();
    assert!(workspace.analysis().is_none());
    assert_eq!(workspace.publish_analysis(old), Err(Error::StaleAnalysis));
    let old = workspace.analyze(&AtomicBool::new(false)).unwrap();
    let mut changed = old.request().clone();
    changed.entry = workspace.document(DocumentId(3)).unwrap().snapshot();
    workspace.set_analysis_request(changed).unwrap();
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
      workspace.set_analysis_request(request.clone()).unwrap();
      let damaged = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
      workspace.set_analysis_request(request).unwrap();
      let repaired = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
    workspace.set_analysis_request(request).unwrap();
    let cancelled = AtomicBool::new(false);
    std::thread::scope(|scope| {
      scope.spawn(|| {
        std::thread::sleep(Duration::from_millis(1));
        cancelled.store(true, Ordering::Relaxed);
      });
      assert!(matches!(
        workspace.analyze(&cancelled),
        Err(Error::Cancelled)
      ));
    });
    assert!(workspace.analysis().is_none());
  }
}

mod stack {
  use crate::support;

  use shellsmith::interface::application::engine::{
    Analysis, AnalysisLimits, AnalysisRequest, DependencyState, DocumentId,
    InitialEnvironment, OccurrenceKind, ResolutionStatus, Value, Workspace,
  };
  use std::fmt::Write;
  use std::sync::atomic::AtomicBool;

  fn analyze_on_small_stack(
    mut workspace: Workspace,
    limits: AnalysisLimits,
  ) -> (Workspace, Analysis) {
    let entry = workspace.document(DocumentId(1)).unwrap().snapshot();
    let mut request = AnalysisRequest::new(entry);
    request.initial = InitialEnvironment::empty();
    request.limits = limits;
    workspace.set_analysis_request(request).unwrap();
    std::thread::Builder::new()
      .stack_size(256 * 1024)
      .spawn(move || {
        let analysis = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
      let (workspace, analysis) =
        analyze_on_small_stack(workspace(&source), AnalysisLimits::default());
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
      let (workspace, analysis) =
        analyze_on_small_stack(workspace(&source), AnalysisLimits::default());
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
    let (workspace, analysis) =
      analyze_on_small_stack(workspace(&source), AnalysisLimits::default());
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
      .insert(support::parse(1, 1, ". lib2; : \"$x\"; . lib2; : \"$x\"\n"))
      .unwrap();
    for id in 2..=documents {
      let source = if id == documents {
        "x=1; return; x=2\n".to_owned()
      } else {
        format!(". lib{}; return; x=3\n", id + 1)
      };
      workspace.insert(support::parse(id, 1, &source)).unwrap();
    }
    for id in 1..documents {
      let references: Vec<_> = workspace
        .document(DocumentId(id))
        .unwrap()
        .references()
        .iter()
        .map(|reference| reference.id)
        .collect();
      let target = workspace.document(DocumentId(id + 1)).unwrap().snapshot();
      for reference in references {
        workspace
          .set_reference(reference, DependencyState::Resolved(target))
          .unwrap();
      }
    }
    let (workspace, analysis) =
      analyze_on_small_stack(workspace, AnalysisLimits::default());
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
    let (workspace, analysis) =
      analyze_on_small_stack(workspace(&source), AnalysisLimits::default());
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
    let (workspace, analysis) =
      analyze_on_small_stack(workspace(&source), AnalysisLimits::default());
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
    InputQuery, InputResult, SourceDocument, SourceInput, Value, Workspace,
  };
  use std::collections::BTreeMap;
  use std::sync::Arc;
  use std::sync::atomic::{AtomicBool, Ordering};

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
      .analyze_with(request, &mut inputs, &AtomicBool::new(false))
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
    for cancel in [false, true] {
      let flag = AtomicBool::new(false);
      let root = support::parse(1, 1, ". /a; . /b");
      let mut request = AnalysisRequest::new(root.snapshot());
      request.initial = InitialEnvironment::empty();
      let mut workspace = Workspace::default();
      workspace.insert(root).unwrap();
      workspace.set_analysis_request(request.clone()).unwrap();
      let previous = workspace.analyze(&flag).unwrap();
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
      let error = workspace
        .analyze_with(request, &mut inputs, &flag)
        .unwrap_err();
      assert_eq!(
        error,
        if cancel {
          Error::Cancelled
        } else {
          Error::Input("Input provider failed".into())
        }
      );
      assert_eq!(workspace.documents().count(), 1);
      assert_eq!(std::ptr::from_ref(workspace.analysis().unwrap()), published);
    }
  }
}

mod names {
  use crate::support;
  use shellsmith::interface::application::engine::{
    EffectTarget, NameEnvironment, NameSpelling, PartialValue, SymbolKind,
    Value, ValueConstraint, ValueIssue,
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
    let again = workspace.analyze(&AtomicBool::new(false)).unwrap();
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
  fn uncalled_function_effects_keep_names_and_environment_boundaries() {
    for (body, exact, pattern, all) in [
      ("unset -f other", vec!["other"], false, false),
      ("unset other", vec!["other"], false, false),
      (
        "if :; then unset -f a; else b() { :; }; fi",
        vec!["a", "b"],
        false,
        false,
      ),
      ("unset -f \"tmp_$suffix\"", vec![], true, false),
      (
        "other() { unset -f \"$target\"; }",
        vec!["other"],
        false,
        false,
      ),
      ("(unset -f \"$target\")", vec![], false, false),
      ("value=$(unset -f \"$target\")", vec![], false, false),
      ("{ unset -f \"$target\"; } &", vec![], false, false),
      ("unset -f \"$target\" | :", vec![], false, true),
      ("\"/bin/$tool\"", vec![], false, false),
      ("uname", vec![], false, true),
      ("\"$tool\"", vec![], false, true),
      ("\".$suffix\" ./unknown", vec![], false, true),
      (
        "eval \"$code\"; trap 'unset -f other' EXIT",
        vec![],
        false,
        false,
      ),
    ] {
      let source = format!("idle() {{ (clean() {{ {body}\n }}; clean); }}");
      let (mut workspace, analysis) = support::analyze(&source);
      workspace.publish_analysis(analysis).unwrap();
      let names = workspace.name_relations(&AtomicBool::new(false)).unwrap();
      let targets: std::collections::BTreeSet<_> = names
        .effects
        .iter()
        .filter(|effect| {
          effect.location.bytes.start == source.rfind("clean)").unwrap()
        })
        .flat_map(|effect| effect.targets.iter())
        .collect();
      let actual: Vec<_> = targets
        .iter()
        .filter_map(|target| match target {
          EffectTarget::Function(name) => {
            Some(std::str::from_utf8(name).unwrap())
          }
          _ => None,
        })
        .collect();
      assert_eq!(actual, exact, "{body}");
      assert_eq!(targets.contains(&EffectTarget::Functions), all, "{body}");
      let patterns: Vec<_> = targets
        .iter()
        .filter_map(|target| match target {
          EffectTarget::FunctionPattern(value) => Some(value),
          _ => None,
        })
        .collect();
      assert_eq!(patterns.len(), usize::from(pattern), "{body}");
      for value in patterns {
        assert_eq!(
          value,
          &Value::Partial(PartialValue {
            choices: vec![ValueConstraint {
              prefix: b"tmp_".to_vec(),
              suffix: vec![],
              minimum: 4,
              required: (*b"tmp_").into(),
              exact: false,
            }],
            issue: ValueIssue::Dynamic,
          }),
          "{body}"
        );
      }
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
    assert!(
      sites
        .iter()
        .all(|site| site.context.is_none() && site.resolution.is_some())
    );
    assert_eq!(
      sites[1].resolution.as_ref().unwrap().candidates,
      helper.definitions
    );
    assert!(
      names
        .occurrences
        .iter()
        .filter(|site| site.context.is_none()
          && site.names.iter().any(|id| names.names[id.0].text == b"x"))
        .all(|site| site.resolution.is_none())
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
  }
}
