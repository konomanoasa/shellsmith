mod assignments {
  use crate::support;

  use shellsmith::interface::application::engine::{
    Analysis, AnalysisRequest, DocumentId, InitialEnvironment, OccurrenceKind,
    ResolutionIssue, Value, ValueIssue, Workspace,
  };
  use std::sync::atomic::AtomicBool;

  use support::analyze;

  #[test]
  fn completion_status_crosses_environment_boundaries_without_leaking_variables()
   {
    for (body, expected) in [
      ("f; x=1", "0"),
      ("f; (:)", "0"),
      (":; x=$(f)", "7"),
      ("f; : &", "0"),
      (":; x=$(f) y=$(:)", "0"),
      (":; x=$(:) y=$(f)", "7"),
      (":; >\"$(f)\"", "1"),
      (":; x=$(f) >\"$(f)\"", "1"),
      ("f; : \"$(f)\"", "0"),
      ("f; g() { :; }", "0"),
      ("f; x=$( )", "0"),
      ("f; x=$(# comment\n)", "0"),
    ] {
      let separator = if body.ends_with('&') { " " } else { "; " };
      let source =
        format!("f() {{ return 7; }}; {body}{separator}out=$?; : \"$out\"");
      let (workspace, analysis) = analyze(&source);
      assert_eq!(
        reads(&workspace, &analysis, b"out"),
        [Value::Literal(expected.as_bytes().to_vec())],
        "{source}"
      );
    }
    let (workspace, analysis) =
      analyze("f() { return 7; }; x=$(f) >output; out=$?; : \"$out\"");
    assert_eq!(
      reads(&workspace, &analysis, b"out"),
      [Value::Unknown(ValueIssue::Dynamic)]
    );
    let (workspace, analysis) =
      analyze("out=kept; (out=changed); : \"$(out=other)\"; : \"$out\"");
    assert_eq!(
      reads(&workspace, &analysis, b"out"),
      [Value::Literal(b"kept".to_vec())]
    );
  }

  #[test]
  fn value_constraints_restrict_calls_after_capture_copy_and_transformation() {
    for body in [
      "read suffix; name=tool_$suffix; tool=$(command -v \"$name\"); \"$tool\"",
      "read suffix; tool=$(command -v \"/bin/$suffix\"); \"$tool\"",
      "tool=$(command -v missing); copy=$tool; \"$copy\"",
      "read name; \"/bin/$name\"",
      "read a b; \"$a/middle/$b\"",
      "read name; command \"/bin/$name\"",
      "read name; command -- \"/bin/$name\"",
      "tool=$(command -v missing); command \"$tool\"",
      "tool=$(command -v missing); copy=$(command -p printf '%s' \"$tool\"); \"$copy\"",
      "tool=$(command -v missing); f() { command -p printf '%s' \"$tool\"; }; copy=$(f); \"$copy\"",
      "tool=$(command -v missing); suffix() { keep=no; }; IFS=/; set -- \"$tool\" suffix; \"$*\"",
      "read name; export \"tool=$name/suffix\"; \"$tool\"",
      "read name; tool=$(command -p printf /bin/; command -p printf '%s' \"$name\"); \"$tool\"",
      "read name; tool=$(command -p printf '%s' \"$name\"; command -p printf /suffix); \"$tool\"",
      "if condition; then tool=$(command -v missing); else tool=$(command -v other); fi; \"$tool\"",
      "tool=/bin/; while condition; do tool=${tool}x; done; \"$tool\"",
      "read tail; tool=; while condition; do tool=${tail}x; done; \"$tool\"",
      "f() { read name; \"/bin/$name\"; command -v f; f; }; f",
    ] {
      let source = format!("keep=yes; {body}; out=$keep; : \"$out\"");
      let (workspace, analysis) = analyze(&source);
      assert_eq!(
        reads(&workspace, &analysis, b"out"),
        [Value::Literal(b"yes".to_vec())],
        "{source}"
      );
    }
  }

  #[test]
  fn constraints_keep_possible_shell_effects_and_resolve_names_at_call_time() {
    for (body, expected) in [
      (
        "f() { out=old; }; tool=$(command -v f); f() { out=new; }; \"$tool\"",
        "new",
      ),
      (
        "tool=$(command -v missing); \"${tool}unset\" out; out=${out-no}",
        "no",
      ),
    ] {
      let source = format!("{body}; : \"$out\"");
      let (workspace, analysis) = analyze(&source);
      assert_eq!(
        reads(&workspace, &analysis, b"out").last(),
        Some(&Value::Literal(expected.as_bytes().to_vec())),
        "{source}"
      );
    }
    for body in [
      "tool=$(command -v missing); name=${tool##*/}; \"$name\"",
      "read name; \"$name\"",
      "tool=$(command -v missing); $tool",
      "tool=$(command -V missing); \"$tool\"",
      "read name; \"/bin/$((1/0))\"",
    ] {
      let source = format!("keep=yes; {body}; out=$keep; : \"$out\"");
      let (workspace, analysis) = analyze(&source);
      assert!(
        matches!(
          reads(&workspace, &analysis, b"out").last(),
          Some(Value::Unknown(_))
        ),
        "{source}"
      );
    }
    let (workspace, analysis) = analyze(
      "keep=yes; change() { touched=changed; }; tool=$(if :; then command -v change; else command -v missing; fi); \"$tool\"; : \"$keep\" \"$touched\"",
    );
    assert_eq!(
      reads(&workspace, &analysis, b"keep"),
      [Value::Literal(b"yes".to_vec())]
    );
    assert!(!matches!(
      reads(&workspace, &analysis, b"touched").last(),
      Some(Value::Literal(_))
    ));
  }

  #[test]
  fn partial_values_preserve_nonemptiness_and_restrict_dynamic_name_effects() {
    for source in [
      "read tail; x=\"prefix_$tail\"; out=${x:+yes}; : \"$out\"",
      "read tail; x=\"$tail/suffix\"; out=${x:+yes}; : \"$out\"",
      "read a b; x=\"$a/middle/$b\"; y=$x; out=${y:+yes}; : \"$out\"",
      "keep=yes; read suffix; unset -v \"tmp_$suffix\"; out=$keep; : \"$out\"",
      "keep=yes; read suffix; target=\"tmp_$suffix\"; unset -v \"$target\"; out=$keep; : \"$out\"",
      "read suffix; unset -v \"tmp_$suffix\"; out=${unrelated-yes}; : \"$out\"",
      "out=yes; read suffix; export \"other=$suffix\"; : \"$out\"",
      "keep=yes; read suffix; export \"tmp_$suffix=value\"; out=$keep; : \"$out\"",
      "out=yes; read suffix; getopts a \"tmp_$suffix\" -a; : \"$out\"",
    ] {
      let (workspace, analysis) = analyze(source);
      assert_eq!(
        reads(&workspace, &analysis, b"out"),
        [Value::Literal(b"yes".to_vec())],
        "{source}"
      );
    }
    let (workspace, analysis) = analyze(
      "tmp_x=yes; read suffix; unset -v \"tmp_$suffix\"; out=$tmp_x; : \"$out\"",
    );
    assert!(matches!(
      reads(&workspace, &analysis, b"out").as_slice(),
      [Value::Unknown(_)]
    ));
  }

  #[test]
  fn parameter_length_drops_string_constraints_before_resolving_names() {
    let (workspace, analysis) =
      analyze("read suffix; path=/$suffix; out=${#path}; : \"$out\"");
    assert_eq!(
      reads(&workspace, &analysis, b"out"),
      [Value::Unknown(ValueIssue::Dynamic)]
    );
    let (workspace, analysis) = analyze(
      "x1=kept; read suffix; path=/$suffix; target=x${#path}; unset -v \"$target\"; out=$x1; : \"$out\"",
    );
    assert_eq!(
      reads(&workspace, &analysis, b"out"),
      [Value::Unknown(ValueIssue::Dynamic)]
    );
  }

  #[test]
  fn branch_choices_keep_presence_positionals_and_function_definitions_together()
   {
    for (source, expected) in [
      (
        "if condition; then set -- ab c; else set -- a bc; fi; out=$1$2; : \"$out\"",
        "abc",
      ),
      (
        "if condition; then unset x; y=expected; else x=expected; y=unused; fi; out=${x-$y}; : \"$out\"",
        "expected",
      ),
      (
        "if condition; then f() { out=$x; }; x=ok; else f() { out=$y; }; y=ok; fi; f; : \"$out\"",
        "ok",
      ),
      (
        "if condition; then f() { out=old; }; else f() { out=other; }; fi; f() { out=new; }; f; : \"$out\"",
        "new",
      ),
      (
        "if condition; then set -- a; else set -- b; fi; set -- replaced; out=$1; : \"$out\"",
        "replaced",
      ),
    ] {
      let (workspace, analysis) = analyze(source);
      assert_eq!(
        reads(&workspace, &analysis, b"out"),
        [Value::Literal(expected.as_bytes().to_vec())],
        "{source}"
      );
    }
  }

  #[test]
  fn unchanged_state_keeps_branch_choices_and_readonly_values() {
    let branches = "u=$(external); if condition; then unset x; y=expected; else x=expected; y=unused; fi";
    for step in [
      "readonly x; x=other",
      "readonly x; f() { case $u in a) f; x=1 ;; esac; }; f",
    ] {
      let source = format!("{branches}; {step}; out=${{x-$y}}; : \"$out\"");
      let (workspace, analysis) = analyze(&source);
      assert_eq!(
        reads(&workspace, &analysis, b"out"),
        [Value::Literal(b"expected".to_vec())],
        "{source}"
      );
    }
    for (source, expected) in [
      (
        "if condition; then set -- ok; f() { value=; }; else set --; f() { value=ok; }; fi; read -Z v; f; out=$value${1-}; : \"$out\"",
        "ok",
      ),
      ("readonly x; read -Z v; out=${x-unset}; : \"$out\"", "unset"),
      (
        "readonly x; u=$(external); read \"x$u\"; out=${x-unset}; : \"$out\"",
        "unset",
      ),
    ] {
      let (workspace, analysis) = analyze(source);
      assert_eq!(
        reads(&workspace, &analysis, b"out"),
        [Value::Literal(expected.as_bytes().to_vec())],
        "{source}"
      );
    }
  }

  #[test]
  fn undecoded_definitions_forget_branch_choices_and_keep_readonly_values() {
    for (source, name, expected) in [
      (
        "if condition; then f() { out=a; }; else f() { out=b; }; fi; g() { a | ; }; f; : \"$out\"",
        b"out".as_slice(),
        Value::Unknown(ValueIssue::Dynamic),
      ),
      (
        "readonly r=1; x=$(a | ); : \"$r\"",
        b"r",
        Value::Literal(b"1".to_vec()),
      ),
    ] {
      let document = support::parse(1, 1, source);
      assert!(document.has_syntax_errors(), "{source}");
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
      assert_eq!(reads(&workspace, &analysis, name), [expected], "{source}");
    }
  }

  #[test]
  fn captured_output_uses_shared_results_and_restores_redirections() {
    for (body, expected) in [
      (":", ""),
      ("command -v :", ":"),
      ("f() { :; }; command -v f", "f"),
      ("Version() { :; }; command -v Version", "Version"),
      ("command -p printf %s ./lib.sh", "./lib.sh"),
      ("command -p printf '%s\\n' one two", "one\ntwo"),
      ("f() { command -p printf %s value; }; f", "value"),
      (
        "(command -p printf %s sub); command -p printf %s shell",
        "subshell",
      ),
      (
        "command -p printf ignored >output; command -p printf kept",
        "kept",
      ),
      (
        "{ command -p printf ignored; } >output; command -p printf kept",
        "kept",
      ),
      (
        "command -p printf ignored >&2; command -p printf kept",
        "kept",
      ),
      ("command -p printf '%s' \"$(command -v :)\"", ":"),
      ("command -p printf ignored 1<<'END'\ninput\nEND\n", ""),
      ("exec >output; command -p printf ignored", ""),
      ("command exec >output; command -p printf ignored", ""),
      ("f() { exec >output; }; f; command -p printf ignored", ""),
      (
        "{ exec >other; command -p printf ignored; } >output; command -p printf kept",
        "kept",
      ),
    ] {
      let source = format!("out=$({body}); : \"$out\"");
      let (workspace, analysis) = analyze(&source);
      assert_eq!(
        reads(&workspace, &analysis, b"out"),
        [Value::Literal(expected.as_bytes().to_vec())],
        "{source}"
      );
    }
    for body in [
      "printf value",
      "unknown_utility",
      ". ./missing.sh",
      "command -V :",
      "command -p printf '%d' 5",
      "command -p printf -- %s value",
      "export --",
      "readonly --",
      "set -o -e",
      "f() { f; }; f",
      "command -p printf value & wait",
      "command -p printf value 2>&1",
      "command -p printf value | unknown_utility",
      "<input",
      "<input >output",
    ] {
      let source = format!("out=$({body}); : \"$out\"");
      let (workspace, analysis) = analyze(&source);
      assert!(
        matches!(
          reads(&workspace, &analysis, b"out").as_slice(),
          [Value::Unknown(_)]
        ),
        "{source}"
      );
    }
  }

  #[test]
  fn command_queries_may_report_words_an_implementation_reserves() {
    for name in ["time", "[[", "select", "label:"] {
      let source = format!("out=$(command -v '{name}'); : \"$out\"");
      let (workspace, analysis) = analyze(&source);
      let values = reads(&workspace, &analysis, b"out");
      let [Value::Partial(value)] = values.as_slice() else {
        panic!("{source}: {values:?}");
      };
      assert!(
        value
          .choices
          .iter()
          .any(|choice| choice.exact && choice.prefix == name.as_bytes()),
        "{source}: {value:?}"
      );
    }
  }

  #[test]
  fn input_consumption_survives_temporary_and_isolated_redirections() {
    let mut failures = Vec::new();
    for body in [
      ": <\"$(read -r first; command -p printf input)\"; read -r out",
      "<\"$(read -r first; command -p printf input)\"; read -r out",
      ": <<INNER\n$(read -r first)\nINNER\nread -r out",
      "(read -r first; exec <<'INNER'\nother\nINNER\n); read -r out",
      "ignored=$(read -r first; exec <<'INNER'\nother\nINNER\n); read -r out",
      "read -r first; tool=$(command -v missing); \"$tool\" <<'INNER'\nother\nINNER\nread -r out",
      "read -r first <&0; read -r out",
    ] {
      let source = format!("{{ {body}; }} <<'END'\none\ntwo\nEND\n: \"$out\"");
      let (workspace, analysis) = analyze(&source);
      let actual = reads(&workspace, &analysis, b"out");
      if actual != [Value::Literal(b"two".to_vec())] {
        failures.push(format!("{source}: {actual:?}"));
      }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
  }

  #[test]
  fn read_keeps_unsplit_last_operand_and_escaped_ifs_bytes() {
    let mut failures = Vec::new();
    for (operands, line, expected) in [
      ("out", "value:", "value:"),
      ("out", ":", ":"),
      ("first out", "one:two:", "two:"),
      ("first out", "one:two:three:", "two:three:"),
      ("first out", "one:two::", "two::"),
      ("first out", "one: : ", ":"),
      ("first out", "one:two\\:", "two:"),
      ("first out", "one:two:  ", "two:"),
    ] {
      let source =
        format!("IFS=': '; read {operands} <<'END'\n{line}\nEND\n: \"$out\"");
      let (workspace, analysis) = analyze(&source);
      let actual = reads(&workspace, &analysis, b"out");
      if actual != [Value::Literal(expected.as_bytes().to_vec())] {
        failures.push(format!("{source}: {actual:?}, expected {expected:?}"));
      }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
  }

  #[test]
  fn uncertain_input_and_evaluation_order_do_not_fabricate_values() {
    for source in [
      "{ { read -r out; : \"$out\"; } & wait; } <<'END'\nvalue\nEND\n",
      "f() { return 7; }; x=$(f) >\"$(command -p printf output)\"; out=$?; : \"$out\"",
      "{ : <\"$(unknown_utility)\"; read -r out; } <<'END'\none\ntwo\nEND\n: \"$out\"",
      "{ { if :; then :; else exec <<'INNER'\none\ntwo\nINNER\nfi; read first; } <&0; read -r out; } <<'END'\none\ntwo\nEND\n: \"$out\"",
      "{ out=$(read first; command -p printf %s \"$first\") >\"$(read second; command -p printf output)\"; } <<'END'\none\ntwo\nEND\n: \"$out\"",
      "{ ignored=$(read out; command -p printf %s \"$out\") : <<'INNER'\ninner\nINNER\n} <<'END'\nouter\nEND\n",
    ] {
      let (workspace, analysis) = analyze(source);
      let actual = reads(&workspace, &analysis, b"out");
      assert!(
        matches!(actual.as_slice(), [Value::Unknown(_) | Value::Partial(_)]),
        "{source}: {actual:?}"
      );
    }
  }

  #[test]
  fn implicit_function_unset_invalidates_branch_choices() {
    let source = "out=before; if condition; then f() { out=after; }; else f() { out=after; }; fi; unset f; f; : \"$out\"";
    let (workspace, analysis) = analyze(source);
    assert!(matches!(
      reads(&workspace, &analysis, b"out").as_slice(),
      [Value::Unknown(_) | Value::Partial(_)]
    ));
    let (workspace, analysis) = analyze(
      "f=set; if condition; then f() { out=after; }; else f() { out=after; }; fi; unset f; f; : \"$out\"",
    );
    assert_eq!(
      reads(&workspace, &analysis, b"out"),
      [Value::Literal(b"after".to_vec())]
    );
  }

  #[test]
  fn read_failure_keeps_later_writes_conditional() {
    for source in [
      "out=before; readonly blocked=; read blocked out <<'END'\nignored after\nEND\n: \"$out\"",
      "out=before; if :; then readonly blocked=; fi; read blocked out <<'END'\nignored after\nEND\n: \"$out\"",
    ] {
      let (workspace, analysis) = analyze(source);
      assert!(
        matches!(
          reads(&workspace, &analysis, b"out").as_slice(),
          [Value::Unknown(_) | Value::Partial(_)]
        ),
        "{source}"
      );
    }
    let (workspace, analysis) = analyze(
      "readonly blocked=; read out blocked <<'END'\nbefore ignored\nEND\n: \"$out\"",
    );
    assert_eq!(
      reads(&workspace, &analysis, b"out"),
      [Value::Literal(b"before".to_vec())]
    );
  }

  #[test]
  fn known_input_and_option_arguments_produce_values_without_executing_commands()
   {
    for (source, expected) in [
      ("read -r out <<'END'\n./lib.sh\nEND\n: \"$out\"", "./lib.sh"),
      ("IFS= read out <<'END'\n\\value\nEND\n: \"$out\"", "\\value"),
      (
        "IFS= read out <<'END'\none\\\ntwo\nEND\n: \"$out\"",
        "onetwo",
      ),
      (
        "{ unused=$(command -v :) >output; read -r out; } <<'END'\nvalue\nEND\n: \"$out\"",
        "value",
      ),
      (
        "{ unused=$(command -v :) : <<'INNER'\nother\nINNER\nread -r out; } <<'END'\nvalue\nEND\n: \"$out\"",
        "value",
      ),
      (
        "ignored=$(read -r out <<'INNER'\nvalue\nINNER\ncommand -p printf %s \"$out\") : >output",
        "value",
      ),
      (
        "input=value; read -r out <<END\n$input\nEND\n: \"$out\"",
        "value",
      ),
      (
        "read -r first out <<'END'\n  one  two   three  \nEND\n: \"$out\"",
        "two   three",
      ),
      (
        "IFS=: read -r first out <<'END'\none:two:three\nEND\n: \"$out\"",
        "two:three",
      ),
      (
        "{ read -r first; read -r out; } <<'END'\none\ntwo\nEND\n: \"$out\"",
        "two",
      ),
      (
        "read first out <<'END'\none\\ two three\\\nfour\nEND\n: \"$out\"",
        "threefour",
      ),
      ("read -r out <<END\nfoo\\\nbar\nEND\n: \"$out\"", "foobar"),
      (
        "read -r out <<END\n$(command -p printf value)\nEND\n: \"$out\"",
        "value",
      ),
      (
        "{ (read -r first); read -r out; } <<'END'\none\ntwo\nEND\n: \"$out\"",
        "two",
      ),
      (
        "{ ignored=$(read -r first); read -r out; } <<'END'\none\ntwo\nEND\n: \"$out\"",
        "two",
      ),
      ("exec <<'END'\nvalue\nEND\nread -r out; : \"$out\"", "value"),
      (
        "out=kept; read -d out other <<'END'\nvalue\nEND\n: \"$out\"",
        "kept",
      ),
      (
        "out=kept; read -rd out other <<'END'\nvalue\nEND\n: \"$out\"",
        "kept",
      ),
      ("OPTIND=1; getopts a out -a; : \"$out\"", "a"),
      ("OPTIND=1; getopts -- a out -a; : \"$out\"", "a"),
      (
        "OPTIND=1; getopts ab first -ab; getopts ab out -ab; : \"$out\"",
        "b",
      ),
      (
        "OPTIND=1; getopts a: option -avalue; out=$OPTARG; : \"$out\"",
        "value",
      ),
      (
        "OPTIND=1; set -- -a value; getopts a: option; out=$OPTARG; : \"$out\"",
        "value",
      ),
      (
        "OPTIND=1; getopts ab first -ab; export OPTIND=1; getopts ab out -ab; : \"$out\"",
        "a",
      ),
      (
        "OPTIND=1; getopts ab first -ab; OPTIND=1; getopts ab out -ab; : \"$out\"",
        "a",
      ),
    ] {
      let (workspace, analysis) = analyze(source);
      assert_eq!(
        reads(&workspace, &analysis, b"out"),
        [Value::Literal(expected.as_bytes().to_vec())],
        "{source}"
      );
    }
  }

  #[test]
  fn tab_stripping_here_documents_remove_only_tabs_that_begin_body_lines() {
    for (source, expected) in [
      (
        "IFS= read -r out <<-END\n\t\tvalue\n\tEND\n: \"$out\"",
        "value",
      ),
      (
        "read -r out <<-END\n\tfoo\\\n\tbar\n\tEND\n: \"$out\"",
        "foobar",
      ),
      (
        "tab='\tvalue'; IFS= read -r out <<-END\n\t$tab\n\tEND\n: \"$out\"",
        "\tvalue",
      ),
      (
        "{ IFS= read -r first; IFS= read -r out; } <<-END\n\tone\n\t\ttwo\n\tEND\n: \"$out\"",
        "two",
      ),
      (
        "{ IFS= read -r first; IFS= read -r out; } <<-'END'\n\tone\n\t\ttwo\n\tEND\n: \"$out\"",
        "two",
      ),
    ] {
      let (workspace, analysis) = analyze(source);
      assert_eq!(
        reads(&workspace, &analysis, b"out"),
        [Value::Literal(expected.as_bytes().to_vec())],
        "{source}"
      );
    }
  }

  #[test]
  fn unknown_input_and_invalidated_option_cursors_do_not_reuse_previous_values()
  {
    for source in [
      "{ command -p cat; read -r out; } <<'END'\nvalue\nEND\n: \"$out\"",
      "{ exec <input; read -r out; } <<'END'\nvalue\nEND\n: \"$out\"",
      "OPTIND=1; getopts ab first -ab; read OPTIND; getopts ab out -ab; : \"$out\"",
      "OPTIND=1; getopts ab first -ab; unset OPTIND; OPTIND=2; getopts ab out -ab; : \"$out\"",
      "OPTIND=1; readonly OPTIND; getopts a out -a; : \"$out\"",
      "OPTIND=1; getopts a:: out -a; : \"$out\"",
      "x=prefix$((1/0)); out=${x:+yes}; : \"$out\"",
    ] {
      let (workspace, analysis) = analyze(source);
      assert!(
        matches!(
          reads(&workspace, &analysis, b"out").as_slice(),
          [Value::Unknown(_) | Value::Partial(_)]
        ),
        "{source}: {:?}",
        reads(&workspace, &analysis, b"out")
      );
    }
  }

  fn reads(
    workspace: &Workspace,
    analysis: &Analysis,
    name: &[u8],
  ) -> Vec<Value> {
    workspace
      .document(DocumentId(1))
      .unwrap()
      .occurrences()
      .iter()
      .filter(|site| {
        site.kind == OccurrenceKind::VariableExpansion
          && site.name == Value::Literal(name.to_vec())
      })
      .flat_map(|site| {
        analysis.values(site.id).map(|(context, value)| {
          assert_eq!(analysis.value(site.id, context), Some(value));
          value.clone()
        })
      })
      .collect()
  }

  #[test]
  fn termination_options_keep_values_definitions_and_readonly_attributes() {
    for command in [
      "set -xvabC",
      "set -o xtrace -o verbose -o allexport",
      "set -o pipefail -o nolog",
      "set +o pipefail",
      "set -f",
      "set -o noglob",
      "flags=-eu; set $flags",
      "set -e",
      "set -u",
      "set -eu",
      "set -ue",
      "set +eu",
      "set -e +u",
      "set -eeuu",
      "set -o errexit -o nounset",
      "set +o errexit +o nounset",
      "set -euo nounset",
      "set -uoerrexit",
      "set +eonounset",
      "set",
      "set -o",
      "set +o",
      "set -- \"$@\"",
      "set -eu -- \"$@\"",
      "set \"$absent\"",
      "set operand -f",
      "flags=-eu; set \"$flags\"",
      "name=nounset; set -o \"$name\"",
      "command set -eu",
    ] {
      let source = format!(
        "readonly path=./lib.sh; f() {{ :; }}; {command}; false; : \"$missing\"; : \"$path\"; f"
      );
      let (mut workspace, result) = analyze(&source);
      assert_eq!(
        reads(&workspace, &result, b"path"),
        vec![Value::Literal(b"./lib.sh".to_vec())],
        "{source}"
      );
      assert_eq!(
        reads(&workspace, &result, b"missing"),
        vec![Value::Literal(Vec::new())],
        "{source}"
      );
      assert_eq!(result.gaps(), [], "{source}");
      let path = workspace.symbols_named(b"path").next().unwrap().id;
      assert_eq!(result.references(path).count(), 1, "{source}");
      let function = workspace.symbols_named(b"f").next().unwrap().id;
      assert_eq!(result.references(function).count(), 1, "{source}");
      workspace.publish_analysis(result).unwrap();
      let spans = workspace.semantic_spans(DocumentId(1)).unwrap();
      assert_eq!(
        spans
          .iter()
          .filter(|span| {
            span.modifiers.contains(
              &shellsmith::interface::application::engine::Modifier::Readonly,
            )
          })
          .count(),
        2,
        "{source}"
      );
    }
  }

  #[test]
  fn unknown_fields_keep_known_fields_prefixes_and_failure_reasons() {
    for (source, expected) in [
      (
        "read tail; set -- first \"$tail\" third; path=$1/$3/$#; : \"$path\"",
        Value::Literal(b"first/third/3".to_vec()),
      ),
      (
        "read tail; set -- first\"$tail\"; path=$1; : \"$path\"",
        Value::Partial(
          shellsmith::interface::application::engine::PartialValue {
            choices: vec![
              shellsmith::interface::application::engine::ValueConstraint {
                prefix: b"first".to_vec(),
                suffix: Vec::new(),
                minimum: 5,
                required: b"first".iter().copied().collect(),
                exact: false,
              },
            ],
            issue: ValueIssue::Dynamic,
          },
        ),
      ),
      (
        "read tail; set -- \"$tail\" b; path=$*; : \"$path\"",
        Value::Partial(
          shellsmith::interface::application::engine::PartialValue {
            choices: vec![
              shellsmith::interface::application::engine::ValueConstraint {
                prefix: Vec::new(),
                suffix: b" b".to_vec(),
                minimum: 2,
                required: b" b".iter().copied().collect(),
                exact: false,
              },
            ],
            issue: ValueIssue::Dynamic,
          },
        ),
      ),
      (
        "read tail; set -- \"$tail\"*; path=$#; : \"$path\"",
        Value::Unknown(ValueIssue::Dynamic),
      ),
      (
        "if condition; then path=$((1/0)); else path=one; fi; : \"$path\"",
        Value::Unknown(ValueIssue::Arithmetic),
      ),
      (
        "set -- first; set -x; path=$1; : \"$path\"",
        Value::Literal(b"first".to_vec()),
      ),
    ] {
      let (workspace, analysis) = analyze(source);
      assert_eq!(
        reads(&workspace, &analysis, b"path").last(),
        Some(&expected),
        "{source}"
      );
    }
    let (workspace, analysis) =
      analyze("f() { :; }; read tail; unset -v \"var_$tail\"; f");
    let site = workspace
      .document(DocumentId(1))
      .unwrap()
      .occurrences()
      .last()
      .unwrap();
    assert!(matches!(
      analysis
        .summarize(shellsmith::interface::application::engine::UseSite::Syntax(
          site.id
        ))
        .status(),
      shellsmith::interface::application::engine::ResolutionStatus::Resolved(_)
    ));
  }

  #[test]
  fn recursion_invalidates_only_the_summarized_effect_targets() {
    for source in [
      "path=kept; f() { f; }; f; : \"$path\"",
      "path=kept; f() { x=changed; f; }; f; : \"$path\"",
      "path=kept; f() { g; }; g() { f; }; f; : \"$path\"",
      "path=kept; f() { path=changed & f; }; f; : \"$path\"",
    ] {
      let (workspace, analysis) = analyze(source);
      assert_eq!(
        reads(&workspace, &analysis, b"path"),
        [Value::Literal(b"kept".to_vec())],
        "{source}"
      );
      assert!(analysis.has_recursive_calls());
    }
  }

  #[test]
  fn recursion_summaries_include_any_function_an_undecoded_definition_may_name()
  {
    use shellsmith::interface::application::engine::{
      ResolutionStatus, UseSite,
    };
    let (workspace, analysis) = analyze(
      "g() { :; }\ncat <<EOF\n$(f() { if c; then f; g; fi; h\\\nx() { :; }; }; f)\nEOF\n",
    );
    let call = workspace
      .document(DocumentId(1))
      .unwrap()
      .occurrences()
      .iter()
      .rfind(|site| {
        site.kind == OccurrenceKind::Command
          && site.name == Value::Literal(b"g".to_vec())
      })
      .unwrap();
    assert!(!matches!(
      analysis.summarize(UseSite::Syntax(call.id)).status(),
      ResolutionStatus::Resolved(_)
    ));
  }

  #[test]
  fn recursive_builtin_effects_preserve_unrelated_variables_and_namespaces() {
    use shellsmith::interface::application::engine::EffectTarget;
    for (command, targets) in [
      (
        "read changed",
        vec![EffectTarget::Variable(b"changed".to_vec())],
      ),
      (
        "unset -v changed",
        vec![EffectTarget::Variable(b"changed".to_vec())],
      ),
      (
        "unset -f other",
        vec![EffectTarget::Function(b"other".to_vec())],
      ),
      (
        "readonly changed",
        vec![
          EffectTarget::Variable(b"changed".to_vec()),
          EffectTarget::Attribute(b"changed".to_vec()),
        ],
      ),
      (
        "getopts ab option",
        vec![
          EffectTarget::Variable(b"option".to_vec()),
          EffectTarget::Variable(b"OPTARG".to_vec()),
          EffectTarget::Variable(b"OPTIND".to_vec()),
        ],
      ),
      ("cd ./sub", vec![EffectTarget::Directory]),
      (
        "set -- value",
        vec![EffectTarget::Options, EffectTarget::Positionals],
      ),
      ("shift", vec![EffectTarget::Positionals]),
      (
        "command read changed",
        vec![EffectTarget::Variable(b"changed".to_vec())],
      ),
    ] {
      let source = format!(
        "kept=stable; changed=before; other() {{ :; }}; f() {{ f; {command}; }}; f; : \"$kept\""
      );
      let (workspace, analysis) = analyze(&source);
      assert_eq!(
        reads(&workspace, &analysis, b"kept"),
        [Value::Literal(b"stable".to_vec())],
        "{command}"
      );
      let expected = targets
        .into_iter()
        .chain([EffectTarget::SpecialParameters])
        .collect::<std::collections::BTreeSet<_>>();
      let effects: Vec<_> = analysis
        .unknown_effects()
        .filter(|effect| effect.issue == ResolutionIssue::Recursion)
        .map(|effect| &effect.targets)
        .collect();
      assert_eq!(effects, [&expected], "{command}");
    }
  }

  #[test]
  fn unsupported_set_forms_keep_unrelated_variables_and_allow_later_assignments()
   {
    for command in [
      "set -euo unknown",
      "set -o hashall",
      "set -z",
      "set -",
      "flags=$(echo -eu); set \"$flags\"",
      "name=$(echo nounset); set -o \"$name\"",
    ] {
      let source =
        format!("path=./old; {command}; : \"$path\"; path=./new; : \"$path\"");
      let (workspace, result) = analyze(&source);
      assert_eq!(
        reads(&workspace, &result, b"path"),
        vec![
          Value::Literal(b"./old".to_vec()),
          Value::Literal(b"./new".to_vec()),
        ],
        "{source}"
      );
      assert!(
        result
          .gaps()
          .iter()
          .any(|gap| { gap.issue == ResolutionIssue::UnknownEffect }),
        "{source}"
      );
    }
  }

  #[test]
  fn runtime_termination_keeps_values_in_functions_branches_and_substitutions()
  {
    for (source, expected) in [
      ("path=./lib.sh; exit; : \"$path\"", b"./lib.sh".as_slice()),
      ("path=./lib.sh; exec replacement; : \"$path\"", b"./lib.sh"),
      (
        "path=./old; f() { exit; path=./new; }; f; : \"$path\"",
        b"./new",
      ),
      (
        "path=./old; if condition; then exit; path=./new; else exec replacement; path=./new; fi; : \"$path\"",
        b"./new",
      ),
    ] {
      let (workspace, result) = analyze(source);
      assert_eq!(
        reads(&workspace, &result, b"path").last(),
        Some(&Value::Literal(expected.to_vec())),
        "{source}"
      );
      assert_eq!(result.gaps(), [], "{source}");
    }
    let (workspace, result) = analyze(
      "path=./old; : \"$(exit; path=./inner; : \"$path\")\"; : \"$path\"",
    );
    assert_eq!(
      reads(&workspace, &result, b"path"),
      vec![
        Value::Literal(b"./inner".to_vec()),
        Value::Literal(b"./old".to_vec()),
      ]
    );
  }

  #[test]
  fn assignments_propagate_expanded_bytes_instead_of_reusing_later_values() {
    for (source, expected) in [
      (
        "base=./lib; path=\"$base/main.sh\"; : \"$path\"",
        b"./lib/main.sh".as_slice(),
      ),
      (
        "base='./with space'; path=${base}/lib.sh; : \"$path\"",
        b"./with space/lib.sh",
      ),
      ("base=./old; path=$base; base=./new; : \"$path\"", b"./old"),
      (
        "path=./base; path=\"$path/sub\"; : \"$path\"",
        b"./base/sub",
      ),
      ("path=; : \"$path\"", b""),
      (
        "base=./lib path=$base/main.sh; : \"$path\"",
        b"./lib/main.sh",
      ),
      (
        "path=./lib\\ dir/\"quoted\"' end'; : \"$path\"",
        b"./lib dir/quoted end",
      ),
      (
        "path=./lib*?[a-z][[:alpha:]]; : \"$path\"",
        b"./lib*?[a-z][[:alpha:]]",
      ),
      ("base=z; path=./[$base]; : \"$path\"", b"./[z]"),
      ("path=$'./lib\\x2esh'; : \"$path\"", b"./lib.sh"),
      ("path=old; unset -v path; : \"$path\"", b""),
      ("path=./old; path=./new set -eu; : \"$path\"", b"./new"),
    ] {
      let (workspace, result) = analyze(source);
      assert_eq!(
        reads(&workspace, &result, b"path").last(),
        Some(&Value::Literal(expected.to_vec())),
        "{source}"
      );
    }
  }

  #[test]
  fn declaration_values_use_argument_expansion_before_prefixes_and_assignments()
  {
    for (source, expected) in [
      (
        "base=./old; export base=./new path=$base; : \"$path\"",
        b"./old".as_slice(),
      ),
      (
        "base=./old; base=./new readonly path=$base; : \"$path\"",
        b"./old",
      ),
      ("readonly path=./lib.sh; : \"$path\"", b"./lib.sh"),
      (
        "export path=./lib.sh; export path; : \"$path\"",
        b"./lib.sh",
      ),
      ("command readonly path=./lib.sh; : \"$path\"", b"./lib.sh"),
    ] {
      let (workspace, result) = analyze(source);
      assert_eq!(
        reads(&workspace, &result, b"path"),
        vec![Value::Literal(expected.to_vec())],
        "{source}"
      );
    }
  }

  #[test]
  fn value_joins_preserve_equal_bytes_and_drop_uncertain_or_unsupported_values()
  {
    for (source, expected) in [
      (
        "if condition; then path=./lib; else path=./lib; fi; : \"$path\"",
        Some(b"./lib".as_slice()),
      ),
      (
        "if condition; then path=./a; else path=./b; fi; : \"$path\"",
        None,
      ),
      ("if condition; then path=./lib; fi; : \"$path\"", None),
      ("path=./lib; condition && path=./other; : \"$path\"", None),
      ("path=./lib; read path; : \"$path\"", None),
      ("path=./lib; eval change; : \"$path\"", Some(b"./lib")),
      ("path=./lib; path=$(printf other); : \"$path\"", None),
      ("path=./lib; path=$((1 + 2)); : \"$path\"", Some(b"3")),
      (
        "path=./lib; path=${path:-other}; : \"$path\"",
        Some(b"./lib"),
      ),
      ("path=./lib; path=~; : \"$path\"", None),
      (
        "readonly path=./lib; path=./other; : \"$path\"",
        Some(b"./lib"),
      ),
      (
        "path=./lib; read path; path=./new; : \"$path\"",
        Some(b"./new"),
      ),
      (
        "path=./lib; while condition; do path=./lib; done; : \"$path\"",
        Some(b"./lib"),
      ),
      (
        "path=./lib; while condition; do path=\"$path/sub\"; done; : \"$path\"",
        None,
      ),
    ] {
      let (workspace, result) = analyze(source);
      let issue = if source.contains("path=$(") || source.contains("path=~") {
        ValueIssue::UnsupportedExpansion
      } else {
        ValueIssue::Dynamic
      };
      let issue = if source.contains("path=~") {
        ValueIssue::Unspecified
      } else {
        issue
      };
      let actual = reads(&workspace, &result, b"path");
      let actual = actual.last().expect("path read");
      if let Some(bytes) = expected {
        assert_eq!(actual, &Value::Literal(bytes.to_vec()), "{source}");
      } else {
        let actual_issue = match actual {
          Value::Partial(value) => value.issue,
          Value::Unknown(issue) => *issue,
          Value::Literal(_) => panic!("fabricated value: {source}: {actual:?}"),
        };
        assert_eq!(actual_issue, issue, "{source}");
      }
    }
  }

  #[test]
  fn function_and_subshell_values_follow_the_existing_execution_environments() {
    for (source, expected) in [
      (
        "path=./old; f() { path=./new; }; f; : \"$path\"",
        Some(b"./new".as_slice()),
      ),
      ("path=./old; (path=./new); : \"$path\"", Some(b"./old")),
      (
        "path=./old; path=./new echo ignored; : \"$path\"",
        Some(b"./old"),
      ),
      ("path=./old; f() { :; }; path=./new f; : \"$path\"", None),
      (
        "path=./same; f() { :; }; path=./same f; : \"$path\"",
        Some(b"./same"),
      ),
    ] {
      let (workspace, result) = analyze(source);
      let actual = reads(&workspace, &result, b"path");
      if let Some(bytes) = expected {
        assert_eq!(actual, [Value::Literal(bytes.to_vec())], "{source}");
      } else {
        assert!(
          matches!(actual.as_slice(), [Value::Partial(_)]),
          "{source}: {actual:?}"
        );
      }
    }
    let (workspace, result) =
      analyze("f() { : \"$path\"; }; path=./first; f; path=./second; f");
    assert_eq!(
      reads(&workspace, &result, b"path"),
      vec![
        Value::Literal(b"./first".to_vec()),
        Value::Literal(b"./second".to_vec())
      ]
    );
    let (workspace, result) = analyze("f() { : \"$path\"; }");
    assert_eq!(reads(&workspace, &result, b"path"), Vec::<Value>::new());
  }

  #[test]
  fn missing_initial_values_are_unknown_and_explicit_bindings_override_environment()
   {
    let document = support::parse(1, 1, ": \"$path\"");
    let mut request = AnalysisRequest::new(document.snapshot());
    request.initial.other_functions =
      shellsmith::interface::application::engine::InitialBinding::Absent;
    let mut workspace = Workspace::default();
    workspace.insert(document).unwrap();
    for (environment, binding, expected) in [
      (None, None, Value::Unknown(ValueIssue::Dynamic)),
      (
        Some(b"./lib".to_vec()),
        None,
        Value::Literal(b"./lib".to_vec()),
      ),
      (
        Some(b"./lib".to_vec()),
        Some(
          shellsmith::interface::application::engine::InitialBinding::Unknown,
        ),
        Value::Unknown(ValueIssue::Dynamic),
      ),
      (
        Some(b"./lib".to_vec()),
        Some(
          shellsmith::interface::application::engine::InitialBinding::Absent,
        ),
        Value::Literal(Vec::new()),
      ),
    ] {
      request.initial.environment.clear();
      request.initial.variables.clear();
      if let Some(value) = environment {
        request.initial.environment.insert(b"path".to_vec(), value);
      }
      if let Some(binding) = binding {
        request.initial.variables.insert(b"path".to_vec(), binding);
      }
      let result = workspace
        .analyze(
          request.clone(),
          &mut support::Inputs::default(),
          &AtomicBool::new(false),
        )
        .unwrap();
      assert_eq!(reads(&workspace, &result, b"path"), vec![expected]);
    }
  }

  #[test]
  fn expanded_value_limits_stop_growth_without_discarding_later_analysis() {
    let analyze = |source| {
      let document = support::parse(1, 1, source);
      let mut request = AnalysisRequest::new(document.snapshot());
      request.initial = InitialEnvironment::empty();
      request.limits.value_bytes = 4;
      let mut workspace = Workspace::default();
      workspace.insert(document).unwrap();
      let result = workspace
        .analyze(
          request,
          &mut support::Inputs::default(),
          &AtomicBool::new(false),
        )
        .unwrap();
      (workspace, result)
    };
    let limited = |result: &Analysis| {
      result
        .gaps()
        .iter()
        .any(|gap| gap.issue == ResolutionIssue::ValueLimit)
    };
    let (workspace, result) =
      analyze("x=ab; x=$x$x; x=$x$x; : \"$x\"; x=ok; : \"$x\"");
    assert_eq!(
      reads(&workspace, &result, b"x"),
      vec![
        Value::Literal(b"ab".to_vec()),
        Value::Literal(b"ab".to_vec()),
        Value::Literal(b"abab".to_vec()),
        Value::Literal(b"abab".to_vec()),
        Value::Unknown(ValueIssue::SizeLimit),
        Value::Literal(b"ok".to_vec()),
      ]
    );
    assert!(limited(&result));
    let (_, result) = analyze("x=$'\\0'abcdefgh; : \"$x\"");
    assert!(limited(&result));
  }

  #[test]
  fn utility_effects_preserve_unrelated_variables_and_function_definitions() {
    for (command, expected) in [
      ("read -r x", Value::Unknown(ValueIssue::Dynamic)),
      ("command read x", Value::Unknown(ValueIssue::Dynamic)),
      ("getopts ab x", Value::Unknown(ValueIssue::Dynamic)),
      ("read x; x=recovered", Value::Literal(b"recovered".to_vec())),
      ("set -o \"$(external)\"", Value::Literal(b"old".to_vec())),
      ("shift \"$(external)\"", Value::Literal(b"old".to_vec())),
      ("cd \"$(external)\"", Value::Literal(b"old".to_vec())),
      ("(read x)", Value::Literal(b"old".to_vec())),
      (": \"$(read x)\"", Value::Literal(b"old".to_vec())),
      ("read x & wait", Value::Literal(b"old".to_vec())),
      ("printf hi | read x", Value::Unknown(ValueIssue::Dynamic)),
      ("unset -v x", Value::Literal(Vec::new())),
      (
        "pair=x=updated; export \"$pair\"",
        Value::Literal(b"updated".to_vec()),
      ),
      ("readonly 'x=updated'", Value::Literal(b"updated".to_vec())),
      (
        "readonly \"z$(external)\"; x=updated",
        Value::Literal(b"updated".to_vec()),
      ),
      ("export -- x=updated", Value::Literal(b"updated".to_vec())),
      ("export x=updated -p", Value::Literal(b"updated".to_vec())),
      ("export x=-p", Value::Literal(b"-p".to_vec())),
      ("time :", Value::Literal(b"old".to_vec())),
      ("x=new [[ -n a ]]", Value::Literal(b"old".to_vec())),
      ("\\[[ -n a ]]", Value::Literal(b"old".to_vec())),
      ("'label:' operand", Value::Literal(b"old".to_vec())),
    ] {
      let source = format!(
        "x=old; y=keep; f() {{ :; }}; {command}; : \"$x\"; : \"$y\"; f"
      );
      let (workspace, result) = analyze(&source);
      assert_eq!(reads(&workspace, &result, b"x"), [expected], "{source}");
      assert_eq!(
        reads(&workspace, &result, b"y"),
        [Value::Literal(b"keep".to_vec())],
        "{source}"
      );
      let function = workspace.symbols_named(b"f").next().unwrap().id;
      let calls: Vec<_> = result.references(function).collect();
      assert_eq!(calls.len(), 1, "{source}");
      assert_eq!(
        calls[0].resolution.status(),
        shellsmith::interface::application::engine::ResolutionStatus::Resolved(
          function
        ),
        "{source}"
      );
    }
  }

  #[test]
  fn unknown_utility_arguments_are_limited_to_their_state_namespaces() {
    use shellsmith::interface::application::engine::EffectTarget;
    for (command, targets, keep_variable, keep_function) in [
      (
        "read \"$target\"",
        vec![EffectTarget::Variables],
        false,
        true,
      ),
      (
        "read -p prompt value",
        vec![EffectTarget::Variables],
        false,
        true,
      ),
      (
        "unset -v -- \"$target\"",
        vec![EffectTarget::Variables],
        false,
        true,
      ),
      (
        "unset -f -- \"$target\"",
        vec![EffectTarget::Functions],
        true,
        false,
      ),
      (
        "unset -f -v x",
        vec![EffectTarget::Variables, EffectTarget::Functions],
        false,
        false,
      ),
      ("export -p x=1", vec![EffectTarget::Variables], false, true),
      ("export -n x=1", vec![EffectTarget::Variables], false, true),
      (
        "readonly -p x=1",
        vec![EffectTarget::Variables, EffectTarget::Attributes],
        false,
        true,
      ),
      (
        "set \"$target\"",
        vec![EffectTarget::Positionals, EffectTarget::Options],
        true,
        true,
      ),
      (
        "shift \"$target\"",
        vec![EffectTarget::Positionals],
        true,
        true,
      ),
      ("cd \"$target\"", vec![EffectTarget::Directory], true, true),
    ] {
      let source =
        format!("x=keep; f() {{ :; }}; read target; {command}; : \"$x\"; f");
      let (workspace, result) = analyze(&source);
      let expected = if keep_variable {
        Value::Literal(b"keep".to_vec())
      } else {
        Value::Unknown(ValueIssue::Dynamic)
      };
      assert_eq!(reads(&workspace, &result, b"x"), [expected], "{command}");
      let expected: std::collections::BTreeSet<_> =
        targets.into_iter().collect();
      assert!(
        result
          .unknown_effects()
          .any(|effect| effect.targets == expected
            && workspace
              .document(DocumentId(1))
              .unwrap()
              .text(&effect.location)
              == Some(command)),
        "{command}: {:?}",
        result.unknown_effects().collect::<Vec<_>>()
      );
      let function = workspace.symbols_named(b"f").next().unwrap().id;
      assert_eq!(
        result
          .references(function)
          .next()
          .unwrap()
          .resolution
          .is_complete(),
        keep_function,
        "{command}"
      );
    }
  }

  #[test]
  fn unknown_directory_changes_make_pwd_and_oldpwd_unknown() {
    use shellsmith::interface::application::engine::{
      EffectTarget, SymbolKind,
    };
    let (workspace, result) = analyze(
      "PWD=/a; OLDPWD=/b; target=$(external); cd \"$target\"; : \"$PWD\" \"$OLDPWD\"",
    );
    for name in [b"PWD".as_slice(), b"OLDPWD"] {
      assert!(EffectTarget::Directory.covers(SymbolKind::Variable, name));
      assert_eq!(
        reads(&workspace, &result, name),
        [Value::Unknown(ValueIssue::UnsupportedExpansion)]
      );
    }
  }

  #[test]
  fn unknown_sources_preserve_known_readonly_but_prevent_unproven_overwrites() {
    let (workspace, result) = analyze(
      "readonly kept=value; x=before; . missing; x=after; : \"$kept\"; : \"$x\"",
    );
    assert_eq!(
      reads(&workspace, &result, b"kept"),
      [Value::Literal(b"value".to_vec())]
    );
    let values = reads(&workspace, &result, b"x");
    let [Value::Unknown(issue)] = values.as_slice() else {
      panic!("{values:?}")
    };
    assert_eq!(
      issue.causes().collect::<Vec<_>>(),
      [ValueIssue::Dynamic, ValueIssue::Unspecified]
    );
  }

  #[test]
  fn uncertain_expansion_order_preserves_unrelated_reads_during_and_after_commands()
   {
    for command in [
      "x=1 temp=$kept external",
      "x=1 : >\"$kept\"",
      "x=${other:=written} external",
      ": $(((x=1) + x + kept))",
    ] {
      let source =
        format!("kept=2; x=0; {command}; : \"$kept\"; x=3; : \"$x\"");
      let (workspace, result) = analyze(&source);
      assert!(
        reads(&workspace, &result, b"kept")
          .iter()
          .all(|value| *value == Value::Literal(b"2".to_vec())),
        "{source}"
      );
      assert_eq!(
        reads(&workspace, &result, b"x").last(),
        Some(&Value::Literal(b"3".to_vec())),
        "{source}"
      );
      let definition = workspace.symbols_named(b"kept").next().unwrap().id;
      assert!(
        result
          .references(definition)
          .all(|site| site.resolution.is_complete()),
        "{source}"
      );
    }
  }

  #[test]
  fn getopts_keeps_known_write_targets_and_the_unset_optarg_alternative() {
    let (workspace, result) = analyze(
      "kept=value; getopts ab option; : \"$option\"; : \"$OPTIND\"; : \"${OPTARG+set}\"; : \"$kept\"",
    );
    for name in [b"option".as_slice(), b"OPTIND"] {
      let use_site = workspace
        .document(DocumentId(1))
        .unwrap()
        .occurrences()
        .iter()
        .find(|site| {
          site.kind == OccurrenceKind::VariableExpansion
            && site.name == Value::Literal(name.to_vec())
        })
        .unwrap();
      let resolved = result
        .summarize(shellsmith::interface::application::engine::UseSite::Syntax(
          use_site.id,
        ))
        .resolution
        .unwrap();
      assert!(
        matches!(
          resolved.status(),
          shellsmith::interface::application::engine::ResolutionStatus::Resolved(_)
        ),
        "{resolved:?}"
      );
    }
    let optarg = workspace
      .document(DocumentId(1))
      .unwrap()
      .occurrences()
      .iter()
      .find(|site| site.name == Value::Literal(b"OPTARG".to_vec()))
      .unwrap();
    let resolved = result
      .summarize(shellsmith::interface::application::engine::UseSite::Syntax(
        optarg.id,
      ))
      .resolution
      .unwrap();
    assert!(resolved.alternatives.contains(
      &shellsmith::interface::application::engine::Alternative::Unset
    ));
    assert_eq!(
      reads(&workspace, &result, b"kept"),
      [Value::Literal(b"value".to_vec())]
    );
    assert_eq!(
      workspace
        .projected_symbols(DocumentId(1))
        .filter(|(symbol, _)| matches!(
          symbol.name.as_slice(),
          b"OPTIND" | b"OPTARG"
        ))
        .count(),
      0
    );
  }
}

mod expansion {
  use crate::support;
  use shellsmith::interface::application::engine::{
    AnalysisRequest, CharacterEncoding, DocumentId, InitialEnvironment,
    InputQuery, InputResult, PartialValue, Value, ValueConstraint, ValueIssue,
    Workspace,
  };
  use std::sync::atomic::AtomicBool;

  fn last_value(source: &str, initial: InitialEnvironment) -> Value {
    last_value_with_limit(source, initial, 1024 * 1024)
  }

  #[test]
  fn definite_preparation_errors_complete_with_status_one() {
    for command in [
      "unset x; : \"${x:?missing}\"",
      "unset x; x=${x?missing}",
      "x=$((1/0))",
      "expr=08; x=$(( $expr ))",
      ": \"$((1%0))\"",
      "read n; : \"$((n/0))\"",
      "n=2; : \"$((n/=0))\"",
      "expr='1/0'; x=$(( $expr ))",
      "expr='1+'; x=$(( $expr ))",
      "readonly x=1; x=2",
      "readonly x=1; x=2 :",
      "readonly x=1; : \"$((x=2))\"",
      "readonly x=; : \"${x:=2}\"",
      "set -u; unset x; : \"$x\"",
      "set -u; unset x; : \"${#x}\"",
      "set -u; set --; : \"$1\"",
      ": < ''",
      ": > ''",
      "> ''",
      "x=kept > ''",
      "f() { return 7; }; x=$(f) >\"$(:)\"",
      "{ :; } > ''",
      "(:) > ''",
      ": > \"${x:?missing}\"",
      ": <<EOF\n$((1/0))\nEOF\n",
    ] {
      let source = format!("(exit 7); {command}\nresult=$?; : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Literal(b"1".to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn preparation_errors_respect_command_and_substitution_boundaries() {
    for (command, expected) in [
      ("x=$((1/0)); :", "0"),
      ("x=$((1/0)); : \"$x\"", "0"),
      ("x=$((1/0)); y=$x", "0"),
      ("readonly x=1; x=2; :", "0"),
      (": > ''; :", "0"),
      ("(x=$((1/0)))", "1"),
      ("x=$(x=$((1/0)))", "1"),
      (": \"$(x=$((1/0)))\"", "0"),
      ("f() { x=$((1/0)); }; f", "1"),
      ("f() { :; }; f \"$((1/0))\"", "1"),
      (": \"$((1/0))\" \"$(:)\"", "1"),
      ("x=$((0 && 1/0))", "0"),
      ("x=$((1 || 1/0))", "0"),
      ("x=$((1 ? 2 : 1/0))", "0"),
      ("x=yes; : \"${x:?$((1/0))}\"", "0"),
      ("set -u; unset x; : \"${x:-default}\"", "0"),
      ("set -u; set --; : \"$@\"", "0"),
    ] {
      let source = format!("{command}; result=$?; : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
    assert_eq!(
      last_value(
        "kept=ok; readonly x=before; x=after; result=$?/$x/$kept; : \"$result\"",
        InitialEnvironment::empty()
      ),
      Value::Literal(b"1/before/ok".to_vec())
    );
  }

  #[test]
  fn uncertain_preparation_does_not_become_definite_failure_or_success() {
    for (command, issue) in [
      ("read x; : \"$((x))\"", ValueIssue::Dynamic),
      (": > file", ValueIssue::Dynamic),
      (": >&9", ValueIssue::Dynamic),
      ("{ :; } > file", ValueIssue::Dynamic),
      (
        "if read condition; then x=yes; else unset x; fi; : \"${x:?missing}\"",
        ValueIssue::Dynamic,
      ),
      ("x=$((9223372036854775807+1))", ValueIssue::Arithmetic),
    ] {
      let source = format!("{command}; result=$?; : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Unknown(issue),
        "{source}"
      );
    }
    assert_eq!(
      last_value(
        "read x; : \"$x\"; result=$?; : \"$result\"",
        InitialEnvironment::empty()
      ),
      Value::Literal(b"0".to_vec())
    );
    let value = last_value(
      "read x; : \"$((x && 1/0))\"; result=$?; : \"$result\"",
      InitialEnvironment::empty(),
    );
    let Value::Unknown(issue) = value else {
      panic!("{value:?}");
    };
    assert_eq!(
      issue.causes().collect::<Vec<_>>(),
      [ValueIssue::Dynamic, ValueIssue::Arithmetic]
    );
    assert_eq!(
      last_value_with_limit(
        "x=abcdefghijklmnopqrstuvwxyz; : \"$x\"; result=$?; : \"$result\"",
        InitialEnvironment::empty(),
        16,
      ),
      Value::Literal(b"0".to_vec())
    );
    assert_eq!(
      last_value_with_limit(
        "expr=abcdefghijklmnopqrstuvwxyz; : \"$(( $expr ))\"; result=$?; : \"$result\"",
        InitialEnvironment::empty(),
        16,
      ),
      Value::Unknown(ValueIssue::SizeLimit)
    );
  }

  fn last_value_with_limit(
    source: &str,
    initial: InitialEnvironment,
    limit: usize,
  ) -> Value {
    let mut inputs = support::Inputs::default();
    inputs.evaluated.insert(
      InputQuery::Encoding(b"en_US.UTF-8".to_vec()),
      InputResult::Encoding(CharacterEncoding::Utf8),
    );
    last_value_with_inputs(source, initial, limit, &mut inputs)
  }

  fn last_value_with_inputs(
    source: &str,
    initial: InitialEnvironment,
    limit: usize,
    inputs: &mut support::Inputs,
  ) -> Value {
    let document = support::parse(1, 1, source);
    assert!(!document.has_syntax_errors(), "{source}");
    let mut request = AnalysisRequest::new(document.snapshot());
    request.initial = initial;
    request.limits.value_bytes = limit;
    let mut workspace = Workspace::default();
    workspace.insert(document).unwrap();
    let result = workspace
      .analyze(request, inputs, &AtomicBool::new(false))
      .unwrap();
    let site = workspace
      .document(DocumentId(1))
      .unwrap()
      .occurrences()
      .iter()
      .rev()
      .find(|site| site.name == Value::Literal(b"result".to_vec()))
      .unwrap();
    result.values(site.id).last().unwrap().1.clone()
  }

  #[test]
  fn incomplete_arithmetic_invalidates_writes_and_allows_later_recovery() {
    let source =
      "n=1; expr='n=2'; result=$(( $expr )); result=$n; : \"$result\"";
    assert_eq!(
      last_value(source, InitialEnvironment::empty()),
      Value::Literal(b"2".to_vec())
    );
    assert!(matches!(
      last_value_with_limit(source, InitialEnvironment::empty(), 7),
      Value::Unknown(_)
    ));
    assert_eq!(
      last_value_with_limit(
        "n=1; expr='n=2'; : $(( $expr )); n=3; result=$n; : \"$result\"",
        InitialEnvironment::empty(),
        7
      ),
      Value::Literal(b"3".to_vec())
    );
    assert_eq!(
      last_value_with_limit(
        "readonly n=1; expr='x=2 + 0 + 0 + 0'; : $(( $expr )); result=$n; : \"$result\"",
        InitialEnvironment::empty(),
        16
      ),
      Value::Literal(b"1".to_vec())
    );
  }

  #[test]
  fn undetermined_choices_keep_the_reasons_of_their_conditions() {
    for source in [
      "x=abcdefgh; result=${x:+set}; : \"$result\"",
      "x=abcdefgh; result=$((x ? 1 : 2)); : \"$result\"",
      "x=abcdefgh; y=$(external); result=$((y + x)); : \"$result\"",
    ] {
      let value = last_value_with_limit(source, InitialEnvironment::empty(), 4);
      assert!(
        matches!(value, Value::Unknown(issue)
          if issue.causes().any(|cause| cause == ValueIssue::SizeLimit)),
        "{source}: {value:?}"
      );
    }
  }

  #[test]
  fn derived_values_preserve_the_causes_that_prevent_their_evaluation() {
    let mut failures = Vec::new();
    for body in [
      "set -- $x; result=$1",
      "set -- $x; result=$#",
      "set -- $x; result=\"$@\"",
      "set -- $x; result=\"$*\"",
      "set -- \"$x\"; result=$1",
      "f() { result=$1; }; f $x",
      "set -- a b; shift \"$x\"; result=$1",
      "set -- $x; shift; result=$1",
      "set -- $x; shift; result=$?",
      "set \"$x\"; result=$#",
      "set \"$x\"; result=$?",
      "result=$(command -p echo \"$x\")",
      "result=$(command -p echo $x)",
      "result=$(command -p printf '%s' $x)",
      "result=$(command -p printf \"$x\" a)",
      "result=$(command -v \"$x\")",
      "result=$(command -v $x)",
      "cd \"$x\"; result=$PWD",
      "HOME=$x; cd; result=$PWD",
      "CDPATH=$x; cd relative; result=$PWD",
      "getopts a result \"$x\"",
      "getopts \"$x\" result -a",
      "set -- $x; getopts a result",
      "OPTIND=$x; getopts a result -a",
      "IFS=$x; read result <<'EOF'\na b\nEOF\n",
      "read result <<'EOF'\nabcdefghijklmnopqrstuvwxyz\nEOF\n",
      "read result <<EOF\n$x\nEOF\n",
      "read value <<EOF\n$x\nEOF\nresult=$?",
      "set -- ${x:+yes}; result=$1",
      "IFS=$x; set -- a b; result=\"$*\"",
      "getopts a: flag -a \"$x\"; result=$OPTARG",
      "kept=a; unset -- \"$x\"; result=$kept",
      "kept=a; export \"$x\"; result=$kept",
      "result=$(command -v -- \"$x\")",
      "result=$(command -v -- $x)",
      "set -- $x; for value; do result=$value; done",
    ] {
      let source = format!(
        "OPTIND=1; x=abcdefghijklmnopqrstuvwxyz; {body}\n: \"$result\""
      );
      let mut initial = InitialEnvironment::empty();
      initial.cwd = Some(b"/work".to_vec());
      let value = last_value_with_limit(&source, initial, 16);
      if value != Value::Unknown(ValueIssue::SizeLimit) {
        failures.push(format!("{body}: {value:?}"));
      }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
  }

  #[test]
  fn known_results_drop_causes_from_inputs_they_no_longer_depend_on() {
    for (body, expected) in [
      ("set -- \"$x\"; result=$#", "1"),
      ("set -- \"$x\" kept; result=$2", "kept"),
      ("set -- \"$x\" kept; shift; result=$1", "kept"),
      ("set -- $x; set -- kept; result=$1", "kept"),
      ("result=$(command -p printf ok \"$x\")", "ok"),
      ("result=$(command -p printf ok $x)", "ok"),
      ("result=$(set -- $x)", ""),
      ("getopts a: result -a \"$x\"", "a"),
      ("getopts a: flag -a \"$x\"; result=$OPTIND", "3"),
      ("getopts a flag -a \"$x\"; result=$OPTARG", ""),
      ("set -- a; shift 0; result=$1", "a"),
      ("IFS=$x; set -- a; result=$#", "1"),
      ("IFS=$x; set -- a; result=\"$*\"", "a"),
      ("IFS=$x; set --; result=\"$*\"", ""),
      ("set -- \"$x\"; result=${1+yes}", "yes"),
      ("result=${x:+}", ""),
      ("set -- \"${x:+}\"; result=$1", ""),
    ] {
      let source = format!(
        "OPTIND=1; x=abcdefghijklmnopqrstuvwxyz; {body}; : \"$result\""
      );
      assert_eq!(
        last_value_with_limit(&source, InitialEnvironment::empty(), 16),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn provider_causes_survive_state_changes_until_a_known_result_replaces_them()
  {
    for (body, expected) in [
      (
        "cd /unknown; result=$PWD",
        Value::Unknown(ValueIssue::SizeLimit),
      ),
      (
        "cd /unknown; result=$?",
        Value::Unknown(ValueIssue::SizeLimit),
      ),
      (
        "cd /unknown; result=$(command -p pwd)",
        Value::Unknown(ValueIssue::SizeLimit),
      ),
      (
        "cd /unknown; cd /known; result=$PWD",
        Value::Literal(b"/known".to_vec()),
      ),
      (
        "cd /unknown; cd /known; result=$OLDPWD",
        Value::Unknown(ValueIssue::SizeLimit),
      ),
      (
        "set -- *.sh; result=$1",
        Value::Unknown(ValueIssue::Arithmetic),
      ),
      (
        "set -- *.sh; result=$#",
        Value::Unknown(ValueIssue::Arithmetic),
      ),
    ] {
      let mut inputs = support::Inputs::default();
      for (path, result) in [
        (
          b"/unknown".as_slice(),
          InputResult::Unknown(ValueIssue::SizeLimit),
        ),
        (b"/known", InputResult::Path(b"/known".to_vec())),
      ] {
        inputs.evaluated.insert(
          InputQuery::Directory {
            cwd: Vec::new(),
            path: path.to_vec(),
            physical: false,
          },
          result,
        );
      }
      inputs.evaluated.insert(
        InputQuery::Glob {
          cwd: b"/work".to_vec(),
          pattern: b"*.sh".to_vec(),
          quoted: vec![false; 4],
          utf8: false,
          portable_collation: true,
        },
        InputResult::Unknown(ValueIssue::Arithmetic),
      );
      let mut initial = InitialEnvironment::empty();
      initial.cwd = Some(b"/work".to_vec());
      initial.values.insert(b"PWD".to_vec(), b"/work".to_vec());
      let source = format!("{body}; : \"$result\"");
      assert_eq!(
        last_value_with_inputs(&source, initial, 1024, &mut inputs),
        expected,
        "{source}"
      );
    }
  }

  #[test]
  fn source_operands_keep_the_same_cause_through_argument_and_output_paths() {
    for body in [
      ". $x",
      "set -- $x; . \"$1\"",
      "set -- \"$x\"; . \"$1\"",
      ". \"$(command -p echo \"$x\")\"",
    ] {
      let source = format!("x=abcdefghijklmnopqrstuvwxyz; {body}");
      let document = support::parse(1, 1, &source);
      let mut request = AnalysisRequest::new(document.snapshot());
      request.initial = InitialEnvironment::empty();
      request.limits.value_bytes = 16;
      let mut workspace = Workspace::default();
      workspace.insert(document).unwrap();
      let result = workspace
        .analyze(
          request,
          &mut support::Inputs::default(),
          &AtomicBool::new(false),
        )
        .unwrap();
      assert_eq!(
        result
          .source_inputs()
          .map(|(_, _, input)| input.operand.clone())
          .collect::<Vec<_>>(),
        [Value::Unknown(ValueIssue::SizeLimit)],
        "{source}"
      );
    }
  }

  #[test]
  fn derived_values_combine_independent_causes_without_adding_processing_steps()
  {
    for body in [
      "set -- $x $y; result=$1",
      "set -- $x $y; result=$#",
      "set -- $x$y; result=$1",
      "set -- \"$x\"$y; result=$1",
      "result=$(command -p echo \"$x\" \"$y\")",
      "result=$(command -p printf '%s' $x $y)",
      "result=${x#$y}",
    ] {
      let source = format!(
        "x=abcdefghijklmnopqrstuvwxyz; y=$((1/0)); {body}; : \"$result\""
      );
      let value =
        last_value_with_limit(&source, InitialEnvironment::empty(), 16);
      let Value::Unknown(issue) = value else {
        panic!("{source}: {value:?}")
      };
      assert_eq!(
        issue.causes().collect::<Vec<_>>(),
        [ValueIssue::SizeLimit, ValueIssue::Arithmetic],
        "{source}"
      );
    }
    let value = last_value_with_limit(
      "x=abcdefghijklmnopqrstuvwxyz; result=$(command -p echo -n \"$x\"); : \"$result\"",
      InitialEnvironment::empty(),
      16,
    );
    let Value::Unknown(issue) = value else {
      panic!("{value:?}")
    };
    assert_eq!(
      issue.causes().collect::<Vec<_>>(),
      [ValueIssue::SizeLimit, ValueIssue::Unspecified]
    );
    for body in [
      "set -- \"$x\" $y",
      "set -- \"$x\"$y",
      "set -- \"$x\"$y; set -- \"$@\"",
    ] {
      let source = format!(
        "x=abcdefghijklmnopqrstuvwxyz; y=$((1/0)); {body}; result=$#; : \"$result\""
      );
      assert_eq!(
        last_value_with_limit(&source, InitialEnvironment::empty(), 16),
        Value::Unknown(ValueIssue::Arithmetic),
        "{source}"
      );
    }
  }

  #[test]
  fn branch_values_preserve_correlations_until_a_write_replaces_them() {
    for (source, expected) in [
      (
        "if condition; then p=a.sh; else p=b.sh; fi; result=${p#?}; : \"$result\"",
        ".sh",
      ),
      (
        "if condition; then a=1; b=1; else a=2; b=2; fi; result=$((a-b)); : \"$result\"",
        "0",
      ),
      (
        "if outer; then if inner; then a=1; b=1; else a=2; b=2; fi; else a=3; b=3; fi; x=$a; y=$b; result=$((x-y)); : \"$result\"",
        "0",
      ),
      (
        "if c; then a=1; else a=2; fi; if d; then b=3; else b=4; fi; sum=$((a+b)); f() { result=$((sum-a-b)); }; f; : \"$result\"",
        "0",
      ),
      (
        "if c; then a=1; else a=2; fi; if d; then b=3; else b=4; fi; joined=\"$a$b\"; result=$((joined-10*a-b)); : \"$result\"",
        "0",
      ),
      (
        "if c; then a=1; else a=2; fi; if d; then b=3; else b=4; fi; joined=\"$a$b\"; tail=${joined#?}; result=$((tail-b)); : \"$result\"",
        "0",
      ),
      (
        "if c; then a=1; else a=2; fi; if d; then b=3; else b=4; fi; joined=\"$a$b\"; : \"$((result=joined-10*a-b))\"; : \"$result\"",
        "0",
      ),
      (
        "if c; then a=1; b=1; else a=2; b=2; fi; printf() { result=$(($1-$2)); }; printf \"$a\" \"$b\"; : \"$result\"",
        "0",
      ),
      (
        "if c; then a=1; b=1; else a=2; b=2; fi; : \"$((result=a-b))\"; : \"$result\"",
        "0",
      ),
      (
        "if c; then a=1; b=1; else a=2; b=2; fi; printf >\"$((result=a-b))\"; : \"$result\"",
        "0",
      ),
      (
        "if c; then p=a/lib.sh; else p=b/lib.sh; fi; result=./${p#*/}; : \"$result\"",
        "./lib.sh",
      ),
      (
        "if c; then a=1; b=1; else a=2; b=2; fi; a=temporary printf '%s' \"$b\"; result=$((a-b)); : \"$result\"",
        "0",
      ),
      (
        "if condition; then a=1; b=1; else a=2; b=2; fi; a=3; result=$((a-a)); : \"$result\"",
        "0",
      ),
    ] {
      assert_eq!(
        last_value(source, InitialEnvironment::empty()),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
    for source in [
      "if first; then a=1; else a=2; fi; if second; then b=1; else b=2; fi; result=$((a-b)); : \"$result\"",
      "if condition; then a=1; b=1; else a=2; b=2; fi; read b; result=$((a-b)); : \"$result\"",
      "if condition; then a=1; b=1; else a=2; b=2; fi; b=3; result=$((a-b)); : \"$result\"",
    ] {
      assert!(
        matches!(
          last_value(source, InitialEnvironment::empty()),
          Value::Unknown(_) | Value::Partial(_)
        ),
        "{source}"
      );
    }
  }

  #[test]
  fn finite_merges_preserve_related_values_without_correlating_independent_values()
   {
    for body in [
      ": \"${unknown:-default}\"",
      ": \"$((unknown && 1))\"",
      ": \"$((unknown || 1))\"",
      ": \"$((unknown ? 1 : 2))\"",
      "readonly locked; read locked",
      "case word in x) : ;& *) : ;; esac",
    ] {
      let related = format!(
        "if condition; then a=1; b=1; else a=2; b=2; fi; read unknown; {body}; result=$((a-b)); : \"$result\""
      );
      assert_eq!(
        last_value(&related, InitialEnvironment::empty()),
        Value::Literal(b"0".to_vec()),
        "{body}"
      );
      let independent = format!(
        "if first; then a=1; else a=2; fi; if second; then b=1; else b=2; fi; read unknown; {body}; result=$((a-b)); : \"$result\""
      );
      let Value::Partial(value) =
        last_value(&independent, InitialEnvironment::empty())
      else {
        panic!("independent values: {body}");
      };
      assert!(value.choices.iter().all(|choice| choice.exact), "{body}");
      assert_eq!(
        value
          .choices
          .iter()
          .map(|choice| choice.prefix.as_slice())
          .collect::<Vec<_>>(),
        [b"-1".as_slice(), b"0", b"1"],
        "{body}"
      );
    }
    for source in [
      "read unknown; : \"$((unknown ? (a=b=3) : (a=b=4)))\"; result=$((a-b)); : \"$result\"",
      "if condition; then a=1; b=1; else unset a; b=0; fi; read unknown; : \"${unknown:-default}\"; result=$((a-b)); : \"$result\"",
    ] {
      assert_eq!(
        last_value(source, InitialEnvironment::empty()),
        Value::Literal(b"0".to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn repeated_execution_keeps_unmodified_relations_across_nested_transfers() {
    for body in [
      "while condition; do :; done",
      "while condition; do unrelated=1; done",
      "for item in x y; do :; done",
      ": | :",
      "unrelated=1 | :",
      "f() { while outer; do for item in x y; do if skip; then continue; else if finish; then break 2; else return; fi; fi; done; done; }; f",
      "while outer; do while inner; do if skip; then continue 2; else break; fi; done; unrelated=1; done",
    ] {
      let source = format!(
        "if condition; then a=1; b=1; else a=2; b=2; fi; {body}; result=$((a-b)); : \"$result\""
      );
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Literal(b"0".to_vec()),
        "{body}"
      );
    }
    for body in [
      "while condition; do a=3; done",
      "while condition; do a=$((a+1)); done",
      "a=3 | :",
    ] {
      let source = format!(
        "if condition; then a=1; b=1; else a=2; b=2; fi; {body}; result=$((a-b)); : \"$result\""
      );
      assert!(
        matches!(
          last_value(&source, InitialEnvironment::empty()),
          Value::Unknown(_) | Value::Partial(_)
        ),
        "{body}"
      );
    }
  }

  #[test]
  fn merges_keep_function_and_positional_choices_paired_with_variables() {
    for body in [
      "read unknown; : \"${unknown:-default}\"",
      "while condition; do unrelated=1; done",
      "for item in x y; do unrelated() { :; }; done",
      ": | :",
    ] {
      let source = format!(
        "if condition; then a=1; set -- 2; f() {{ result=$(($1-a-1)); }}; else a=2; set -- 4; f() {{ result=$(($1-a-2)); }}; fi; {body}; f \"$@\"; : \"$result\""
      );
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Literal(b"0".to_vec()),
        "{body}"
      );
    }
  }

  #[test]
  fn independent_branch_values_are_only_combined_when_the_command_uses_them() {
    let mut source = String::new();
    let mut arguments = String::new();
    for index in 0..32 {
      use std::fmt::Write;
      writeln!(source, "if condition; then x{index}=1; else x{index}=2; fi")
        .unwrap();
      write!(arguments, " \"$x{index}\"").unwrap();
    }
    source.push_str("result=$((x31-x31)); : \"$result\"");
    assert_eq!(
      last_value(&source, InitialEnvironment::empty()),
      Value::Literal(b"0".to_vec())
    );
    for command in ["exit", "command return"] {
      let input = format!(
        "f() {{ {source}; {command}{arguments}; }}; f; result=$?; : \"$result\""
      );
      assert_eq!(
        last_value(&input, InitialEnvironment::empty()),
        Value::Unknown(ValueIssue::Unspecified),
        "{command}"
      );
    }
  }

  #[test]
  fn computed_values_retain_independent_branch_choices_across_assignments() {
    use std::fmt::Write;
    let mut source = String::new();
    let mut sum = String::from("0");
    let mut difference = String::from("sum");
    let mut joined = String::new();
    for index in 0..32 {
      writeln!(source, "if condition; then x{index}=1; else x{index}=2; fi")
        .unwrap();
      write!(sum, "+x{index}").unwrap();
      write!(difference, "-x{index}").unwrap();
      write!(joined, "$x{index}").unwrap();
    }
    for (body, expected) in [
      (format!("sum=$(({sum})); result=$(({difference}))"), "0"),
      (
        format!(
          "sum=$(({sum})); while condition; do unrelated=1; done; result=$(({difference}))"
        ),
        "0",
      ),
      (
        format!("joined=\"{joined}\"; copied=$joined; result=${{#copied}}"),
        "32",
      ),
    ] {
      assert_eq!(
        last_value(
          &format!("{source}{body}; : \"$result\""),
          InitialEnvironment::empty(),
        ),
        Value::Literal(expected.as_bytes().to_vec()),
        "{body}"
      );
    }
  }

  #[test]
  fn unknown_values_preserve_all_failure_causes_across_words_and_branches() {
    for source in [
      "read x; result=\"$x$((1/0))\"; : \"$result\"",
      "if condition; then read result; else result=$((1/0)); fi; : \"$result\"",
    ] {
      let Value::Unknown(issue) =
        last_value(source, InitialEnvironment::empty())
      else {
        panic!("expected an unknown value: {source}");
      };
      assert_eq!(
        issue.causes().collect::<Vec<_>>(),
        [ValueIssue::Dynamic, ValueIssue::Arithmetic],
        "{source}"
      );
    }
  }

  #[test]
  fn arithmetic_calculates_values_and_preserves_selected_side_effects() {
    for (source, expected) in [
      (
        "n=2; result=$((0 && $((n=1)))); result=$result/$n; : \"$result\"",
        "0/1",
      ),
      ("n=0; result=$(( $((n=1)) + n )); : \"$result\"", "2"),
      ("n=2; result=$((n += n)); : \"$result\"", "4"),
      ("result=$((1 + 2 * 3)); : \"$result\"", "7"),
      ("n='1+2'; result=$(( $n * 3 )); : \"$result\"", "7"),
      ("n=3; result=$(( $n + 2 )); : \"$result\"", "5"),
      (
        "n=3; case $(( m = $n + 2 )) in *) ;; esac; result=$m; : \"$result\"",
        "5",
      ),
      (
        "n=3; { :; } >\"$(( m = $n + 2 ))\"; result=$m; : \"$result\"",
        "5",
      ),
      (
        "n=3; : <<EOF\n$(( m = $n + 2 ))\nEOF\nresult=$m; : \"$result\"",
        "5",
      ),
      ("n=4; result=$(( $((n+1)) * 2 )); : \"$result\"", "10"),
      ("n=010; result=$((n + 0x10)); : \"$result\"", "24"),
      (
        "n=2; result=$((n *= 3)); result=$result/$n; : \"$result\"",
        "6/6",
      ),
      (
        "n=2; result=$((0 && (n=3))); result=$result/$n; : \"$result\"",
        "0/2",
      ),
      (
        "n=2; result=$((1 || (n=3))); result=$result/$n; : \"$result\"",
        "1/2",
      ),
      (
        "n=2; result=$((0 ? (n=3) : (n=4))); result=$result/$n; : \"$result\"",
        "4/4",
      ),
      ("result=$(( (5 << 2) | 3 )); : \"$result\"", "23"),
      ("result=$(( !0 + ~0 + -2 )); : \"$result\"", "-2"),
      ("result=$(( (8 >= 4) && (4 != 3) )); : \"$result\"", "1"),
    ] {
      assert_eq!(
        last_value(source, InitialEnvironment::empty()),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
    for expression in [
      "1 / 0",
      "9223372036854775807 + 1",
      "1 << 64",
      "-1 >> 2",
      "1 / 0 ? 1 : 1",
      "9223372036854775807 + 1 ? 1 : 2",
    ] {
      let source = format!("result=$(({expression})); : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Unknown(ValueIssue::Arithmetic),
        "{source}"
      );
    }
  }

  #[test]
  fn inherited_quote_context_preserves_backslashes_and_resets_in_commands() {
    for (source, expected) in [
      (
        r#"result="${v:-\q}"; : "$result""#,
        Value::Literal(br"\q".to_vec()),
      ),
      (
        "read -r result <<END\n${v:-\\q}\nEND\n: \"$result\"",
        Value::Literal(br"\q".to_vec()),
      ),
      (
        "read -r result <<END\n$(command -p printf %s ${v:-\\q})\nEND\n: \"$result\"",
        Value::Literal(b"q".to_vec()),
      ),
      (
        r#"q=7; result=$(( ${v:-\q} )); : "$result""#,
        Value::Unknown(ValueIssue::Arithmetic),
      ),
      (
        r#"result=$(( 0x${v:-\f} )); : "$result""#,
        Value::Unknown(ValueIssue::Arithmetic),
      ),
      (
        r#"q=7; result=$(( ${w:-${v:-\q}} )); : "$result""#,
        Value::Unknown(ValueIssue::Arithmetic),
      ),
      (
        r#"q=7; result=$(( $(command -p printf %s ${v:-\q}) )); : "$result""#,
        Value::Literal(b"7".to_vec()),
      ),
      (
        r#"q=7; result=$(( ${v:-$(command -p printf %s \q)} )); : "$result""#,
        Value::Literal(b"7".to_vec()),
      ),
    ] {
      assert_eq!(
        last_value(source, InitialEnvironment::empty()),
        expected,
        "{source}"
      );
    }
  }

  #[test]
  fn arithmetic_operators_handle_both_comparison_results_and_invalid_operands()
  {
    for (expression, expected) in [
      ("7 % 3", "1"),
      ("-7 % 3", "-1"),
      ("8 >> 2", "2"),
      ("6 & 3", "2"),
      ("6 ^ 3", "5"),
      ("+4", "4"),
      ("2 == 2", "1"),
      ("2 == 3", "0"),
      ("2 < 3", "1"),
      ("3 < 2", "0"),
      ("2 <= 2", "1"),
      ("3 <= 2", "0"),
      ("3 > 2", "1"),
      ("2 > 3", "0"),
      ("0 || 5", "1"),
      ("0 || 0", "0"),
    ] {
      let source = format!("result=$(({expression})); : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Literal(expected.as_bytes().to_vec()),
        "{expression}"
      );
    }
    for expression in ["1 % 0", "1 >> 64", "1 << -1", "9223372036854775807 * 2"]
    {
      let source = format!("result=$(({expression})); : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Unknown(ValueIssue::Arithmetic),
        "{expression}"
      );
    }
  }

  #[test]
  fn nonempty_arithmetic_variables_require_signed_integer_constants() {
    for (value, expected) in [
      ("-0x10", Some("-16")),
      ("+7", Some("7")),
      ("--5", None),
      ("+-5", None),
      ("0x-5", None),
      ("0-5", None),
      (" 5", None),
      ("5 ", None),
      (" ", None),
      ("\t", None),
      ("08", None),
      ("0x", None),
    ] {
      let source = format!("v='{value}'; result=$((v)); : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        expected.map_or(Value::Unknown(ValueIssue::Arithmetic), |expected| {
          Value::Literal(expected.as_bytes().to_vec())
        }),
        "{source}"
      );
    }
  }

  #[test]
  fn arithmetic_uses_zero_only_for_known_unset_or_empty_variables() {
    for (body, expected) in [
      ("unset x; result=$((x))", "0"),
      ("unset x; result=$((x + 1))", "1"),
      ("x=; result=$((x + 1))", "1"),
      ("unset x; result=$((x += 1)); result=$result/$x", "1/1"),
      ("x=; result=$((x += 1)); result=$result/$x", "1/1"),
      ("unset x; : $((x + 1)); result=${x-unset}", "unset"),
      ("x=; : $((x + 1)); result=${x+set}:$x", "set:"),
      ("unset x; result=$((x = 3)); result=$result/$x", "3/3"),
      ("unset x; result=$((${x:-0} + 1))", "1"),
      ("x=; result=$(($x + 1))", "1"),
      (
        "unset x y; result=$((x && (y = 1))); result=$result/${y-unset}",
        "0/unset",
      ),
      ("x=; result=$((x || (y = 1))); result=$result/$y", "1/1"),
    ] {
      let source = format!("{body}; : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
    for (body, issue) in [
      ("read x; result=$((x + 1))", ValueIssue::Dynamic),
      ("read x; result=$((x += 1)); result=$x", ValueIssue::Dynamic),
      ("x=$((1 / 0)); result=$((x + 1))", ValueIssue::Arithmetic),
      ("x=; result=$(($x))", ValueIssue::Arithmetic),
    ] {
      let source = format!("{body}; : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Unknown(issue),
        "{source}"
      );
    }
  }

  #[test]
  fn return_status_uses_numeric_operands_and_preserves_the_previous_status_when_omitted()
   {
    for (source, expected) in [
      (
        "f() { return 7; }; f; result=$?; : \"$result\"",
        Value::Literal(b"7".to_vec()),
      ),
      (
        "f() { return '007'; }; f; result=$?; : \"$result\"",
        Value::Literal(b"7".to_vec()),
      ),
      (
        "f() { ! return 7; }; f; result=$?; : \"$result\"",
        Value::Literal(b"7".to_vec()),
      ),
      (
        "f() { ! { return 7; }; }; f; result=$?; : \"$result\"",
        Value::Literal(b"7".to_vec()),
      ),
      (
        "f() { return 7; }; ! f; result=$?; : \"$result\"",
        Value::Literal(b"0".to_vec()),
      ),
      (
        "f() { ! :; return; }; f; result=$?; : \"$result\"",
        Value::Literal(b"1".to_vec()),
      ),
      (
        "f() { ! :; command return; }; f; result=$?; : \"$result\"",
        Value::Literal(b"1".to_vec()),
      ),
      (
        "n=255; f() { return \"$n\"; }; f; result=$?; : \"$result\"",
        Value::Literal(b"255".to_vec()),
      ),
      (
        "f() { return 256; }; f; result=$?; : \"$result\"",
        Value::Unknown(ValueIssue::Unspecified),
      ),
      (
        "f() { return -1; }; f; result=$?; : \"$result\"",
        Value::Unknown(ValueIssue::Unspecified),
      ),
    ] {
      assert_eq!(
        last_value(source, InitialEnvironment::empty()),
        expected,
        "{source}"
      );
    }
  }

  #[test]
  fn return_status_normalization_joins_equivalent_numeric_spellings() {
    for source in [
      "f() { if condition; then n=7; else n=007; fi; return \"$n\"; }; f; result=$?; : \"$result\"",
      "f() { if condition; then n=7; else n=007; fi; command return \"$n\"; }; f; result=$?; : \"$result\"",
      "empty=; f() { if condition; then n=7; else n=007; fi; return $empty \"$n\" $empty; }; f; result=$?; : \"$result\"",
      "f() { if condition; then n=7; else n=007; fi; return \"$@\" \"$n\"; }; f; result=$?; : \"$result\"",
    ] {
      assert_eq!(
        last_value(source, InitialEnvironment::empty()),
        Value::Literal(b"7".to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn pipeline_negation_inverts_known_status_and_preserves_effects_and_unknown_status()
   {
    for (source, expected) in [
      (
        "! :; result=$?; : \"$result\"",
        Value::Literal(b"1".to_vec()),
      ),
      (
        "! { ! :; }; result=$?; : \"$result\"",
        Value::Literal(b"0".to_vec()),
      ),
      (
        "x=before; ! { x=after; :; }; result=$?/$x; : \"$result\"",
        Value::Literal(b"1/after".to_vec()),
      ),
      (
        "! external; result=$?; : \"$result\"",
        Value::Unknown(ValueIssue::Dynamic),
      ),
    ] {
      assert_eq!(
        last_value(source, InitialEnvironment::empty()),
        expected,
        "{source}"
      );
    }
  }

  #[test]
  fn compound_commands_complete_with_zero_when_no_branch_or_body_runs() {
    let statuses = |choices: &[&[u8]]| {
      Value::Partial(PartialValue {
        choices: choices
          .iter()
          .map(|status| ValueConstraint {
            prefix: status.to_vec(),
            suffix: status.to_vec(),
            minimum: status.len(),
            required: status.iter().copied().collect(),
            exact: true,
          })
          .collect(),
        issue: ValueIssue::Dynamic,
      })
    };
    for (command, expected) in [
      ("if (exit 3); then (exit 3); fi", statuses(&[b"0", b"3"])),
      (
        "(exit 3); if (exit 3); then :; fi",
        Value::Literal(b"0".to_vec()),
      ),
      (
        "if (exit 3); then :; elif (exit 5); then (exit 4); fi",
        statuses(&[b"0", b"4"]),
      ),
      ("while (exit 3); do (exit 3); done", statuses(&[b"0", b"3"])),
      ("until (exit 0); do (exit 3); done", statuses(&[b"0", b"3"])),
      (
        "(exit 3); while (exit 0); do :; done",
        Value::Literal(b"0".to_vec()),
      ),
      (
        "(exit 4); for item in a; do (exit 3); done",
        statuses(&[b"0", b"3"]),
      ),
      (
        "(exit 3); for item in; do :; done",
        Value::Literal(b"0".to_vec()),
      ),
      (
        "(exit 3); case a in b) :;; esac",
        Value::Literal(b"0".to_vec()),
      ),
      ("(exit 3); case a in esac", Value::Literal(b"0".to_vec())),
      (
        "(exit 7); case a in a) (exit 3);; esac",
        statuses(&[b"0", b"3"]),
      ),
      (
        "(exit 7); case a in a) ;& b) :;; esac",
        Value::Literal(b"0".to_vec()),
      ),
      (
        "(exit 7); case a in a) ;& b) (exit \"$?\");; esac",
        statuses(&[b"0", b"7"]),
      ),
      (
        "(exit 7); case a in a) ;& b) ;& c) (exit 3);; esac",
        statuses(&[b"0", b"3"]),
      ),
      (
        "(exit 7); case a in a) ;; esac; :",
        Value::Literal(b"0".to_vec()),
      ),
    ] {
      let source = format!("{command}; result=$?; : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        expected,
        "{source}"
      );
    }
  }

  #[test]
  fn case_status_is_zero_when_the_last_executed_clause_has_no_body() {
    for command in [
      "case a in a) ;; esac",
      "case a in b) ;; esac",
      "case a in a) esac",
      "case a in a) # comment\n ;; esac",
      "case a in a) # comment\n esac",
      "case a in a) ;& esac",
      "case a in a) (exit 3);& b) ;; esac",
      "case a in a) (exit 3);& b) ;& c) esac",
      "case a in a) (exit 3);& b) ;& esac",
      "case a in \"$(command -p printf a; exit 4)\") ;; esac",
    ] {
      let source = format!("(exit 7); {command}; result=$?; : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Literal(b"0".to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn empty_case_clauses_preserve_variables_functions_arguments_and_io() {
    let source = r#"
      result=$(
        value=kept
        f() { command -p printf '%s' "$value/$1/$line"; }
        set -- argument
        {
          command -p printf before/
          case a in a) ;; esac
          IFS= read -r line
          f "$1"
        } <<'EOF'
input
EOF
      )
      : "$result"
    "#;
    assert_eq!(
      last_value(source, InitialEnvironment::empty()),
      Value::Literal(b"before/kept/argument/input".to_vec())
    );
  }

  #[test]
  fn parameter_lengths_and_pattern_removal_respect_quotes_and_match_extent() {
    for (expression, expected) in [
      ("${#p}", "12"),
      ("${p#*/}", "b/file.txt"),
      ("${p##*/}", "file.txt"),
      ("${p%.*}", "a/b/file"),
      ("${p%%/*}", "a"),
      ("${p#'*/'}", "a/b/file.txt"),
      ("${p#a/[ab]/}", "file.txt"),
      ("${p##[[:alpha:]]/}", "b/file.txt"),
    ] {
      let source =
        format!("p=a/b/file.txt; result=\"{expression}\"; : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
    let source =
      "LC_ALL=en_US.UTF-8; p=日本語; result=${#p}/${p#?}; : \"$result\"";
    assert_eq!(
      last_value(source, InitialEnvironment::empty()),
      Value::Literal("3/本語".as_bytes().to_vec())
    );
    for (source, expected) in [
      ("set -- a b; p='a b'; result=${p#$*}; : \"$result\"", ""),
      (
        "IFS='*'; set -- a b; p=a-b; result=${p#$*}; : \"$result\"",
        "",
      ),
      (
        "IFS='*'; set -- a b; p=a-b; result=${p#\"$*\"}; : \"$result\"",
        "a-b",
      ),
    ] {
      assert_eq!(
        last_value(source, InitialEnvironment::empty()),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn portable_character_classes_match_members_and_reject_nonmembers() {
    for (class, member, nonmember) in [
      ("alnum", "7", "!"),
      ("alpha", "A", "7"),
      ("blank", "\t", "\n"),
      ("cntrl", "\n", "A"),
      ("digit", "7", "a"),
      ("graph", "!", " "),
      ("lower", "a", "A"),
      ("print", " ", "\t"),
      ("punct", "!", "a"),
      ("space", "\n", "a"),
      ("upper", "A", "a"),
      ("xdigit", "f", "g"),
    ] {
      for (input, prefix, suffix) in [
        (member, "tail".to_owned(), "head".to_owned()),
        (
          nonmember,
          format!("{nonmember}tail"),
          format!("head{nonmember}"),
        ),
      ] {
        for (value, expression, expected) in [
          (
            format!("{input}tail"),
            format!("${{p#[[:{class}:]]}}"),
            prefix,
          ),
          (
            format!("head{input}"),
            format!("${{p%[[:{class}:]]}}"),
            suffix,
          ),
        ] {
          let source = format!(
            "LC_ALL=C; p='{value}'; result=\"{expression}\"; : \"$result\""
          );
          assert_eq!(
            last_value(&source, InitialEnvironment::empty()),
            Value::Literal(expected.into_bytes()),
            "{source:?}"
          );
        }
      }
    }
  }

  #[test]
  fn bracket_patterns_preserve_quoting_negation_and_literal_boundaries() {
    for (value, pattern, expected) in [
      ("abc", "[!b]", "bc"),
      ("bbc", "[!b]", "bbc"),
      ("]abc", "[]a]", "abc"),
      ("-abc", "[-a]", "abc"),
      ("bcd", "[a-c]", "cd"),
      ("abc", "[[.a.]]", "bc"),
      ("abc", "[[=a=]]", "bc"),
      ("abc", "'[a-c]'", "abc"),
      ("[abc", "[", "abc"),
      ("abc", "z*", "abc"),
      ("", "?", ""),
    ] {
      let source = format!(
        "LC_ALL=C; p='{value}'; result=\"${{p#{pattern}}}\"; : \"$result\""
      );
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn expanded_backslashes_escape_pattern_characters_unless_posix_leaves_them_unspecified()
   {
    for (assignments, expression, expected) in [
      (r"x='a*b' p='\*'", "${x#*$p}", Some("b")),
      (r"x='a*b' p='\*'", "${x%$p*}", Some("a")),
      (r"x='a?b' p='a\?'", "${x#$p}", Some("b")),
      (r"x='*b' p='\'", "${x#$p*}", Some("b")),
      (r"x='\b' p='\\'", "${x#$p}", Some("b")),
      (r"x='a*b' p='\*'", r#"${x#*"$p"}"#, Some("a*b")),
      (r"x=ab p='a\'", "${x#$p}", None),
      (r"x='*b' p='\'", r#"${x#$p""*}"#, None),
      (r"x='*b' p='\'", "${x#$p'*'}", None),
      (r"x='*b' p='\'", r"${x#$p\*}", None),
      (r"x='\b' p='[\]'", "${x#$p}", None),
      ("x=abc", "${x#[^a]}", None),
      ("x=bcd", "${x#[[=a=]-c]}", None),
    ] {
      let source =
        format!("LC_ALL=C; {assignments}; result={expression}; : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        expected.map_or(Value::Unknown(ValueIssue::Unspecified), |expected| {
          Value::Literal(expected.as_bytes().to_vec())
        }),
        "{source}"
      );
    }
  }

  #[test]
  fn dollar_single_quotes_decode_portable_escapes_without_expanding_text() {
    for (word, expected) in [
      (r"$'\a\b\f\n\r\t\v'", b"\x07\x08\x0c\n\r\t\x0b".as_slice()),
      (r#"$'\\\'\"'"#, b"\\'\""),
      (r"$'\101\x42'", b"AB"),
      (r"$'$HOME `cmd` $(cmd)'", b"$HOME `cmd` $(cmd)"),
    ] {
      let source = format!("result={word}; : \"$result\"");
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Literal(expected.to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn utf8_field_splitting_preserves_quoted_fields_and_empty_separators() {
    for (expansion, expected) in [("$p", "3//a/b"), ("\"$p\"", "1/éaéb//")] {
      let source = format!(
        "LC_ALL=en_US.UTF-8; IFS=é; p=éaéb; set -- {expansion}; result=$#/$1/$2/$3; : \"$result\""
      );
      assert_eq!(
        last_value(&source, InitialEnvironment::empty()),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn positional_arguments_preserve_empty_fields_and_function_scope() {
    for (source, expected) in [
      (
        "set -- one two; result=$1/$2/$#; : \"$result\"",
        "one/two/2",
      ),
      (
        "set -- one two; shift; result=$1/$#; : \"$result\"",
        "two/1",
      ),
      (
        "set -- one two; IFS=:; result=\"$*\"; : \"$result\"",
        "one:two",
      ),
      ("set --; set -- \"$@\"; result=$#; : \"$result\"", "0"),
      (
        "set -- '' 'a b'; set -- \"$@\"; result=$#/$1/$2; : \"$result\"",
        "2//a b",
      ),
      (
        "set -- a b; set -- pre\"$@\"post; result=$1/$2; : \"$result\"",
        "prea/bpost",
      ),
      (
        "set -- outer; f() { result=$1/$#; set -- inner; }; f value; result=$result/$1; : \"$result\"",
        "value/1/outer",
      ),
      ("set --; result=${1:-fallback}; : \"$result\"", "fallback"),
    ] {
      assert_eq!(
        last_value(source, InitialEnvironment::empty()),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn undefined_expansions_and_readonly_arithmetic_do_not_prove_values() {
    for source in [
      "n=1; result=$((n += (n=2))); : \"$result\"",
      "n=1; : $((n = (n=2) + 1)); result=$n; : \"$result\"",
      "readonly n=1; result=$((n=2)); : \"$result\"",
      "set --; x=; set -- \"$@$x\"; result=$#; : \"$result\"",
      "read expression; result=$(( $expression )); : \"$result\"",
      "set -- a; result=${@:+set}; : \"$result\"",
      "set -- a; result=${*+set}; : \"$result\"",
      "f() { result=$r; }; x=é; LC_ALL=en_US.UTF-8 r=${#x} f; : \"$result\"",
    ] {
      assert!(
        matches!(
          last_value(source, InitialEnvironment::empty()),
          Value::Unknown(_)
        ),
        "{source}"
      );
    }
  }

  #[test]
  fn unordered_arithmetic_writes_keep_readonly_values() {
    for source in [
      "readonly n=1; : $(( (n=2) + n )); result=$n; : \"$result\"",
      "readonly n=1; : $(( n += (n=2) )); result=$n; : \"$result\"",
    ] {
      assert_eq!(
        last_value(source, InitialEnvironment::empty()),
        Value::Literal(b"1".to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn explicit_shell_inputs_and_tilde_values_do_not_depend_on_the_host_environment()
   {
    let mut initial = InitialEnvironment::empty();
    initial.positional = Some(vec![b"first".to_vec(), b"second".to_vec()]);
    initial.special.insert(b"0".to_vec(), b"script".to_vec());
    initial.special.insert(b"$".to_vec(), b"123".to_vec());
    let mut inputs = support::Inputs::default();
    inputs.evaluated.insert(
      InputQuery::Home(b"example".to_vec()),
      InputResult::Path(b"/home/example".to_vec()),
    );
    assert_eq!(
      last_value("result=$0/$$/$1/$2/$#; : \"$result\"", initial.clone()),
      Value::Literal(b"script/123/first/second/2".to_vec())
    );
    assert_eq!(
      last_value_with_inputs(
        "result=~example/lib; : \"$result\"",
        initial,
        1024 * 1024,
        &mut inputs
      ),
      Value::Literal(b"/home/example/lib".to_vec())
    );
    assert_eq!(inputs.queries, [InputQuery::Home(b"example".to_vec())]);
    assert_eq!(
      last_value(
        "set --; set -- \"${missing-}\"; result=$#; : \"$result\"",
        InitialEnvironment::empty()
      ),
      Value::Literal(b"1".to_vec())
    );
  }
}

mod words {
  use crate::support;

  use shellsmith::interface::application::engine::{
    DocumentId, OccurrenceKind, Value,
  };

  use support::analyze;

  fn last_value(source: &str, name: &[u8]) -> Value {
    let (workspace, result) = analyze(source);
    let site = workspace
      .document(DocumentId(1))
      .unwrap()
      .occurrences()
      .iter()
      .rev()
      .find(|site| {
        site.kind == OccurrenceKind::VariableExpansion
          && site.name == Value::Literal(name.to_vec())
      })
      .unwrap();
    result
      .values(site.id)
      .last()
      .expect("evaluated read")
      .1
      .clone()
  }

  #[test]
  fn conditional_parameter_values_and_writes_share_the_same_branch() {
    for (source, expected) in [
      ("x=known; y=${x:-fallback}; : \"$y\"", "known"),
      ("x=known; : \"${x:=fallback}\"; : \"$x\"", "known"),
      ("x=; y=${x-fallback}; : \"$y\"", ""),
      ("x=; y=${x:-fallback}; : \"$y\"", "fallback"),
      ("y=${x:=fallback}; : \"$y\"", "fallback"),
      ("y=${x:=fallback}; : \"$x\"", "fallback"),
      ("x=known; y=${x:+${z:=yes}}; : \"$z\"", "yes"),
      ("x=known; z=before; y=${x:-${z:=after}}; : \"$z\"", "before"),
      ("y=${x:=first}$x; : \"$y\"", "firstfirst"),
      ("x=old; y=${x:+${x:=new}}$x; : \"$y\"", "oldold"),
      ("y=${x+fallback}; : \"$y\"", ""),
      ("x=; y=${x+fallback}; : \"$y\"", "fallback"),
      ("x=; y=${x:+fallback}; : \"$y\"", ""),
      ("x=; y=${x=fallback}; : \"$y\"", ""),
      ("y=${x=fallback}; : \"$x\"", "fallback"),
      ("x=; y=${x?error}; : \"$y\"", ""),
      ("x=known; y=${x:?${z:=unused}}; : \"$z\"", ""),
      ("export y=; : \"$y\"", ""),
    ] {
      let name = if source.ends_with("\"$x\"") {
        b"x"
      } else if source.ends_with("\"$z\"") {
        b"z"
      } else {
        b"y"
      };
      assert_eq!(
        last_value(source, name),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn expanded_command_and_builtin_arguments_use_shared_fields() {
    for source in [
      "x=before; action=unset; name=x; \"$action\" -v \"$name\"; : \"$x\"",
      "x=before; name=x; command unset -v \"$name\"; : \"$x\"",
      "x=before; names='x y'; y=other; unset -v $names; : \"$x\"",
      "x=before; names='x:y'; IFS=:; unset -v $names; : \"$x\"",
    ] {
      assert_eq!(
        last_value(source, b"x"),
        Value::Literal(Vec::new()),
        "{source}"
      );
    }
    assert_eq!(
      last_value(
        "x=before; name=x; readonly \"$name\"; unset -v x; : \"$x\"",
        b"x"
      ),
      Value::Literal(b"before".to_vec())
    );
  }

  #[test]
  fn quote_boundaries_and_ifs_are_preserved_through_nested_expansion() {
    for (source, expected) in [
      (
        "IFS=:; x='a:b'; for item in $x; do value=$item; done; : \"$value\"",
        None,
      ),
      (
        "IFS=:; x='a:b'; for item in \"$x\"; do : \"$item\"; done",
        Some("a:b"),
      ),
      ("IFS=:; x=; value=${x:-\"a:b\"}; : \"$value\"", Some("a:b")),
      ("set -f; x='*.sh'; value=$x; : \"$value\"", Some("*.sh")),
      (
        "set -- a b; IFS=:; set -- \"${x:-$*}\"; value=$#/$1; : \"$value\"",
        Some("1/a:b"),
      ),
      (
        "set -- a b; IFS=:; set -- \"pre${x:-$*}post\"; value=$#/$1; : \"$value\"",
        Some("1/prea:bpost"),
      ),
      (
        "set -- a b; IFS=:; set -- \"${x:-$@}\"; value=$#/$1/$2; : \"$value\"",
        Some("2/a/b"),
      ),
      (
        "set -- a b; IFS=:; set -- ${x:-$*}; value=$#/$1/$2; : \"$value\"",
        Some("2/a/b"),
      ),
      (
        "set -- a b; IFS='*'; p=a-b; value=\"${p#${x:-$*}}\"; : \"$value\"",
        Some(""),
      ),
    ] {
      let name = if source.contains("do : \"$item\"") {
        b"item".as_slice()
      } else {
        b"value".as_slice()
      };
      let actual = last_value(source, name);
      if let Some(expected) = expected {
        assert_eq!(
          actual,
          Value::Literal(expected.as_bytes().to_vec()),
          "{source}"
        );
      } else {
        assert!(matches!(actual, Value::Partial(_)), "{source}: {actual:?}");
      }
    }
  }

  #[test]
  fn possibly_unset_or_unknown_ifs_does_not_prove_joined_or_split_values() {
    use shellsmith::interface::application::engine::ValueIssue;
    for (source, expected) in [
      (
        "unset IFS; set -- a b; out=\"$*\"; : \"$out\"",
        Value::Literal(b"a b".to_vec()),
      ),
      (
        "IFS=; set -- a b; out=\"$*\"; : \"$out\"",
        Value::Literal(b"ab".to_vec()),
      ),
      (
        "IFS=; if condition; then unset IFS; fi; set -- a b; out=\"$*\"; : \"$out\"",
        Value::Unknown(ValueIssue::Dynamic),
      ),
      (
        "IFS=; if condition; then unset IFS; fi; x='a b'; set -- $x; out=$#; : \"$out\"",
        Value::Unknown(ValueIssue::Dynamic),
      ),
      (
        "IFS=; if condition; then unset IFS; fi; read -r out <<'END'\n  value  \nEND\n: \"$out\"",
        Value::Unknown(ValueIssue::Dynamic),
      ),
      (
        "IFS=$(unknown); x='a b'; set -- $x; out=$#; : \"$out\"",
        Value::Unknown(ValueIssue::UnsupportedExpansion),
      ),
    ] {
      assert_eq!(last_value(source, b"out"), expected, "{source}");
    }
  }

  #[test]
  fn source_fields_preserve_empty_quotes_splitting_and_pattern_boundaries() {
    use shellsmith::interface::application::engine::ValueIssue;
    for (source, expected) in [
      ("x=; . $x", Value::Unknown(ValueIssue::MissingOperand)),
      ("x=; . \"$x\"", Value::Unknown(ValueIssue::EmptyOperand)),
      ("x=; . $x./lib.sh", Value::Literal(b"./lib.sh".to_vec())),
      (
        "IFS=:; x=':./lib.sh'; . $x",
        Value::Unknown(ValueIssue::EmptyOperand),
      ),
      (
        "IFS=' :'; x='  :./lib.sh'; . $x",
        Value::Unknown(ValueIssue::EmptyOperand),
      ),
      (
        "IFS=' :'; x=' ./lib.sh :other'; . $x",
        Value::Literal(b"./lib.sh".to_vec()),
      ),
      (
        "IFS=:; x='lib:other'; . ./$x",
        Value::Literal(b"./lib".to_vec()),
      ),
      (
        "IFS=:; x='lib:other'; . \"./$x\"",
        Value::Literal(b"./lib:other".to_vec()),
      ),
      (
        "IFS=:; . ${x:-\"./lib:other\"}",
        Value::Literal(b"./lib:other".to_vec()),
      ),
      (
        "IFS=:; . ${x:-./lib:other}",
        Value::Literal(b"./lib".to_vec()),
      ),
      (
        "IFS=; x='./lib other'; . $x",
        Value::Literal(b"./lib other".to_vec()),
      ),
      (
        "IFS=; if condition; then unset IFS; fi; x='./lib other'; . $x",
        Value::Unknown(ValueIssue::Dynamic),
      ),
      ("x='*.sh'; . $x", Value::Unknown(ValueIssue::Dynamic)),
      ("set -f; x='*.sh'; . $x", Value::Literal(b"*.sh".to_vec())),
      (r"x='./\*.sh'; . $x", Value::Literal(br"./\*.sh".to_vec())),
      (r"x='./\'; . $x", Value::Literal(br"./\".to_vec())),
      (
        r#"x='./\'; . $x""*"#,
        Value::Unknown(ValueIssue::Unspecified),
      ),
      (
        "set -f; set +f; x='*.sh'; . $x",
        Value::Unknown(ValueIssue::Dynamic),
      ),
      (". ./]literal[", Value::Literal(b"./]literal[".to_vec())),
      (". ./'['abc']'", Value::Literal(b"./[abc]".to_vec())),
      (". ./[abc]", Value::Unknown(ValueIssue::Dynamic)),
      (
        "IFS=é; x='./élib'; . $x",
        Value::Unknown(ValueIssue::FieldSplitting),
      ),
      (
        "IFS=$(unknown); x=./lib; . $x",
        Value::Unknown(ValueIssue::UnsupportedExpansion),
      ),
      (". \"$1\"", Value::Unknown(ValueIssue::EmptyOperand)),
      (". \"$@\"", Value::Unknown(ValueIssue::MissingOperand)),
    ] {
      let (_, result) = analyze(source);
      let actual: Vec<_> = result
        .source_inputs()
        .map(|(_, _, input)| input.operand.clone())
        .collect();
      assert_eq!(actual, vec![expected], "{source}");
    }
    let (_, result) = analyze("if condition; then x=; fi; . ${x-\"\"}");
    let input = result.source_inputs().next().unwrap().2;
    let Value::Unknown(issue) = input.operand else {
      panic!("unresolved operand: {input:?}")
    };
    assert_eq!(
      issue.causes().collect::<Vec<_>>(),
      [ValueIssue::EmptyOperand, ValueIssue::MissingOperand]
    );
  }

  #[test]
  fn failed_parameter_assignments_do_not_prove_values_or_change_unrelated_variables()
   {
    use shellsmith::interface::application::engine::ValueIssue;
    for source in [
      "readonly x=; y=${x:=fallback}; : \"$y\"",
      "readonly x; y=${x=fallback}; : \"$y\"",
      "set --; y=${1:=fallback}; : \"$y\"",
    ] {
      assert_eq!(
        last_value(source, b"y"),
        Value::Unknown(ValueIssue::Dynamic),
        "{source}"
      );
    }
    for (source, name, expected) in [
      (
        "readonly x=; y=${x:=${z:=side}}; : \"$x\"",
        b"x".as_slice(),
        "",
      ),
      ("readonly x=; y=${x:=${z:=side}}; : \"$z\"", b"z", "side"),
      (
        "set --; kept=known; y=${1:=fallback}; : \"$kept\"",
        b"kept",
        "known",
      ),
      (
        "readonly x=known; y=${x:=fallback}; : \"$y\"",
        b"y",
        "known",
      ),
      ("set -- kept; y=${1:=fallback}; : \"$y\"", b"y", "kept"),
    ] {
      assert_eq!(
        last_value(source, name),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn parameter_errors_keep_side_effects_and_continue_static_analysis() {
    use shellsmith::interface::application::engine::ValueIssue;
    for (source, name, expected) in [
      (
        "y=${x?${z:=message}}; : \"$z\"",
        b"z".as_slice(),
        Value::Literal(b"message".to_vec()),
      ),
      (
        "x=; y=${x:?message}; : \"$y\"",
        b"y",
        Value::Unknown(ValueIssue::Dynamic),
      ),
      (
        "x=; y=${x:?message}; after=yes; : \"$after\"",
        b"after",
        Value::Literal(b"yes".to_vec()),
      ),
    ] {
      assert_eq!(last_value(source, name), expected, "{source}");
    }
  }
}

mod excluded_effects {
  use crate::support;

  use shellsmith::interface::application::engine::{
    Analysis, DocumentId, InitialBinding, InitialEnvironment, OccurrenceKind,
    ResolutionIssue, Value, Workspace,
  };

  fn analyze(source: &str) -> (Workspace, Analysis) {
    support::analyze_with(
      source,
      InitialEnvironment {
        other_variables: InitialBinding::Unknown,
        ..InitialEnvironment::empty()
      },
    )
  }

  fn last_value(
    workspace: &Workspace,
    analysis: &Analysis,
    name: &[u8],
  ) -> Value {
    let read = workspace
      .document(DocumentId(1))
      .unwrap()
      .occurrences()
      .iter()
      .rev()
      .find(|site| {
        site.kind == OccurrenceKind::VariableExpansion
          && site.name == Value::Literal(name.to_vec())
      })
      .unwrap();
    let values: Vec<_> = analysis
      .values(read.id)
      .map(|(_, value)| value.clone())
      .collect();
    assert_eq!(values.len(), 1);
    values[0].clone()
  }

  #[test]
  fn eval_and_alias_preserve_state_and_respect_function_overrides() {
    for (source, expected) in [
      (
        "result=before; eval 'result=after'; : \"$result\"",
        "before",
      ),
      (
        "result=before; code='result=after'; eval \"$code\"; : \"$result\"",
        "before",
      ),
      (
        "result=before; cmd=eval; \"$cmd\" 'result=after'; : \"$result\"",
        "before",
      ),
      (
        "result=before; command eval 'result=after'; : \"$result\"",
        "before",
      ),
      (
        "result=before; eval \"$(unknown)\"; : \"$result\"",
        "before",
      ),
      (
        "result=before; alias next='result=after'; next; unalias next; : \"$result\"",
        "before",
      ),
      (
        "alias f='result=alias'; f() { result=function; }; f; : \"$result\"",
        "function",
      ),
      (
        "alias() { result=function; }; alias 'result=after'; : \"$result\"",
        "function",
      ),
      (
        "result=before; result=prefix eval 'result=after'; : \"$result\"",
        "prefix",
      ),
      (
        "result=before; result=temporary alias next=other; : \"$result\"",
        "before",
      ),
      (
        "eval 'result=after' >\"${result:=redirect}\"; : \"$result\"",
        "redirect",
      ),
      ("result=before; eval \"$((result=3))\"; : \"$result\"", "3"),
      (
        "result=before; eval \"$(result=inner)\"; : \"$result\"",
        "before",
      ),
      (
        "code='eval \"$code\"'; eval \"$code\"; result=after; : \"$result\"",
        "after",
      ),
      (
        "set -- saved; set -f; eval 'set -- changed; set +f'; result=$1; : \"$result\"",
        "saved",
      ),
    ] {
      let (workspace, analysis) = support::analyze(source);
      assert_eq!(
        last_value(&workspace, &analysis, b"result"),
        Value::Literal(expected.as_bytes().to_vec()),
        "{source}"
      );
    }
  }

  #[test]
  fn excluded_eval_and_alias_bodies_keep_argument_expansion_effects() {
    for source in [
      "eval '${x:=body}' \"${x:=argument}\"; : \"$x\"",
      "alias 'a=if' \"${x:=argument}\"; : \"$x\"",
      "unalias -a \"${x:=argument}\"; : \"$x\"",
      "eval \"${x:=argument}\"; : \"$x\"",
      "alias \"next=${x:=argument}\"; : \"$x\"",
    ] {
      let (workspace, result) = support::analyze(source);
      assert_eq!(
        last_value(&workspace, &result, b"x"),
        Value::Literal(b"argument".to_vec()),
        "{source}"
      );
      assert!(result.unknown_effects().next().is_none(), "{source}");
      assert!(
        result
          .gaps()
          .iter()
          .any(|gap| gap.issue == ResolutionIssue::ExcludedEffect)
      );
    }
  }

  #[test]
  fn trap_forms_and_unknown_arguments_do_not_change_the_callers_bindings() {
    let cases: &[(&str, &[ResolutionIssue])] = &[
      (
        "trap 'kept=changed; hidden() { :; }; . ./hidden.sh' INT EXIT",
        &[],
      ),
      (
        "trap 'exit 1' HUP INT QUIT TERM USR1 USR2 CHLD ALRM PIPE",
        &[],
      ),
      ("trap 'if then' INT", &[]),
      ("trap \"$unknown\" \"$condition\"", &[]),
      ("trap \"$(external)\" TERM", &[]),
      ("trap 'kept=changed' KILL STOP ERR DEBUG SIGINT 123", &[]),
      (
        "trap - INT; trap '' EXIT; trap -p; trap; trap 0; trap --",
        &[],
      ),
      ("command trap 'kept=changed' INT", &[]),
      ("cmd=trap; \"$cmd\" 'kept=changed' EXIT", &[]),
      ("H='H=\"$H \"; trap \"$H\" TERM'; trap \"$H\" TERM", &[]),
      ("f() { trap 'kept=changed' INT; }; f", &[]),
      (
        "if condition; then trap 'kept=changed' INT; else trap \"$unknown\" EXIT; fi",
        &[],
      ),
      ("while condition; do trap 'kept=changed' INT; done", &[]),
      ("(trap 'kept=changed' EXIT)", &[]),
      (
        "trap 'kept=changed' INT | :",
        &[ResolutionIssue::PipelineEnvironment],
      ),
      ("trap 'kept=changed' EXIT & wait", &[]),
      ("set -eu; trap 'kept=changed' EXIT; false; exit 1", &[]),
    ];
    for &(call, expected_gaps) in cases {
      let source = format!("kept=original; {call}; : \"$kept\"");
      let (workspace, analysis) = analyze(&source);
      assert_eq!(
        analysis
          .source_inputs()
          .filter(|(_, context, _)| context
            .is_some_and(|context| !support::deferred(&analysis, context)))
          .count(),
        0,
        "{call}"
      );
      assert_eq!(
        analysis
          .unknown_effects()
          .filter(|effect| !support::deferred(&analysis, effect.context))
          .count(),
        0,
        "{call}"
      );
      assert_eq!(
        analysis
          .gaps()
          .iter()
          .filter(|gap| !support::deferred(&analysis, gap.context))
          .filter(|gap| gap.issue != ResolutionIssue::ExcludedEffect)
          .map(|gap| gap.issue)
          .collect::<Vec<_>>(),
        expected_gaps,
        "{call}"
      );
      assert_eq!(
        last_value(&workspace, &analysis, b"kept"),
        Value::Literal(b"original".to_vec()),
        "{call}"
      );
    }
  }

  #[test]
  fn trap_arguments_preserve_expansion_assignments_prefixes_and_substitution_isolation()
   {
    for (source, name, expected) in [
      (
        "x=0; trap \"$((x+=1))\" INT; : \"$x\"",
        b"x".as_slice(),
        b"1".as_slice(),
      ),
      (
        "unset action; trap \"${action:=exit 1}\" INT; : \"$action\"",
        b"action",
        b"exit 1",
      ),
      ("x=before; x=after trap ':' INT; : \"$x\"", b"x", b"after"),
      (
        "x=before; trap \"$(x=inside; : \"$x\")\" INT; : \"$x\"",
        b"x",
        b"before",
      ),
      (
        "unset output; trap ':' INT >\"${output:=log}\"; : \"$output\"",
        b"output",
        b"log",
      ),
    ] {
      let (workspace, analysis) = analyze(source);
      assert_eq!(
        last_value(&workspace, &analysis, name),
        Value::Literal(expected.to_vec()),
        "{source}"
      );
    }
  }
}
