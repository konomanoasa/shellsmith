use crate::interface::application::engine::{
  Document, DocumentId, Revision, Snapshot,
};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use super::Error;
use crate::interface::application::engine::word::Pattern;
use crate::interface::application::engine::{
  CharacterEncoding, InputQuery, InputResult, SourceInput, Value, ValueIssue,
  check_cancelled,
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

/// Selects files whose prefix cannot be read, so that loading reports them
/// unless an opened text replaces them.
pub(super) fn selected_shell(path: &Path, identity: &Path) -> bool {
  let mut prefix = Vec::new();
  std::fs::File::open(path)
    .and_then(|file| file.take(4096).read_to_end(&mut prefix))
    .is_err()
    || shell(&prefix, identity)
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

pub(super) fn entry_path(
  entries: &BTreeMap<PathBuf, Snapshot>,
  id: DocumentId,
) -> Option<&Path> {
  entries
    .iter()
    .find_map(|(path, entry)| (entry.document == id).then_some(path.as_path()))
}

pub(super) fn resolve(
  input: &SourceInput,
  opened: &BTreeMap<PathBuf, Arc<str>>,
  entries: &BTreeMap<PathBuf, Snapshot>,
) -> Result<PathBuf, String> {
  if input.needs_annotation()
    && let Some(annotation) = &input.annotation
  {
    let path = path_bytes(&annotation.path)?;
    let directory = entry_path(entries, annotation.document)
      .and_then(Path::parent)
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
      return Err("PATH search is undetermined".into());
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
  let overlay = opened.contains_key(&identity) && parents_exist(candidate);
  ordinary(candidate, overlay).map_err(|error| error.to_string())?;
  if !overlay {
    std::fs::File::open(candidate).map_err(|error| error.to_string())?;
  }
  Ok(identity)
}

/// Even unsaved files require existing directories before each `..`.
fn parents_exist(path: &Path) -> bool {
  let mut prefix = PathBuf::new();
  path.components().all(|component| {
    let exists = component != Component::ParentDir || prefix.is_dir();
    prefix.push(component);
    exists
  })
}

#[cfg(unix)]
fn path_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
  use std::os::unix::ffi::OsStrExt;
  if bytes.contains(&0) {
    return Err("NUL in path".into());
  }
  Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}
#[cfg(not(unix))]
fn path_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
  std::str::from_utf8(bytes)
    .map(PathBuf::from)
    .map_err(|error| error.to_string())
}

pub(super) fn evaluate(
  query: &InputQuery,
  cancelled: &AtomicBool,
) -> Result<InputResult, Error> {
  check_cancelled(cancelled)?;
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
  check_cancelled(cancelled)?;
  Ok(result)
}

fn directory(cwd: &[u8], path: &[u8], physical: bool) -> InputResult {
  let mut curpath = Vec::new();
  if !path.starts_with(b"/") {
    curpath.extend(cwd);
    if !curpath.ends_with(b"/") {
      curpath.push(b'/');
    }
  }
  curpath.extend(path);
  let Ok(joined) = path_bytes(&curpath) else {
    return InputResult::Unknown(ValueIssue::Nul);
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
  match logical(&curpath)
    .and_then(|curpath| existing_directory(&curpath).map(|()| curpath))
  {
    Ok(curpath) => InputResult::Path(curpath),
    Err(result) => result,
  }
}

fn existing_directory(path: &[u8]) -> Result<(), InputResult> {
  match path_bytes(path).map(std::fs::metadata) {
    Ok(Ok(metadata)) if metadata.is_dir() => Ok(()),
    Ok(Ok(_)) => Err(InputResult::Missing),
    Ok(Err(error)) => Err(failure(&error)),
    Err(_) => Err(InputResult::Unknown(ValueIssue::Nul)),
  }
}

/// Applies POSIX `cd` steps 8a and 8b to an absolute path, keeping the
/// slashes that step 8c only permits removing.
fn logical(curpath: &[u8]) -> Result<Vec<u8>, InputResult> {
  let render = |leading: usize, components: &[(&[u8], usize)]| {
    let mut path = vec![b'/'; leading];
    for (component, slashes) in components {
      path.extend(*component);
      path.extend(std::iter::repeat_n(b'/', *slashes));
    }
    path
  };
  let mut leading = curpath.iter().take_while(|byte| **byte == b'/').count();
  let mut components: Vec<(&[u8], usize)> = Vec::new();
  let mut rest = &curpath[leading..];
  while !rest.is_empty() {
    let end = rest.iter().position(|byte| *byte == b'/');
    let end = end.unwrap_or(rest.len());
    let slashes = rest[end..].iter().take_while(|byte| **byte == b'/').count();
    let component = &rest[..end];
    rest = &rest[end + slashes..];
    let removed = match component {
      b"." => true,
      b".." if components.last().is_some_and(|last| last.0 != b"..") => {
        existing_directory(&render(leading, &components))?;
        components.pop();
        true
      }
      _ => {
        components.push((component, slashes));
        false
      }
    };
    // Slashes after a removed final component are not separators to delete.
    if removed && rest.is_empty() {
      match components.last_mut() {
        Some(last) => last.1 += slashes,
        None => leading += slashes,
      }
    }
  }
  Ok(render(leading, &components))
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
    check_cancelled(cancelled)?;
    let protection = &quoted[offset..offset + part.len()];
    let pattern = match Pattern::compile(part, protection, utf8, portable) {
      Ok(p) => p,
      Err(issue) => return Ok(InputResult::Unknown(issue)),
    };
    if pattern.ambiguous_glob(cancelled) {
      return Ok(InputResult::Unknown(ValueIssue::Unspecified));
    }
    let last = offset + part.len() == bytes.len();
    let literal = pattern.literal();
    let mut next = Vec::new();
    for prefix in paths {
      check_cancelled(cancelled)?;
      let found = if let Some(literal) = &literal {
        let mut path = prefix.clone();
        path.extend(literal);
        if !last {
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
        let found = (candidate.exists()
          || std::fs::symlink_metadata(candidate).is_ok())
        .then_some(path);
        if literal.as_slice() == part {
          next.extend(found);
          continue;
        }
        Some(found)
      } else {
        None
      };
      let matched =
        match listed(&cwd, &prefix, &pattern, last, utf8, cancelled)? {
          Ok(matched) => matched,
          Err(issue) => return Ok(InputResult::Unknown(issue)),
        };
      // An escaped component may be looked up by name or matched against
      // directory entries, so only an agreeing result is determined.
      if found
        .is_some_and(|found| found.into_iter().ne(matched.iter().cloned()))
      {
        return Ok(InputResult::Unknown(ValueIssue::Unspecified));
      }
      next.extend(matched);
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

fn listed(
  cwd: &Path,
  prefix: &[u8],
  pattern: &Pattern,
  last: bool,
  utf8: bool,
  cancelled: &AtomicBool,
) -> Result<Result<Vec<Vec<u8>>, ValueIssue>, Error> {
  let directory = path_bytes(prefix).map_err(Error::Message)?;
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
      return Ok(Ok(Vec::new()));
    }
    Err(_) => return Ok(Err(ValueIssue::PathnameExpansion)),
  };
  let mut matched = Vec::new();
  for entry in entries {
    check_cancelled(cancelled)?;
    let Ok(entry) = entry else {
      return Ok(Err(ValueIssue::PathnameExpansion));
    };
    let name = entry.file_name();
    let name = name.as_encoded_bytes();
    match pattern.match_entry(name, utf8, cancelled) {
      Ok(true) => {}
      Ok(false) => continue,
      Err(issue) => return Ok(Err(issue)),
    }
    let mut path = prefix.to_vec();
    path.extend(name);
    if !last {
      if !entry.path().is_dir() {
        continue;
      }
      path.push(b'/');
    }
    matched.push(path);
  }
  Ok(Ok(matched))
}

#[cfg(unix)]
fn home(name: &[u8], cancelled: &AtomicBool) -> Result<InputResult, Error> {
  let Ok(name) = std::ffi::CString::new(name) else {
    return Ok(InputResult::Unknown(ValueIssue::Nul));
  };
  let mut size = 1024;
  loop {
    check_cancelled(cancelled)?;
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
    b"UTF-8" | b"UTF8" => CharacterEncoding::Utf8,
    b"US-ASCII" | b"ASCII" | b"ANSI_X3.4-1968" => CharacterEncoding::Ascii,
    _ => CharacterEncoding::Other,
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
  staged: Option<Arc<Document>>,
}

impl Documents {
  /// Shares parses across requests. Staged text replaces an input only
  /// hypothetically, so its parse stays apart from the input parse until the
  /// input has that text.
  pub(super) fn parse_entry(
    &self,
    entries: &mut BTreeMap<PathBuf, Snapshot>,
    path: &Path,
    bytes: &[u8],
    staged: bool,
    cancelled: &AtomicBool,
  ) -> Result<Arc<Document>, Error> {
    let after = entries.get(path).map_or(0, |entry| entry.revision.0);
    let document = self.parse(path, bytes, after, staged, cancelled)?;
    entries.insert(path.to_path_buf(), document.snapshot());
    Ok(document)
  }

  fn parse(
    &self,
    path: &Path,
    bytes: &[u8],
    after: u64,
    staged: bool,
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
            staged: None,
          }))
        })
        .clone()
    };
    let mut file = file
      .lock()
      .map_err(|error| Error::Message(error.to_string()))?;
    check_cancelled(cancelled)?;
    let reusable = |document: Option<&Arc<Document>>| {
      document
        .filter(|document| {
          document.snapshot().revision.0 > after
            && document.source().as_bytes() == bytes
        })
        .cloned()
    };
    if let Some(document) = reusable(file.document.as_ref()) {
      return Ok(document);
    }
    if let Some(document) = reusable(file.staged.as_ref()) {
      if !staged {
        file.document = file.staged.take();
      }
      return Ok(document);
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
    if staged {
      file.staged = Some(document.clone());
    } else {
      file.document = Some(document.clone());
      file.staged = None;
    }
    Ok(document)
  }
}
