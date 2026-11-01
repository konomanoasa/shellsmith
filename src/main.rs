use std::process::ExitCode;

fn main() -> ExitCode {
  let args: Vec<_> = std::env::args_os().skip(1).collect();
  match shellsmith::interface::cli::run(&args) {
    Ok(true) => ExitCode::SUCCESS,
    Ok(false) => ExitCode::FAILURE,
    Err(error) => {
      eprintln!("shellsmith: {error}");
      ExitCode::FAILURE
    }
  }
}
