use super::application::{RequestScope, Session, Settings};
use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const HELP: &str =
  "shellsmith lsp\nshellsmith format PATH\nshellsmith --help | --version\n";

pub fn run(args: &[OsString]) -> Result<bool, Box<dyn Error>> {
  match args {
    [arg] if arg == "lsp" => Ok(super::lsp::stdio()?),
    [command, path]
      if command == "format"
        && !path.is_empty()
        && !path.as_encoded_bytes().starts_with(b"-") =>
    {
      format(Path::new(path))
    }
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
    _ => Err(format!("Usage:\n{HELP}").into()),
  }
}

fn format(path: &Path) -> Result<bool, Box<dyn Error>> {
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()?;
  let session = Session::new(Settings::new(std::env::current_dir()?))?;
  let mut errors = io::stderr().lock();
  let mut success = true;
  let paths = if path.is_dir() {
    session.add_input(path)?;
    let outcome =
      runtime.block_on(session.request(RequestScope::Workspace, |app, _| {
        Ok(
          app
            .workspace()
            .documents()
            .map(|document| {
              app
                .path(document.snapshot().document)
                .expect("workspace document path")
                .to_path_buf()
            })
            .collect::<BTreeSet<_>>(),
        )
      }));
    for failure in outcome.failures {
      writeln!(
        errors,
        "shellsmith: {}: {}",
        failure.path.display(),
        failure.message
      )?;
      success = false;
    }
    outcome.result?
  } else {
    BTreeSet::from([session.absolute(path)?])
  };
  for path in paths {
    let requested = path.clone();
    let outcome = runtime.block_on(session.request(
      RequestScope::Document(path.clone()),
      move |app, flag| {
        let id = app.loaded_document(&requested)?;
        let edits = app.format(id, flag)?;
        if edits.is_empty() {
          return Ok(None);
        }
        let original = app
          .workspace()
          .document(id)
          .expect("formatted document")
          .source()
          .to_owned();
        let mut formatted = original.clone();
        for edit in edits.into_iter().rev() {
          formatted.replace_range(edit.location.bytes, &edit.new_text);
        }
        Ok(Some(Replacement {
          original,
          formatted,
        }))
      },
    ));
    let failed_loading = !outcome.failures.is_empty();
    for failure in outcome.failures {
      writeln!(
        errors,
        "shellsmith: {}: {}",
        failure.path.display(),
        failure.message
      )?;
      success = false;
    }
    let result = match outcome.result {
      Ok(Some(replacement)) => replacement.write(&path),
      Ok(None) => Ok(()),
      Err(_) if failed_loading => continue,
      Err(error) => Err(error.into()),
    };
    if let Err(error) = result {
      writeln!(errors, "shellsmith: {}: {error}", path.display())?;
      success = false;
    }
  }
  Ok(success)
}

struct Replacement {
  original: String,
  formatted: String,
}

impl Replacement {
  fn write(self, path: &Path) -> Result<(), Box<dyn Error>> {
    if !std::fs::symlink_metadata(path)?.is_file() {
      return Err("File changed during formatting".into());
    }
    // Renaming alone would bypass the original file's write permissions.
    let mut original = OpenOptions::new().read(true).write(true).open(path)?;
    let metadata = original.metadata()?;
    let mut bytes = Vec::new();
    original.read_to_end(&mut bytes)?;
    if bytes != self.original.as_bytes() {
      return Err("File changed during formatting".into());
    }
    let (temporary, mut file) =
      Temporary::create(path.parent().expect("absolute file path"))?;
    file.write_all(self.formatted.as_bytes())?;
    file.set_permissions(metadata.permissions())?;
    file.sync_all()?;
    if !std::fs::symlink_metadata(path)?.is_file()
      || std::fs::read(path)? != self.original.as_bytes()
    {
      return Err("File changed during formatting".into());
    }
    std::fs::rename(&temporary.path, path)?;
    Ok(())
  }
}

struct Temporary {
  path: PathBuf,
}

impl Temporary {
  fn create(directory: &Path) -> io::Result<(Self, File)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
      let path = directory.join(format!(
        ".shellsmith-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
      ));
      let mut options = OpenOptions::new();
      options.write(true).create_new(true);
      #[cfg(unix)]
      {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
      }
      match options.open(&path) {
        Ok(file) => return Ok((Self { path }, file)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
      }
    }
  }
}

impl Drop for Temporary {
  fn drop(&mut self) {
    let _ = std::fs::remove_file(&self.path);
  }
}

#[cfg(test)]
#[path = "../../tests/implementation/cli.rs"]
mod tests;
