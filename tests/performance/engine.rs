use std::collections::BTreeMap;
use std::fmt::Write;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use shellsmith::interface::application::engine::{
  AnalysisInputs, AnalysisLimits, AnalysisRequest, DependencyState, Document,
  DocumentId, Error, InitialEnvironment, InputQuery, InputResult, Revision,
  Snapshot, SourceDocument, SourceInput, Value, ValueIssue, Workspace,
};

#[derive(Default)]
struct Inputs(BTreeMap<SourceInput, Arc<Document>>);

impl AnalysisInputs for Inputs {
  fn source(&mut self, input: &SourceInput) -> Result<SourceDocument, Error> {
    Ok(self.0.get(input).map_or_else(
      || SourceDocument::Unresolved("No matching source input".into()),
      |document| SourceDocument::Resolved(document.clone()),
    ))
  }

  fn evaluate(&mut self, _: &InputQuery) -> Result<InputResult, Error> {
    Ok(InputResult::Unknown(ValueIssue::Dynamic))
  }
}

fn main() {
  let args: Vec<_> = std::env::args()
    .skip(1)
    .filter(|arg| arg != "--bench")
    .collect();
  assert!(
    args.is_empty() || args.len() == 2,
    "usage: engine [CASE SIZE]"
  );
  if args.is_empty() {
    measure_documents();
  }
  println!(
    "case\tsize\tparse_ms\tindex_ms\tanalysis_ms\tcontexts\tuses\tcandidate_edges\tgaps"
  );
  if args.len() == 2 {
    let size = args[1].parse().expect("positive fixture size");
    assert!(size > 0, "fixture size must be positive");
    measure(&args[0], size);
  } else {
    for case in [
      "commands",
      "trap",
      "independent_arguments",
      "branches",
      "loops",
      "calls",
      "uncalled",
      "uncalled_body",
      "uncalled_branches",
      "documents",
      "values",
      "arithmetic",
      "name_inheritance",
    ] {
      for size in [100, 400] {
        measure(case, size);
      }
    }
    for size in [16, 64] {
      measure("loop_chain", size);
      measure("branch_sum", size);
      measure("branch_concat", size);
      measure("branch_append", size);
      measure("branch_merges", size);
      measure("branch_loop", size);
    }
  }
}

fn measure_documents() {
  let cancelled = AtomicBool::new(false);
  let start = Instant::now();
  black_box(Document::parse(snapshot(0, 1), b"x=1\n", &cancelled).unwrap());
  println!(
    "cold_initialization_ms\t{:.3}",
    start.elapsed().as_secs_f64() * 1_000.0
  );
  println!("case\tsize\tparse_or_insert_ms\tupdate_ms");
  for lines in [1_200, 4_800] {
    let source = "x=1; f() { echo \"$x\"; }; f; . lib\n".repeat(lines);
    let mut workspace = Workspace::default();
    let start = Instant::now();
    workspace
      .update(snapshot(1, 1), source.as_bytes(), &cancelled)
      .unwrap();
    let initial = start.elapsed();
    let changed = source.replacen("x=1", "y=2", 1);
    let start = Instant::now();
    workspace
      .update(snapshot(1, 2), changed.as_bytes(), &cancelled)
      .unwrap();
    println!(
      "document_lines\t{lines}\t{:.3}\t{:.3}",
      initial.as_secs_f64() * 1_000.0,
      start.elapsed().as_secs_f64() * 1_000.0
    );
    black_box(workspace.search_symbols(b"f").count());
  }
  for count in [100, 1_000] {
    let mut workspace = Workspace::default();
    let start = Instant::now();
    for id in 1..=count {
      workspace
        .update(
          snapshot(id, 1),
          b"value=1\nf() { echo \"$value\"; }\n. next\n",
          &cancelled,
        )
        .unwrap();
    }
    for id in 1..count {
      let reference =
        workspace.document(DocumentId(id)).unwrap().references()[0].id;
      workspace
        .set_reference(
          reference,
          DependencyState::Resolved(snapshot(id + 1, 1)),
        )
        .unwrap();
    }
    let initial = start.elapsed();
    let start = Instant::now();
    let change = workspace
      .update(snapshot(count, 2), b"value=2\n", &cancelled)
      .unwrap();
    assert_eq!(u64::try_from(change.affected.len()).unwrap(), count);
    println!(
      "documents_in_chain\t{count}\t{:.3}\t{:.3}",
      initial.as_secs_f64() * 1_000.0,
      start.elapsed().as_secs_f64() * 1_000.0
    );
    black_box(workspace.search_symbols(b"value").count());
  }
}

fn snapshot(id: u64, revision: u64) -> Snapshot {
  Snapshot {
    document: DocumentId(id),
    revision: Revision(revision),
  }
}

fn branch_merges(size: usize, repeated: bool) -> String {
  let mut source = String::new();
  let mut expression = String::from("0");
  for index in 0..size {
    writeln!(source, "if condition; then a{index}=1; b{index}=1; else a{index}=2; b{index}=2; fi").unwrap();
    write!(expression, "+a{index}-b{index}").unwrap();
  }
  source.push_str("read unknown\n");
  if repeated {
    source.push_str("while condition; do\nunrelated=1\n");
  }
  source.push_str(&": \"${unknown:-default}\"\n".repeat(size));
  if repeated {
    source.push_str("done\n");
  }
  writeln!(source, "x=$(({expression}))").unwrap();
  source
}

fn uncalled_source(case: &str, size: usize) -> String {
  let mut source = String::new();
  if case != "uncalled" {
    source.push_str("idle() {\n");
  }
  for index in 0..size {
    match case {
      "uncalled" => writeln!(
        source,
        "f{index}() {{ if :; then a() {{ :; }}; else a() {{ :; }}; fi; a; }}"
      )
      .unwrap(),
      "uncalled_branches" => writeln!(
        source,
        "if unknown; then v{index}=one; else v{index}=two; fi"
      )
      .unwrap(),
      _ => writeln!(source, "v{index}=one; unknown").unwrap(),
    }
  }
  if case != "uncalled" {
    source.push_str("}\n");
  }
  source
}

fn branch_values(case: &str, size: usize) -> String {
  let mut source = String::new();
  let mut expression = String::new();
  for index in 0..size {
    writeln!(source, "if condition; then v{index}=1; else v{index}=2; fi")
      .unwrap();
    if case == "branch_sum" {
      write!(expression, "+v{index}").unwrap();
    } else {
      write!(expression, "$v{index}").unwrap();
    }
  }
  if case == "branch_sum" {
    writeln!(source, "x=$((0{expression}))").unwrap();
  } else {
    writeln!(source, "joined=\"{expression}\"; x=${{#joined}}").unwrap();
  }
  source
}

fn fixture(
  case: &str,
  size: usize,
  workspace: &mut Workspace,
  inputs: &mut Inputs,
  cancelled: &AtomicBool,
) -> String {
  let mut source = String::from("x=0\n");
  match case {
    "name_inheritance" => {
      source.push_str("value=1; read name; read \"$name\"\n");
      source.push_str(&"(: \"$value\")\n".repeat(size));
    }
    "branch_merges" | "branch_loop" => {
      source.push_str(&branch_merges(size, case == "branch_loop"));
    }
    "branch_sum" | "branch_concat" => {
      source.push_str(&branch_values(case, size));
    }
    "branch_append" => {
      source.push_str("PATH=/bin\n");
      source.push_str(
        &"if condition; then PATH=\"$PATH:/opt/bin\"; fi\n".repeat(size),
      );
      source.push_str("x=${#PATH}\n");
    }
    "independent_arguments" => {
      for index in 0..size {
        writeln!(source, "if condition; then v{index}=1; else v{index}=2; fi")
          .unwrap();
      }
      source.push(':');
      for index in 0..size {
        write!(source, " \"$v{index}\"").unwrap();
      }
      source.push('\n');
    }
    "commands" => source.push_str(&": \"$x\"\n".repeat(size)),
    "trap" => {
      source.push_str("trap 'x=2; cleanup; trap \"$action\" EXIT' INT EXIT\n");
      source.push_str(&": \"$x\"\n".repeat(size));
    }
    "loop_chain" => {
      source.push_str("v0=seed\n");
      for index in 1..=size {
        writeln!(source, "v{index}=''").unwrap();
      }
      source.push_str("while condition; do\n");
      for index in (1..=size).rev() {
        writeln!(source, "v{index}=\"$v{}\"", index - 1).unwrap();
      }
      writeln!(source, "done\n: \"$v{size}\"").unwrap();
    }
    "branches" => source.push_str(
      &"if condition; then x=1; else x=2; fi; : \"$x\"\n".repeat(size),
    ),
    "loops" => {
      source
        .push_str(&"while condition; do : \"$x\"; x=1; done\n".repeat(size));
    }
    "calls" => {
      source.push_str("f() { : \"$x\"; }\n");
      source.push_str(&"f\n".repeat(size));
    }
    "uncalled" | "uncalled_body" | "uncalled_branches" => {
      source.push_str(&uncalled_source(case, size));
    }
    "values" => {
      writeln!(source, "x='{}'", "v".repeat(64 * 1024)).unwrap();
      source.push_str(&": \"$x\"\n".repeat(size));
    }
    "arithmetic" => {
      writeln!(source, ": $(({}))", vec!["x"; size].join(" + ")).unwrap();
    }
    "documents" => {
      for id in 2..=u64::try_from(size + 1).unwrap() {
        writeln!(source, ". /lib{id}").unwrap();
        let document = Arc::new(
          Document::parse(snapshot(id, 1), b": \"$x\"; x=1\n", cancelled)
            .unwrap(),
        );
        workspace.insert_shared(document.clone()).unwrap();
        inputs.0.insert(
          SourceInput {
            operand: Value::Literal(format!("/lib{id}").into_bytes()),
            cwd: None,
            search_path: None,
            annotation: None,
          },
          document,
        );
      }
    }
    _ => panic!("unknown fixture: {case}"),
  }
  source.push_str(": \"$x\"\n");
  source
}

fn measure(case: &str, size: usize) {
  let cancelled = AtomicBool::new(false);
  let mut workspace = Workspace::default();
  let mut inputs = Inputs::default();
  let source = fixture(case, size, &mut workspace, &mut inputs, &cancelled);
  let start = Instant::now();
  workspace
    .update(snapshot(1, 1), source.as_bytes(), &cancelled)
    .unwrap();
  let parse = start.elapsed().as_secs_f64() * 1000.0;
  let start = Instant::now();
  workspace
    .index_document(DocumentId(1), AnalysisLimits::default(), &cancelled)
    .unwrap();
  let index = start.elapsed().as_secs_f64() * 1000.0;
  let mut request = AnalysisRequest::new(snapshot(1, 1));
  request.initial = InitialEnvironment::empty();
  let start = Instant::now();
  let analysis = workspace.analyze(request, &mut inputs, &cancelled).unwrap();
  let elapsed = start.elapsed().as_secs_f64() * 1000.0;
  let edges: usize = analysis
    .uses()
    .map(|site| site.resolution.candidates.len())
    .sum();
  println!(
    "{case}\t{size}\t{parse:.3}\t{index:.3}\t{elapsed:.3}\t{}\t{}\t{edges}\t{}",
    analysis.contexts().len(),
    analysis.uses().count(),
    analysis.gaps().len()
  );
  if case == "name_inheritance" {
    workspace.publish_analysis(analysis).unwrap();
    let start = Instant::now();
    let names = workspace.name_relations(&cancelled).unwrap();
    println!(
      "name_relations_ms\t{:.3}\tnames\t{}\toccurrences\t{}",
      start.elapsed().as_secs_f64() * 1_000.0,
      names.names.len(),
      names.occurrences.len()
    );
    black_box(names);
  } else {
    black_box(analysis);
  }
}
