use crate::interface::application::engine::{
  Document, DocumentId, Revision, Snapshot,
};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use super::{Error, check};
use crate::interface::application::engine::word::{Pattern, characters};
use crate::interface::application::engine::{
  InputQuery, InputResult, Value, ValueIssue,
};

pub(super) fn absolute(cwd: &Path, path: &Path) -> PathBuf {
  let joined = if path.is_absolute() {
    path.to_path_buf()
  } else {
    cwd.join(path)
  };
  match joined.canonicalize() {
    Ok(path) => return path,
    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
      return joined;
    }
    Err(_) => {}
  }
  let resolved = joined.ancestors().find_map(|ancestor| {
    ancestor.canonicalize().ok().map(|prefix| {
      prefix.join(joined.strip_prefix(ancestor).expect("path ancestor"))
    })
  });
  let mut result = PathBuf::new();
  for part in resolved.as_ref().unwrap_or(&joined).components() {
    match part {
      Component::CurDir => {}
      Component::ParentDir => {
        result.pop();
      }
      part => result.push(part.as_os_str()),
    }
  }
  result
}

pub(super) fn ordinary(path: &Path, missing: bool) -> Result<(), Error> {
  match std::fs::metadata(path) {
    Ok(metadata) if metadata.is_file() => Ok(()),
    Ok(_) => Err(Error::Message(format!(
      "Not a regular file: {}",
      path.display()
    ))),
    Err(error) if missing && error.kind() == std::io::ErrorKind::NotFound => {
      Ok(())
    }
    Err(error) => Err(error.into()),
  }
}

pub(super) fn read(
  path: &Path,
  opened: &BTreeMap<PathBuf, Arc<str>>,
) -> Result<Vec<u8>, Error> {
  if let Some(source) = opened.get(path) {
    ordinary(path, true)?;
    Ok(source.as_bytes().to_vec())
  } else {
    ordinary(path, false)?;
    std::fs::read(path).map_err(Error::from)
  }
}

pub(super) fn selected_shell(
  path: &Path,
  identity: &Path,
) -> Result<bool, Error> {
  let mut prefix = Vec::new();
  std::fs::File::open(path)?
    .take(4096)
    .read_to_end(&mut prefix)?;
  Ok(shell(&prefix, identity))
}

fn shell(source: &[u8], path: &Path) -> bool {
  let line = source
    .split(|byte| *byte == b'\n')
    .next()
    .unwrap_or_default();
  if let Some(shebang) = line.strip_prefix(b"#!") {
    let words: Vec<_> = shebang
      .split(u8::is_ascii_whitespace)
      .filter(|word| !word.is_empty())
      .collect();
    if let Some(interpreter) = words.first() {
      if interpreter.starts_with(b"/")
        && interpreter.rsplit(|byte| *byte == b'/').next() == Some(b"sh")
      {
        return true;
      }
      if *interpreter == b"/usr/bin/env"
        && words.get(1) == Some(&b"sh".as_slice())
        && words.len() == 2
      {
        return true;
      }
    }
    return false;
  }
  path.extension().is_some_and(|extension| extension == "sh")
}

pub(super) fn resolve(
  input: &crate::interface::application::engine::SourceInput,
  opened: &BTreeMap<PathBuf, std::sync::Arc<str>>,
  entries: &BTreeMap<PathBuf, Snapshot>,
) -> Result<PathBuf, String> {
  if input.needs_annotation()
    && let Some(annotation) = &input.annotation
  {
    let path = path_bytes(&annotation.path)?;
    let directory = entries
      .iter()
      .find(|(_, snapshot)| snapshot.document == annotation.document)
      .and_then(|(path, _)| path.parent())
      .ok_or("Source annotation document is unavailable")?;
    return resolve_file(&directory.join(path), directory, opened);
  }
  let Value::Literal(value) = &input.operand else {
    return Err(format!("{:?}", input.operand));
  };
  if value.is_empty() {
    return Err("Empty source operand".into());
  }
  let path = path_bytes(value)?;
  let cwd = if input.cwd.is_none() {
    PathBuf::new()
  } else {
    let Some(Value::Literal(cwd)) = &input.cwd else {
      return Err("Working directory is unknown".into());
    };
    path_bytes(cwd)?
  };
  let candidates = if path.is_absolute() || value.contains(&b'/') {
    vec![cwd.join(&path)]
  } else {
    let Some(Value::Literal(search_path)) = &input.search_path else {
      return Err("PATH is unset or unknown".into());
    };
    search_path
      .split(|byte| *byte == b':')
      .map(|entry| path_bytes(entry).map(|entry| cwd.join(entry).join(&path)))
      .collect::<Result<Vec<_>, _>>()?
  };
  let search = !value.contains(&b'/');
  for candidate in candidates {
    match resolve_file(&candidate, &cwd, opened) {
      Ok(identity) => return Ok(identity),
      Err(error) if !search => return Err(error),
      Err(_) => {}
    }
  }
  Err("Source file not found".into())
}

fn resolve_file(
  candidate: &Path,
  cwd: &Path,
  opened: &BTreeMap<PathBuf, Arc<str>>,
) -> Result<PathBuf, String> {
  let identity = absolute(cwd, candidate);
  let overlay = opened.contains_key(&identity);
  ordinary(candidate, overlay).map_err(|error| error.to_string())?;
  if !overlay {
    std::fs::File::open(candidate).map_err(|error| error.to_string())?;
  }
  Ok(identity)
}

#[cfg(unix)]
pub(super) fn path_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
  use std::os::unix::ffi::OsStrExt;
  if bytes.contains(&0) {
    return Err("NUL in path".into());
  }
  Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}
#[cfg(not(unix))]
pub(super) fn path_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
  std::str::from_utf8(bytes)
    .map(PathBuf::from)
    .map_err(|error| error.to_string())
}

pub(super) fn evaluate(
  query: &InputQuery,
  cancelled: &AtomicBool,
) -> Result<InputResult, Error> {
  check(cancelled)?;
  let result = match query {
    InputQuery::Home(name) => home(name, cancelled)?,
    InputQuery::Encoding(name) => encoding(name),
    InputQuery::Directory {
      cwd,
      path,
      physical,
    } => directory(cwd, path, *physical),
    InputQuery::Glob {
      cwd,
      pattern,
      quoted,
      utf8,
      portable_collation,
    } => glob(cwd, pattern, quoted, *utf8, *portable_collation, cancelled)?,
  };
  check(cancelled)?;
  Ok(result)
}

fn directory(cwd: &[u8], path: &[u8], physical: bool) -> InputResult {
  let (Ok(cwd), Ok(path)) = (path_bytes(cwd), path_bytes(path)) else {
    return InputResult::Unknown(ValueIssue::Nul);
  };
  let joined = if path.is_absolute() {
    path
  } else {
    cwd.join(path)
  };
  if physical {
    return match joined.canonicalize() {
      Ok(path) if path.is_dir() => {
        InputResult::Path(path.as_os_str().as_encoded_bytes().to_vec())
      }
      Ok(_) => InputResult::Missing,
      Err(error) => failure(&error),
    };
  }
  let mut logical = PathBuf::new();
  for component in joined.components() {
    match component {
      Component::CurDir => {}
      Component::ParentDir => match std::fs::metadata(&logical) {
        Ok(metadata) if metadata.is_dir() => {
          logical.pop();
        }
        Ok(_) => return InputResult::Missing,
        Err(error) => return failure(&error),
      },
      component => logical.push(component),
    }
  }
  match std::fs::metadata(&logical) {
    Ok(metadata) if metadata.is_dir() => {
      InputResult::Path(logical.as_os_str().as_encoded_bytes().to_vec())
    }
    Ok(_) => InputResult::Missing,
    Err(error) => failure(&error),
  }
}

fn failure(error: &std::io::Error) -> InputResult {
  if matches!(
    error.kind(),
    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
  ) {
    InputResult::Missing
  } else {
    InputResult::Unknown(ValueIssue::Dynamic)
  }
}

fn glob(
  cwd: &[u8],
  bytes: &[u8],
  quoted: &[bool],
  utf8: bool,
  portable: bool,
  cancelled: &AtomicBool,
) -> Result<InputResult, Error> {
  let Ok(cwd) = path_bytes(cwd) else {
    return Ok(InputResult::Unknown(ValueIssue::Nul));
  };
  let mut paths = vec![Vec::new()];
  let mut offset = 0;
  for part in bytes.split(|c| *c == b'/') {
    check(cancelled)?;
    let protection = &quoted[offset..offset + part.len()];
    let pattern = match Pattern::compile(part, protection, utf8, portable) {
      Ok(p) => p,
      Err(issue) => return Ok(InputResult::Unknown(issue)),
    };
    if pattern.ambiguous_glob(cancelled) {
      return Ok(InputResult::Unknown(ValueIssue::Unspecified));
    }
    let mut next = Vec::new();
    for prefix in paths {
      check(cancelled)?;
      if pattern.is_literal() {
        let mut path = prefix;
        path.extend(part);
        if offset + part.len() < bytes.len() {
          path.push(b'/');
        }
        let Ok(relative) = path_bytes(&path) else {
          return Ok(InputResult::Unknown(ValueIssue::Nul));
        };
        let candidate = if relative.is_absolute() {
          relative
        } else {
          cwd.join(relative)
        };
        if candidate.exists() || std::fs::symlink_metadata(candidate).is_ok() {
          next.push(path);
        }
        continue;
      }
      let directory = path_bytes(&prefix).map_err(Error::Message)?;
      let directory = if directory.is_absolute() {
        directory
      } else {
        cwd.join(directory)
      };
      let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error)
          if matches!(
            error.kind(),
            std::io::ErrorKind::NotFound
              | std::io::ErrorKind::NotADirectory
              | std::io::ErrorKind::PermissionDenied
          ) =>
        {
          continue;
        }
        Err(_) => {
          return Ok(InputResult::Unknown(ValueIssue::PathnameExpansion));
        }
      };
      for entry in entries {
        check(cancelled)?;
        let Ok(entry) = entry else {
          return Ok(InputResult::Unknown(ValueIssue::PathnameExpansion));
        };
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        let chars = match characters(name, utf8) {
          Ok(c) => c,
          Err(issue) => return Ok(InputResult::Unknown(issue)),
        };
        if name.starts_with(b".") && !pattern.starts_dot() {
          if pattern.ambiguous_dot() && pattern.matches(&chars, cancelled) {
            return Ok(InputResult::Unknown(ValueIssue::Unspecified));
          }
          continue;
        }
        if pattern.matches(&chars, cancelled) {
          let mut path = prefix.clone();
          path.extend(name);
          if offset + part.len() < bytes.len() {
            if !entry.path().is_dir() {
              continue;
            }
            path.push(b'/');
          }
          next.push(path);
        }
      }
    }
    paths = next;
    offset += part.len() + 1;
  }
  if !portable && paths.len() > 1 {
    return Ok(InputResult::Unknown(ValueIssue::Locale));
  }
  paths.sort();
  paths.dedup();
  Ok(InputResult::Paths(paths))
}

#[cfg(unix)]
fn home(name: &[u8], cancelled: &AtomicBool) -> Result<InputResult, Error> {
  let Ok(name) = std::ffi::CString::new(name) else {
    return Ok(InputResult::Unknown(ValueIssue::Nul));
  };
  let mut size = 1024;
  loop {
    check(cancelled)?;
    let mut buffer = Vec::<libc::c_char>::new();
    buffer
      .try_reserve_exact(size)
      .map_err(|e| Error::Message(e.to_string()))?;
    buffer.resize(size, 0);
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut found = std::ptr::null_mut();
    // Both output buffers remain alive throughout the call.
    let code = unsafe {
      libc::getpwnam_r(
        name.as_ptr(),
        record.as_mut_ptr(),
        buffer.as_mut_ptr(),
        buffer.len(),
        &raw mut found,
      )
    };
    if code == libc::ERANGE {
      size = size
        .checked_mul(2)
        .ok_or_else(|| Error::Message("User record is too large".into()))?;
      continue;
    }
    if code != 0 {
      return Ok(InputResult::Unknown(ValueIssue::Dynamic));
    }
    if found.is_null() {
      return Ok(InputResult::Missing);
    }
    // Success initializes the record; pw_dir borrows the live buffer.
    let bytes =
      unsafe { std::ffi::CStr::from_ptr(record.assume_init().pw_dir) }
        .to_bytes()
        .to_vec();
    return Ok(InputResult::Path(bytes));
  }
}

#[cfg(not(unix))]
fn home(_name: &[u8], _cancelled: &AtomicBool) -> Result<InputResult, Error> {
  Ok(InputResult::Unknown(ValueIssue::Dynamic))
}

#[cfg(unix)]
fn encoding(name: &[u8]) -> InputResult {
  #[cfg(target_vendor = "apple")]
  unsafe extern "C" {
    fn nl_langinfo_l(
      item: libc::nl_item,
      locale: libc::locale_t,
    ) -> *mut libc::c_char;
  }
  #[cfg(not(target_vendor = "apple"))]
  use libc::nl_langinfo_l;
  let Ok(name) = std::ffi::CString::new(name) else {
    return InputResult::Unknown(ValueIssue::Nul);
  };
  // A private locale avoids changing process-wide state.
  let locale = unsafe {
    libc::newlocale(libc::LC_CTYPE_MASK, name.as_ptr(), std::ptr::null_mut())
  };
  if locale.is_null() {
    return InputResult::Unknown(ValueIssue::Locale);
  }
  // Copy the borrowed codeset before freeing the locale.
  let codeset =
    unsafe { std::ffi::CStr::from_ptr(nl_langinfo_l(libc::CODESET, locale)) }
      .to_bytes()
      .to_ascii_uppercase();
  unsafe {
    libc::freelocale(locale);
  }
  let encoding = match codeset.as_slice() {
    b"UTF-8" | b"UTF8" => {
      crate::interface::application::engine::CharacterEncoding::Utf8
    }
    b"US-ASCII" | b"ASCII" | b"ANSI_X3.4-1968" => {
      crate::interface::application::engine::CharacterEncoding::Ascii
    }
    _ => crate::interface::application::engine::CharacterEncoding::Other,
  };
  InputResult::Encoding(encoding)
}

#[cfg(not(unix))]
fn encoding(_name: &[u8]) -> InputResult {
  InputResult::Unknown(ValueIssue::Locale)
}

#[derive(Debug, Default)]
pub(super) struct Documents {
  files: Mutex<BTreeMap<PathBuf, Arc<Mutex<ParsedFile>>>>,
}

#[derive(Debug)]
struct ParsedFile {
  id: DocumentId,
  revision: u64,
  document: Option<Arc<Document>>,
}

impl Documents {
  pub(super) fn parse(
    &self,
    path: &Path,
    bytes: &[u8],
    after: u64,
    cancelled: &AtomicBool,
  ) -> Result<Arc<Document>, Error> {
    let file = {
      let mut files = self
        .files
        .lock()
        .map_err(|error| Error::Message(error.to_string()))?;
      let id = DocumentId(files.len() as u64);
      files
        .entry(path.to_path_buf())
        .or_insert_with(|| {
          Arc::new(Mutex::new(ParsedFile {
            id,
            revision: 0,
            document: None,
          }))
        })
        .clone()
    };
    let mut file = file
      .lock()
      .map_err(|error| Error::Message(error.to_string()))?;
    check(cancelled)?;
    if let Some(document) = &file.document
      && document.snapshot().revision.0 > after
      && document.source().as_bytes() == bytes
    {
      return Ok(document.clone());
    }
    file.revision = file.revision.max(after) + 1;
    let document = Arc::new(Document::parse(
      Snapshot {
        document: file.id,
        revision: Revision(file.revision),
      },
      bytes,
      cancelled,
    )?);
    file.document = Some(document.clone());
    Ok(document)
  }
}
