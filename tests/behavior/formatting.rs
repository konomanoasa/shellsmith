use crate::application_client::Client;
use crate::fixtures::Fixture;
use crate::support;
use shellsmith::interface::application::engine;
use shellsmith::interface::application::{Error, FormatEdit, Settings};
use std::sync::atomic::AtomicBool;

fn apply(source: &str, edits: &[FormatEdit]) -> String {
  for pair in edits.windows(2) {
    assert!(pair[0].location.bytes.end <= pair[1].location.bytes.start);
  }
  let mut output = source.to_owned();
  for edit in edits.iter().rev() {
    assert!(source.is_char_boundary(edit.location.bytes.start));
    assert!(source.is_char_boundary(edit.location.bytes.end));
    output.replace_range(edit.location.bytes.clone(), &edit.new_text);
  }
  output
}

fn assert_format(source: &str, expected: &str) -> String {
  let fixture = Fixture::new();
  let path = fixture.root.join("format.sh");
  let mut client = Client::new(Settings::new(fixture.root.clone())).unwrap();
  let cancelled = AtomicBool::new(false);
  client.open(&path, source, &cancelled).unwrap();
  let id = client.document(&path).unwrap();
  let edits = client.snapshot().format(id, &cancelled).unwrap();
  let output = apply(source, &edits);
  assert_eq!(output, expected, "input: {source:?}");
  assert!(
    !support::parse(1, 1, &output).has_syntax_errors(),
    "output: {output:?}"
  );
  client.change(&path, &output, &cancelled).unwrap();
  let id = client.document(&path).unwrap();
  let again = client.snapshot().format(id, &cancelled).unwrap();
  assert_eq!(apply(&output, &again), output, "second pass for {source:?}");
  assert!(again.is_empty(), "unchanged output must have no edits");
  output
}

#[test]
fn parameters_change_spelling_without_changing_words_or_quotes() {
  assert_format(
    "echo  \"$name\"  $x ${1} ${1}0 ${10} ${?} '${name}' $((x + $y))\n",
    "echo  \"${name}\"  ${x} $1 ${1}0 ${10} $? '${name}' $((x + $y))\n",
  );
}

#[test]
fn deep_substitutions_preserve_delimiters_and_are_idempotent() {
  let prefix = "out=\"$(".repeat(100);
  let suffix = ")\"".repeat(100);
  let source = format!("{prefix}echo x{suffix}\n");
  let fixture = Fixture::new();
  let path = fixture.root.join("deep.sh");
  let mut client = Client::new(Settings::new(fixture.root.clone())).unwrap();
  let cancelled = AtomicBool::new(false);
  client.open(&path, &source, &cancelled).unwrap();
  let id = client.document(&path).unwrap();
  let output =
    apply(&source, &client.snapshot().format(id, &cancelled).unwrap());
  assert_eq!(output.matches("$(").count(), 100);
  assert_eq!(output.replace("\\\n", "").matches(")\"").count(), 100);
  assert_format(&output, &output);
}

#[test]
fn preserves_literal_content_here_documents_and_final_newlines() {
  for source in [
    "",
    "\n\n\n",
    "# comment\n\n\n",
    "echo 'a\n  b'\n\n\n",
    "cat <<-EOF\n\t$name\n\tEOF\n",
    "cat <<'EOF'\n$name\nEOF",
    "printf 'x\r\ny'\n",
  ] {
    assert_format(source, source);
  }
}

#[test]
fn controls_functions_and_block_separation_follow_the_specification() {
  assert_format(
    "#!/bin/sh\n\n\nf ()\n{\necho \"$x\"\n}\necho end\nif check\nthen run; else stop; fi\n",
    "#!/bin/sh\n\n\nf() {\n  echo \"${x}\"\n}\necho end\nif check; then\n  run\nelse\n  stop\nfi\n",
  );
  assert_format(
    "while check\ndo\nrun\ndone\nfor i in a b; do echo $i; done\n",
    "while check; do\n  run\ndone\n\nfor i in a b; do\n  echo ${i}\ndone\n",
  );
}

#[test]
fn moves_then_with_comments_only_when_their_order_and_form_are_preserved() {
  for (input, expected) in [
    (
      "if check # condition\nthen\nrun\nfi\n",
      "if check; then # condition\n  run\nfi\n",
    ),
    (
      "if check\nthen # body\nrun\nfi\n",
      "if check; then # body\n  run\nfi\n",
    ),
    (
      "if check\n# body\nthen\nrun\nfi\n",
      "if check; then\n  # body\n  run\nfi\n",
    ),
    (
      "if check # A\nthen # B\nrun\nfi\n",
      "if check # A\nthen # B\n  run\nfi\n",
    ),
    (
      "if check <<EOF\ntext\nEOF\nthen\nrun\nfi\n",
      "if check <<EOF; then\ntext\nEOF\n  run\nfi\n",
    ),
    ("if a\nb\nthen\nrun\nfi\n", "if a\n  b; then\n  run\nfi\n"),
  ] {
    assert_format(input, expected);
  }
}

#[test]
fn case_retains_short_branch_forms_and_expands_multiple_commands() {
  assert_format(
    "case \"$x\" in\n(a|b)echo a;;\nx)\necho b\n;;\ny) one; two;;\nesac\n",
    "case \"${x}\" in\n  a|b) echo a ;;\n  x)\n    echo b\n  ;;\n  y)\n    one; two\n  ;;\nesac\n",
  );
}

#[test]
fn collapses_safe_continuations_and_preserves_unspecified_spacing() {
  assert_format(
    "cmd  a \\\n b 2>  file |\n filter && next \\\n || last\n",
    "cmd  a b 2>  file | filter && next || last\n",
  );
  assert_format("x=\"$(printf a \\\n b)\"\n", "x=\"$(printf a b)\"\n");
  assert_format(
    "f() {\n  echo   a\n\n\n  echo b\n}\n",
    "f() {\n  echo   a\n\n\n  echo b\n}\n",
  );
}

#[test]
fn command_wrapping_packs_arguments_and_counts_continuation_suffixes() {
  let argument = "a".repeat(37);
  assert_format(
    &format!("command {argument} {argument} tail\n"),
    &format!("command \\\n  {argument} {argument} \\\n  tail\n"),
  );
  for length in [75, 76, 77] {
    let argument = "x".repeat(length);
    let input = format!("cmd {argument}\n");
    let expected = if length <= 76 {
      input.clone()
    } else {
      format!("cmd \\\n  {argument}\n")
    };
    assert_format(&input, &expected);
  }
}

#[test]
fn mixed_operator_chains_break_at_every_operator_without_staircase_indent() {
  let argument = "a".repeat(30);
  assert_format(
    &format!("one {argument} | two {argument} && three || four\n"),
    &format!(
      "one {argument} \\\n  | two {argument} \\\n  && three \\\n  || four\n"
    ),
  );
}

#[test]
fn for_lists_wrap_after_in_and_count_do_on_the_last_line() {
  let word = "a".repeat(37);
  assert_format(
    &format!("for item in {word} {word} last; do echo $item; done\n"),
    &format!(
      "for item in \\\n  {word} {word} \\\n  last; do\n  echo ${{item}}\ndone\n"
    ),
  );
  assert_format(
    "for x in \\\n  a b; do :; done\n",
    "for x in a b; do\n  :\ndone\n",
  );
  assert_format("for x\ndo\n:\ndone", "for x; do\n  :\ndone");
}

#[test]
fn displayed_width_counts_unicode_sequences_and_tabs() {
  for (unit, count) in [
    ("界", 38),
    ("👍🏽", 38),
    ("👨‍👩‍👧‍👦", 38),
    ("e\u{301}", 76),
    ("·", 76),
    ("لا", 76),
    ("क\u{93e}", 38),
  ] {
    let argument = unit.repeat(count);
    assert_format(&format!("cmd {argument}\n"), &format!("cmd {argument}\n"));
    let argument = format!("{argument}x");
    assert_format(
      &format!("cmd {argument}\n"),
      &format!("cmd \\\n  {argument}\n"),
    );
  }
  let argument = "a".repeat(72);
  assert_format(&format!("cmd\t{argument}\n"), &format!("cmd\t{argument}\n"));
  assert_format(
    &format!("cmd\t{argument}x\n"),
    &format!("cmd\t\\\n  {argument}x\n"),
  );
}

#[test]
fn chain_comments_precede_the_chain_and_preserve_comment_order() {
  assert_format(
    "a | # inline\n# stage\nb &&\n# fallback\nc\n",
    "# inline\n# stage\n# fallback\na | b && c\n",
  );
  let argument = "a".repeat(40);
  assert_format(
    &format!("one {argument} | # stage\ntwo {argument}\n"),
    &format!("# stage\none {argument} \\\n  | two {argument}\n"),
  );
  assert_format("a \\\n  | b # comment\n", "a | b # comment\n");
  assert_format(
    "echo a\n# function\nf() {\n:\n}\n# next\necho b\n",
    "echo a\n# function\nf() {\n  :\n}\n# next\necho b\n",
  );
}

#[test]
fn preserves_negation_and_background_control_separators() {
  let argument = "a".repeat(38);
  assert_format(
    &format!("! one {argument} && ! two {argument}\n"),
    &format!("! one {argument} \\\n  && ! two {argument}\n"),
  );
  assert_format("if work &\nthen\n:\nfi\n", "if work & then\n  :\nfi\n");
}

#[test]
fn suffixes_nested_substitutions_and_long_atoms_participate_in_wrapping() {
  let argument = "a".repeat(70);
  assert_format(
    &format!("if cmd {argument}; then :; fi\n"),
    &format!("if cmd \\\n  {argument}; then\n  :\nfi\n"),
  );
  let argument = "a".repeat(72);
  assert_format(
    &format!("out=\"$(cmd {argument})\"\n"),
    &format!("out=\"$(\n  cmd {argument}\n)\"\n"),
  );
  let argument = "a".repeat(90);
  assert_format(
    &format!("cmd one {argument} tail\n"),
    &format!(
      "cmd \\\n  one \\\n  {}\\\n{} tail\n",
      "a".repeat(77),
      "a".repeat(13)
    ),
  );
}

#[test]
fn layout_preserves_trailing_spaces_and_existing_single_blank_lines() {
  assert_format("echo a  \necho b\n", "echo a  \necho b\n");
  assert_format(
    "if :; then\n\njob &\nnext\n\nelse\n\n:\n\nfi\n",
    "if :; then\n\n  job &\n  next\n\nelse\n\n  :\n\nfi\n",
  );
  assert_format("cmd  a  \\\n    b\n", "cmd  a  b\n");
}

#[test]
fn nested_blocks_here_documents_and_empty_case_actions_keep_their_structure() {
  for (input, expected) in [
    (
      "if :; then\ncat <<-EOF\n\t$x\n\tEOF\nfi\n",
      "if :; then\n  cat <<-EOF\n\t$x\n\tEOF\nfi\n",
    ),
    (
      "if cat <<A\none\nA\nnext <<B\ntwo\nB\nthen\n:\nfi",
      "if cat <<A\none\nA\n  next <<B; then\ntwo\nB\n  :\nfi",
    ),
    (
      "case x in a);; b) : | :;; esac\n",
      "case x in\n  a) ;;\n  b)\n    : | :\n  ;;\nesac\n",
    ),
    (
      "case x in (x)\n# empty\nesac\n",
      "case x in\n  x)\n    # empty\nesac\n",
    ),
    ("case x in\n    esac\n", "case x in\nesac\n"),
    (
      "case x in\na) : ;;\n# end\nesac\n",
      "case x in\n  a) : ;;\n  # end\nesac\n",
    ),
    (
      "case x in\na)\n:\n# end\nesac\n",
      "case x in\n  a)\n    :\n    # end\nesac\n",
    ),
    ("case x in\n# empty\nesac\n", "case x in\n  # empty\nesac\n"),
    ("f() (\nx=1\n)\n", "f() (\n  x=1\n)\n"),
    (
      "if :; then\nif :; then :; fi\necho x\nfi\n",
      "if :; then\n  if :; then\n    :\n  fi\n  echo x\nfi\n",
    ),
    ("out=\"$(\ncmd a\n)\"\n", "out=\"$(cmd a)\"\n"),
    (
      "if :; then\n:\n# end\nelse\n:\n# end\nfi\n",
      "if :; then\n  :\n  # end\nelse\n  :\n  # end\nfi\n",
    ),
    ("f() {\n:\n# end\n}\n", "f() {\n  :\n  # end\n}\n"),
    (
      "if :; then\nf() { :; }\nfi\n",
      "if :; then\n  f() { :; }\nfi\n",
    ),
    (
      "case x in\na)\n# empty\n;;\nesac\n",
      "case x in\n  a)\n    # empty\n  ;;\nesac\n",
    ),
    ("echo x\n   # end\n\n\n", "echo x\n# end\n\n\n"),
  ] {
    assert_format(input, expected);
  }
}

#[test]
fn syntax_errors_are_refused_and_cancellation_is_not_success() {
  let fixture = Fixture::new();
  let path = fixture.root.join("format.sh");
  let mut client = Client::new(Settings::new(fixture.root.clone())).unwrap();
  let active = AtomicBool::new(false);
  for source in ["if then", "echo ${", "if true; then :\n"] {
    client.open(&path, source, &active).unwrap();
    let id = client.document(&path).unwrap();
    assert!(matches!(
      client.snapshot().format(id, &active),
      Err(Error::FormattingRefused(_))
    ));
  }
  client.change(&path, "echo $x\n", &active).unwrap();
  let id = client.document(&path).unwrap();
  assert!(matches!(
    client.snapshot().format(id, &AtomicBool::new(true)),
    Err(Error::Engine(engine::Error::Cancelled))
  ));
  assert_eq!(
    apply("echo $x\n", &client.snapshot().format(id, &active).unwrap()),
    "echo ${x}\n"
  );
}

#[test]
fn moving_then_does_not_replace_the_unchanged_here_document() {
  let fixture = Fixture::new();
  let path = fixture.root.join("format.sh");
  let source = "if cat <<EOF\nkeep $literal\nEOF\nthen\n:\nfi\n";
  let mut client = Client::new(Settings::new(fixture.root.clone())).unwrap();
  let flag = AtomicBool::new(false);
  client.open(&path, source, &flag).unwrap();
  let id = client.document(&path).unwrap();
  let edits = client.snapshot().format(id, &flag).unwrap();
  let start = source.find("keep").unwrap();
  let end = source.find("\nthen").unwrap();
  assert!(
    edits.iter().all(|edit| edit.location.bytes.end <= start
      || edit.location.bytes.start >= end)
  );
  assert_eq!(
    apply(source, &edits),
    "if cat <<EOF; then\nkeep $literal\nEOF\n  :\nfi\n"
  );
}

#[test]
fn formatting_preserves_expansions_control_flow_and_here_document_values() {
  let original = "set -- one two\nvalue='a b'\nprintf '<%s>\\n' \"$value\" \"${1}\" \"${2}0\"\nif cat <<EOF \\\n; # condition\n$value\nEOF\nthen # body\nprintf 'then\\n'; fi\nfor x in one two; do printf '%s\\n' \"$x\"; done\ncase \"$1\" in (one) printf x; printf y;; esac\n";
  let expected = "set -- one two\nvalue='a b'\nprintf '<%s>\\n' \"${value}\" \"$1\" \"${2}0\"\nif cat <<EOF # condition\n$value\nEOF\nthen # body\n  printf 'then\\n'\nfi\n\nfor x in one two; do\n  printf '%s\\n' \"${x}\"\ndone\n\ncase \"$1\" in\n  one)\n    printf x; printf y\n  ;;\nesac\n";
  let formatted = assert_format(original, expected);
  let fixture = Fixture::new();
  for source in [original, &formatted] {
    let path = fixture.write("run.sh", source);
    let output = std::process::Command::new("/bin/sh")
      .arg(path)
      .current_dir(&fixture.root)
      .env_clear()
      .env("PATH", "/usr/bin:/bin")
      .env("LC_ALL", "C")
      .output()
      .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
      output.stdout,
      b"<a b>\n<one>\n<two0>\na b\nthen\none\ntwo\nxy"
    );
    assert_eq!(output.stderr, b"");
  }
}

#[test]
fn control_separators_survive_line_continuations() {
  for (input, expected) in [
    ("if : \\\n& then :; fi\n", "if : & then\n  :\nfi\n"),
    ("if : \\\n; then : \\\n; fi\n", "if :; then\n  :\nfi\n"),
    (
      "while : \\\n; do break \\\n; done\n",
      "while :; do\n  break\ndone\n",
    ),
    (
      "for x in a b \\\n; # first\ndo # second\nprintf x \\\n;\ndone\n",
      "for x in a b # first\ndo # second\n  printf x\ndone\n",
    ),
    (
      "for x in a b \\\n;\n# before\ndo # second\nprintf x;\ndone\n",
      "for x in a b\n# before\ndo # second\n  printf x\ndone\n",
    ),
  ] {
    assert_format(input, expected);
  }
}

#[test]
fn here_document_header_comments_keep_their_order_and_placement() {
  for (input, expected) in [
    (
      "if cat <<EOF # condition\nx\nEOF\nthen # body\n:; fi\n",
      "if cat <<EOF # condition\nx\nEOF\nthen # body\n  :\nfi\n",
    ),
    (
      "if cat <<EOF # condition\nx\nEOF\nthen\n:; fi\n",
      "if cat <<EOF; then # condition\nx\nEOF\n  :\nfi\n",
    ),
    (
      "if cat <<EOF \\\n;\nx\nEOF\nthen # body\n:; fi\n",
      "if cat <<EOF; then # body\nx\nEOF\n  :\nfi\n",
    ),
  ] {
    assert_format(input, expected);
  }
}

#[test]
fn special_parameter_braces_account_for_following_line_continuations() {
  let source = "echo ${1}\\\n0 ${?}\\\nname\n";
  assert_format(source, "echo ${1}0 ${?}name\n");
}

#[test]
fn arithmetic_expansions_preserve_parameter_syntax_in_their_own_context() {
  assert_format(
    "echo $(( $x * 3 + ${y} + ${1} ))\n",
    "echo $(( $x * 3 + ${y} + ${1} ))\n",
  );
  assert_format(
    "echo $(( $(echo $x) + $y ))\n",
    "echo $(( $(echo ${x}) + $y ))\n",
  );
}

#[test]
fn moved_chain_comments_work_in_nested_syntax() {
  for (source, expected) in [
    (
      "if a | # c\nb; then :; fi\n",
      "if \n  # c\n  a | b; then\n  :\nfi\n",
    ),
    ("f() { a | # c\nb; }\n", "f() { \n  # c\n  a | b; }\n"),
    ("out=$(a | # c\nb)\n", "out=$(\n  # c\n  a | b)\n"),
    ("a; b | # c\nd\n", "a; \n# c\nb | d\n"),
    (
      "# shellsmith source=./lib.sh\n. $path | # c\nb\n",
      "# shellsmith source=./lib.sh\n. ${path} | # c\n  b\n",
    ),
    (
      "a |\n# shellsmith source=./lib.sh\n. $path\n",
      "a |\n  # shellsmith source=./lib.sh\n  . ${path}\n",
    ),
    (
      "{\n# inner\na\n} | # outer\nb\n",
      "{\n  # inner\n  a\n} | # outer\n  b\n",
    ),
  ] {
    assert_format(source, expected);
  }
}

#[test]
fn moving_pipeline_comments_preserves_execution_and_arithmetic_expansion() {
  let original = "x='1+2'\ny=3\nprintf '%s %s\\n' \"$(( $x * 3 ))\" \"$(( y * 3 ))\" | # values\ncat\n";
  let expected = "x='1+2'\ny=3\n# values\nprintf '%s %s\\n' \"$(( $x * 3 ))\" \"$(( y * 3 ))\" | cat\n";
  let formatted = assert_format(original, expected);
  let fixture = Fixture::new();
  for source in [original, &formatted] {
    let path = fixture.write("run.sh", source);
    let output = std::process::Command::new("/bin/sh")
      .arg(path)
      .current_dir(&fixture.root)
      .env_clear()
      .env("PATH", "/usr/bin:/bin")
      .env("LC_ALL", "C")
      .output()
      .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"7 9\n");
    assert_eq!(output.stderr, b"");
  }
}

#[test]
fn blank_line_normalization_is_limited_to_adjacent_blocks() {
  for (source, expected) in [
    ("f() { :; }\ng() { :; }\n", "f() { :; }\n\ng() { :; }\n"),
    (
      "f() { :; }\n\n\n\ng() { :; }\n",
      "f() { :; }\n\ng() { :; }\n",
    ),
    (
      "f() { :; }\n\n\n# next\ng() { :; }\n",
      "f() { :; }\n\n# next\ng() { :; }\n",
    ),
    ("echo a\n\n\necho b\n", "echo a\n\n\necho b\n"),
    ("f() { :; }\n\n\necho b\n", "f() { :; }\n\n\necho b\n"),
    ("echo a\n\n\nf() { :; }\n", "echo a\n\n\nf() { :; }\n"),
    ("f() {\n\n\n  :\n\n\n}\n", "f() {\n\n\n  :\n\n\n}\n"),
  ] {
    assert_format(source, expected);
  }
}

#[test]
fn lexical_continuations_are_removed_without_changing_escapes_or_contents() {
  for (source, expected) in [
    ("echo foo\\\nbar\n", "echo foobar\n"),
    ("ec\\\nho \"ab\\\ncd\"\n", "echo \"abcd\"\n"),
    (
      "na\\\nme=value; echo $na\\\nme\n",
      "name=value; echo ${name}\n",
    ),
    (
      "echo a\\ b \\$name 'a\\\nb'\n",
      "echo a\\ b \\$name 'a\\\nb'\n",
    ),
    ("x=$(\n(echo ok)\n)\n", "x=$( (echo ok))\n"),
    (
      "cat <<EOF\nfoo\\\nbar\nEOF\n",
      "cat <<EOF\nfoo\\\nbar\nEOF\n",
    ),
  ] {
    assert_format(source, expected);
  }
}

#[test]
fn natural_wrapping_is_tried_before_continuations_and_failed_trials_are_discarded()
 {
  let word = "a".repeat(60);
  assert_format(
    &format!("printf '%s' \"$(echo {word})\"\n"),
    &format!("printf '%s' \"$(\n  echo {word}\n)\"\n"),
  );
  let word = "a".repeat(72);
  assert_format(
    &format!("cmd $(echo ok) {word} tailtail\n"),
    &format!("cmd \\\n  $(echo ok) \\\n  {word} \\\n  tailtail\n"),
  );
  let word = "a".repeat(76);
  assert_format(
    &format!("cmd \"$(echo {word})\"\n"),
    &format!(
      "cmd \\\n  \"$(\n    echo \\\n      {}\\\n{}\n  )\"\n",
      "a".repeat(73),
      "a".repeat(3)
    ),
  );
}

#[test]
fn long_words_wrap_without_indentation_or_changes_to_their_values() {
  let word = "a".repeat(170);
  let formatted = assert_format(
    &format!("printf '%s' {word}\n"),
    &format!(
      "printf \\\n  '%s' \\\n  {}\\\n{}\\\n{}\n",
      "a".repeat(77),
      "a".repeat(79),
      "a".repeat(14)
    ),
  );
  let fixture = Fixture::new();
  let path = fixture.write("word.sh", &formatted);
  let output = std::process::Command::new("/bin/sh")
    .arg(path)
    .current_dir(&fixture.root)
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("LC_ALL", "C")
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(0));
  assert_eq!(output.stdout, word.as_bytes());
  assert_eq!(output.stderr, b"");
  assert_format(
    &format!("echo '{word}'\n"),
    &format!("echo \\\n  '{word}'\n"),
  );
}

#[test]
fn quoted_segments_can_break_between_quotes_without_changing_their_contents() {
  let part = "a".repeat(40);
  assert_format(
    &format!("printf '%s' '{part}''{part}'\n"),
    &format!("printf \\\n  '%s' \\\n  '{part}'\\\n'{part}'\n"),
  );
  let word = "a".repeat(100);
  assert_format(
    &format!("printf '%s' \"{word}\"\n"),
    &format!(
      "printf \\\n  '%s' \\\n  \"{}\\\n{}\"\n",
      "a".repeat(76),
      "a".repeat(24)
    ),
  );
}

#[test]
fn substitution_flattening_keeps_comment_and_here_document_terminators() {
  for (source, expected) in [
    (
      "out=$(echo ok # comment\n)\n",
      "out=$(echo ok # comment\n)\n",
    ),
    (
      "out=$(# comment\necho ok\n)\n",
      "out=$(# comment\n  echo ok\n)\n",
    ),
    (
      "out=$(cat <<EOF\nvalue\nEOF\n)\n",
      "out=$(cat <<EOF\nvalue\nEOF\n)\n",
    ),
  ] {
    assert_format(source, expected);
  }
}

#[test]
fn substitution_edge_spaces_survive_wrapping_without_changing_the_next_pass() {
  let word = "a".repeat(65);
  assert_format(
    &format!("out=$(    echo {word}    )\n"),
    &format!("out=$(    \n  echo {word}    \n)\n"),
  );
}
