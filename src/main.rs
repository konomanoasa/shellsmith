use shellsmith::interface::application::Error;
use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

const HELP: &str = "shellsmith lsp\nshellsmith --help | --version\n";

fn main() -> ExitCode {
  match run() {
    Ok(true) => ExitCode::SUCCESS,
    Ok(false) => ExitCode::FAILURE,
    Err(error) => {
      eprintln!("shellsmith: {error}");
      ExitCode::FAILURE
    }
  }
}

fn run() -> Result<bool, Error> {
  let args: Vec<OsString> = std::env::args_os().skip(1).collect();
  match args.as_slice() {
    [arg] if arg == "lsp" => shellsmith::interface::lsp::stdio(),
    [arg] if arg == "--help" => {
      io::stdout().lock().write_all(HELP.as_bytes())?;
      Ok(true)
    }
    [arg] if arg == "--version" => {
      writeln!(
        io::stdout().lock(),
        "shellsmith {}",
        env!("CARGO_PKG_VERSION")
      )?;
      Ok(true)
    }
    _ => Err(Error::Message(format!("Usage:\n{HELP}"))),
  }
}
