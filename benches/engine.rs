use std::fmt::Write;
use std::hint::black_box;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use shellsmith::interface::application::engine::{
  AnalysisRequest, DependencyState, Document, DocumentId, InitialEnvironment,
  Revision, Snapshot, Workspace,
};

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
  println!("case\tsize\tanalysis_ms\tcontexts\tuses\tcandidate_edges\tgaps");
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
      "documents",
      "values",
    ] {
      for size in [100, 400] {
        measure(case, size);
      }
    }
    for size in [16, 64] {
      measure("loop_chain", size);
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

fn fixture(
  case: &str,
  size: usize,
  workspace: &mut Workspace,
  cancelled: &AtomicBool,
) -> String {
  let mut source = String::from("x=0\n");
  match case {
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
    "values" => {
      writeln!(source, "x='{}'", "v".repeat(64 * 1024)).unwrap();
      source.push_str(&": \"$x\"\n".repeat(size));
    }
    "documents" => {
      for id in 2..=u64::try_from(size + 1).unwrap() {
        writeln!(source, ". lib{id}").unwrap();
        workspace
          .update(snapshot(id, 1), b": \"$x\"; x=1\n", cancelled)
          .unwrap();
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
  let source = fixture(case, size, &mut workspace, &cancelled);
  workspace
    .update(snapshot(1, 1), source.as_bytes(), &cancelled)
    .unwrap();
  let references: Vec<_> = workspace
    .document(DocumentId(1))
    .unwrap()
    .references()
    .iter()
    .map(|site| site.id)
    .collect();
  for (index, reference) in references.into_iter().enumerate() {
    workspace
      .set_reference(
        reference,
        DependencyState::Resolved(snapshot(
          u64::try_from(index + 2).unwrap(),
          1,
        )),
      )
      .unwrap();
  }
  let mut request = AnalysisRequest::new(snapshot(1, 1));
  request.initial = InitialEnvironment::empty();
  workspace.set_analysis_request(request).unwrap();
  let start = Instant::now();
  let analysis = workspace.analyze(&cancelled).unwrap();
  let elapsed = start.elapsed().as_secs_f64() * 1000.0;
  let edges: usize = analysis
    .uses()
    .map(|site| site.resolution.candidates.len())
    .sum();
  println!(
    "{case}\t{size}\t{elapsed:.3}\t{}\t{}\t{edges}\t{}",
    analysis.contexts().len(),
    analysis.uses().count(),
    analysis.gaps().len()
  );
  black_box(analysis);
}
