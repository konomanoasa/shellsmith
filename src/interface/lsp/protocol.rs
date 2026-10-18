use crate::interface::application::engine::{
  DocumentId, Modifier, SemanticKind, SemanticSpan, SymbolDetails, SymbolKind,
};
use async_lsp::lsp_types::{
  SemanticToken, SemanticTokenModifier, SemanticTokenType,
};

use crate::interface::application::{
  Application, Error, Failure, RequestScope, Session, Settings,
};
use async_lsp::lsp_types::notification::Notification;
use async_lsp::lsp_types::{
  self as lsp, notification as notif, request as req,
};
use async_lsp::router::Router;
use async_lsp::{
  AnyEvent, AnyNotification, AnyRequest, ClientSocket, ErrorCode, LspService,
  MainLoop, RequestId, ResponseError,
};
use futures_util::future::{AbortHandle, Abortable};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tower::{Service, ServiceBuilder};
use url::Url;

type Reply<T> = Pin<Box<dyn Future<Output = Result<T, ResponseError>> + Send>>;

#[cfg(test)]
#[path = "../../../tests/implementation/protocol.rs"]
mod tests;

fn invalid(message: impl std::fmt::Display) -> ResponseError {
  ResponseError::new(ErrorCode::INVALID_PARAMS, message)
}

fn fault(error: Error) -> ResponseError {
  let code = match error {
    Error::RenameRefused(_) => ErrorCode::REQUEST_FAILED,
    Error::Superseded => ErrorCode::CONTENT_MODIFIED,
    Error::InvalidName(_)
    | Error::Engine(
      crate::interface::application::engine::Error::InvalidPosition,
    ) => ErrorCode::INVALID_PARAMS,
    Error::Engine(crate::interface::application::engine::Error::Cancelled) => {
      ErrorCode::REQUEST_CANCELLED
    }
    _ => ErrorCode::INTERNAL_ERROR,
  };
  ResponseError::new(code, error)
}

fn file_path(uri: &Url) -> Result<PathBuf, ResponseError> {
  if uri.scheme() != "file" || uri.query().is_some() || uri.fragment().is_some()
  {
    return Err(invalid("Expected a file URI"));
  }
  uri
    .to_file_path()
    .map_err(|()| invalid("Unsupported file URI"))
}

fn uri(path: &Path) -> Result<Url, Error> {
  Url::from_file_path(path)
    .map_err(|()| Error::Message("Cannot encode file URI".into()))
}

#[derive(Default)]
struct ClientSupport {
  refresh: bool,
  watch: bool,
  document_changes: bool,
  symbols: SymbolFormat,
}

#[derive(Clone, Copy, Default)]
enum SymbolFormat {
  #[default]
  Flat,
  Hierarchical,
}

struct Server {
  session: Arc<Session>,
  client: ClientSocket,
  versions: BTreeMap<PathBuf, i32>,
  document_uris: BTreeMap<PathBuf, Url>,
  support: ClientSupport,
  types: Vec<lsp::SemanticTokenType>,
  modifiers: Vec<lsp::SemanticTokenModifier>,
  semantic_tokens: bool,
  shutdown: Arc<AtomicBool>,
}

impl Server {
  #[expect(
    deprecated,
    reason = "LSP clients may initialize with rootUri when workspaceFolders is absent."
  )]
  fn initialize(
    &mut self,
    params: lsp::InitializeParams,
  ) -> Result<lsp::InitializeResult, ResponseError> {
    if let Some(folders) = params.workspace_folders {
      for folder in folders {
        self
          .session
          .add_input(&file_path(&folder.uri)?)
          .map_err(fault)?;
      }
    } else if let Some(root) = params.root_uri {
      self.session.add_input(&file_path(&root)?).map_err(fault)?;
    }
    let workspace = params.capabilities.workspace;
    self.support.document_changes = workspace
      .as_ref()
      .and_then(|workspace| workspace.workspace_edit.as_ref())
      .and_then(|edit| edit.document_changes)
      == Some(true);
    self.support.refresh = workspace
      .as_ref()
      .and_then(|workspace| workspace.semantic_tokens.as_ref())
      .and_then(|tokens| tokens.refresh_support)
      == Some(true);
    self.support.watch = workspace
      .as_ref()
      .and_then(|workspace| workspace.did_change_watched_files.as_ref())
      .and_then(|watch| watch.dynamic_registration)
      == Some(true);
    self.support.symbols = if params
      .capabilities
      .text_document
      .as_ref()
      .and_then(|text| text.document_symbol.as_ref())
      .and_then(|symbol| symbol.hierarchical_document_symbol_support)
      == Some(true)
    {
      SymbolFormat::Hierarchical
    } else {
      SymbolFormat::Flat
    };
    if let Some(capability) = params
      .capabilities
      .text_document
      .and_then(|text| text.semantic_tokens)
    {
      self.types = TYPES
        .iter()
        .filter(|name| {
          capability
            .token_types
            .iter()
            .any(|kind| kind.as_str() == **name)
        })
        .map(|name| lsp::SemanticTokenType::new(name))
        .collect();
      self.modifiers = MODIFIERS
        .iter()
        .filter(|name| {
          capability
            .token_modifiers
            .iter()
            .any(|kind| kind.as_str() == **name)
        })
        .map(|name| lsp::SemanticTokenModifier::new(name))
        .collect();
      self.semantic_tokens =
        matches!(
          capability.requests.full,
          Some(
            lsp::SemanticTokensFullOptions::Bool(true)
              | lsp::SemanticTokensFullOptions::Delta { .. }
          )
        ) && capability.formats.contains(&lsp::TokenFormat::RELATIVE);
    }
    Ok(lsp::InitializeResult {
      capabilities: self.capabilities(),
      server_info: Some(lsp::ServerInfo {
        name: "shellsmith".into(),
        version: Some(env!("CARGO_PKG_VERSION").into()),
      }),
    })
  }

  fn capabilities(&self) -> lsp::ServerCapabilities {
    lsp::ServerCapabilities {
      position_encoding: Some(lsp::PositionEncodingKind::UTF16),
      text_document_sync: Some(lsp::TextDocumentSyncCapability::Options(
        lsp::TextDocumentSyncOptions {
          open_close: Some(true),
          change: Some(lsp::TextDocumentSyncKind::FULL),
          save: Some(lsp::TextDocumentSyncSaveOptions::Supported(true)),
          ..Default::default()
        },
      )),
      definition_provider: Some(lsp::OneOf::Left(true)),
      rename_provider: Some(lsp::OneOf::Right(lsp::RenameOptions {
        prepare_provider: Some(true),
        work_done_progress_options: lsp::WorkDoneProgressOptions::default(),
      })),
      document_symbol_provider: Some(lsp::OneOf::Left(true)),
      document_link_provider: Some(lsp::DocumentLinkOptions {
        resolve_provider: Some(false),
        work_done_progress_options: lsp::WorkDoneProgressOptions::default(),
      }),
      workspace_symbol_provider: Some(lsp::OneOf::Left(true)),
      workspace: Some(lsp::WorkspaceServerCapabilities {
        workspace_folders: Some(lsp::WorkspaceFoldersServerCapabilities {
          supported: Some(true),
          change_notifications: Some(lsp::OneOf::Left(true)),
        }),
        ..Default::default()
      }),
      semantic_tokens_provider: self.semantic_tokens.then(|| {
        lsp::SemanticTokensOptions {
          legend: lsp::SemanticTokensLegend {
            token_types: self.types.clone(),
            token_modifiers: self.modifiers.clone(),
          },
          full: Some(lsp::SemanticTokensFullOptions::Bool(true)),
          ..Default::default()
        }
        .into()
      }),
      ..Default::default()
    }
  }

  fn log(&self, message: impl std::fmt::Display) {
    log(&self.client, message);
  }

  fn initialized(&self) {
    if let Err(error) = self.session.start_index() {
      self.log(error);
    }
    if self.support.watch {
      let client = self.client.clone();
      let session = self.session.clone();
      tokio::spawn(async move {
        let options = lsp::DidChangeWatchedFilesRegistrationOptions {
          watchers: vec![lsp::FileSystemWatcher {
            glob_pattern: lsp::GlobPattern::String("**/*".into()),
            kind: Some(
              lsp::WatchKind::Create
                | lsp::WatchKind::Change
                | lsp::WatchKind::Delete,
            ),
          }],
        };
        let result = client
          .request::<req::RegisterCapability>(lsp::RegistrationParams {
            registrations: vec![lsp::Registration {
              id: "shellsmith-files".into(),
              method: "workspace/didChangeWatchedFiles".into(),
              register_options: Some(
                serde_json::to_value(options)
                  .expect("serializable registration"),
              ),
            }],
          })
          .await;
        if let Err(error) = result {
          log(&client, error);
        } else if let Err(error) = session
          .set_watching(true)
          .and_then(|()| session.start_index())
        {
          log(&client, error);
        }
      });
    }
  }

  fn updated(&self, result: Result<(), ResponseError>) {
    if let Err(error) = result {
      self.log(error);
      return;
    }
    if let Err(error) = self.session.start_index() {
      self.log(error);
    }
    if self.support.refresh && self.semantic_tokens {
      let client = self.client.clone();
      tokio::spawn(async move {
        if let Err(error) =
          client.request::<req::SemanticTokensRefresh>(()).await
        {
          log(&client, error);
        }
      });
    }
  }

  fn open(
    &mut self,
    params: lsp::DidOpenTextDocumentParams,
  ) -> Result<(), ResponseError> {
    let document = params.text_document;
    let path = self
      .session
      .absolute(&file_path(&document.uri)?)
      .map_err(fault)?;
    if self.versions.contains_key(&path)
      || self.document_uris.values().any(|uri| *uri == document.uri)
    {
      return Err(invalid("Document is already open"));
    }
    self
      .session
      .set_overlay(&path, Some(document.text))
      .map_err(fault)?;
    self.document_uris.insert(path.clone(), document.uri);
    self.versions.insert(path, document.version);
    Ok(())
  }

  fn change(
    &mut self,
    params: lsp::DidChangeTextDocumentParams,
  ) -> Result<(), ResponseError> {
    file_path(&params.text_document.uri)?;
    let path = self
      .document_uris
      .iter()
      .find(|(_, uri)| **uri == params.text_document.uri)
      .map(|(path, _)| path.clone())
      .ok_or_else(|| invalid("Document is not open"))?;
    let previous = self
      .versions
      .get(&path)
      .ok_or_else(|| invalid("Document is not open"))?;
    if params.text_document.version <= *previous {
      return Err(invalid("Outdated document version"));
    }
    if params
      .content_changes
      .iter()
      .any(|change| change.range.is_some())
    {
      return Err(invalid("Full document changes required"));
    }
    let text = params
      .content_changes
      .into_iter()
      .last()
      .ok_or_else(|| invalid("Missing content changes"))?
      .text;
    self.session.set_overlay(&path, Some(text)).map_err(fault)?;
    self.versions.insert(path, params.text_document.version);
    Ok(())
  }

  fn close(
    &mut self,
    params: &lsp::DidCloseTextDocumentParams,
  ) -> Result<(), ResponseError> {
    file_path(&params.text_document.uri)?;
    let path = self
      .document_uris
      .iter()
      .find(|(_, uri)| **uri == params.text_document.uri)
      .map(|(path, _)| path.clone())
      .ok_or_else(|| invalid("Document is not open"))?;
    self.session.set_overlay(&path, None).map_err(fault)?;
    self.versions.remove(&path);
    self.document_uris.remove(&path);
    Ok(())
  }

  fn folders(
    &mut self,
    params: &lsp::DidChangeWorkspaceFoldersParams,
  ) -> Result<(), ResponseError> {
    let removed: Vec<_> = params
      .event
      .removed
      .iter()
      .map(|folder| file_path(&folder.uri))
      .collect::<Result<_, _>>()?;
    let added: Vec<_> = params
      .event
      .added
      .iter()
      .map(|folder| file_path(&folder.uri))
      .collect::<Result<_, _>>()?;
    for path in removed {
      self.session.remove_input(&path).map_err(fault)?;
    }
    for path in added {
      self.session.add_input(&path).map_err(fault)?;
    }
    Ok(())
  }

  fn query<T: Send + 'static>(
    &mut self,
    path: Option<&Url>,
    scope: fn(PathBuf) -> RequestScope,
    operation: impl FnOnce(
      &mut Application,
      Option<DocumentId>,
      &AtomicBool,
    ) -> Result<T, Error>
    + Send
    + 'static,
  ) -> Reply<T> {
    let path = match path.map(file_path).transpose() {
      Ok(path) => path,
      Err(error) => return Box::pin(std::future::ready(Err(error))),
    };
    let scope = match path.clone() {
      Some(path) => scope(path),
      None => RequestScope::Workspace,
    };
    let job = self.session.request(scope, move |application, cancelled| {
      let id = path
        .as_ref()
        .map(|path| {
          application.document_id(path).ok_or_else(|| {
            Error::Message(format!("Cannot load {}", path.display()))
          })
        })
        .transpose()?;
      let result = operation(application, id, cancelled)?;
      Ok((result, application.failures().to_vec()))
    });
    let client = self.client.clone();
    Box::pin(async move {
      let (result, failures): (T, Vec<Failure>) = job.await.map_err(fault)?;
      for failure in failures {
        log(
          &client,
          format!("{}: {}", failure.path.display(), failure.message),
        );
      }
      Ok(result)
    })
  }

  fn semantic(
    &mut self,
    params: &lsp::SemanticTokensParams,
  ) -> Reply<Option<lsp::SemanticTokensResult>> {
    if !self.semantic_tokens {
      return Box::pin(std::future::ready(Err(ResponseError::new(
        ErrorCode::METHOD_NOT_FOUND,
        "Semantic tokens were not negotiated",
      ))));
    }
    let types = self.types.clone();
    let modifiers = self.modifiers.clone();
    self.query(
      Some(&params.text_document.uri),
      RequestScope::Analysis,
      move |application, id, cancelled| {
        let id = id.expect("document request");
        let spans = application.classify(id, cancelled)?;
        let source = application
          .workspace()
          .document(id)
          .ok_or(
            crate::interface::application::engine::Error::UnknownDocument(id),
          )?
          .source();
        Ok(Some(lsp::SemanticTokensResult::Tokens(
          lsp::SemanticTokens {
            result_id: None,
            data: encode(source, &spans, &types, &modifiers),
          },
        )))
      },
    )
  }
  fn definitions(
    &mut self,
    params: &lsp::GotoDefinitionParams,
  ) -> Reply<Option<lsp::GotoDefinitionResponse>> {
    let position = params.text_document_position_params.position;
    self.query(
      Some(&params.text_document_position_params.text_document.uri),
      RequestScope::Analysis,
      move |application, id, cancelled| {
        let id = id.expect("document request");
        let source = application
          .workspace()
          .document(id)
          .ok_or(
            crate::interface::application::engine::Error::UnknownDocument(id),
          )?
          .source();
        let offset = Lines::new(source).offset(position).ok_or(
          crate::interface::application::engine::Error::InvalidPosition,
        )?;
        let definitions = application.definitions(id, offset, cancelled)?;
        if definitions.is_empty() {
          return Ok(None);
        }
        let locations = definitions
          .into_iter()
          .map(|location| {
            let source = application
              .workspace()
              .document(location.snapshot.document)
              .ok_or(
                crate::interface::application::engine::Error::UnknownDocument(
                  location.snapshot.document,
                ),
              )?
              .source();
            let path =
              application.path(location.snapshot.document).ok_or_else(
                || Error::Message("Unknown definition path".into()),
              )?;
            let range =
              Lines::new(source).range(&location.bytes).ok_or_else(|| {
                Error::Message("Invalid definition range".into())
              })?;
            Ok(lsp::Location {
              uri: uri(path)?,
              range,
            })
          })
          .collect::<Result<_, Error>>()?;
        Ok(Some(lsp::GotoDefinitionResponse::Array(locations)))
      },
    )
  }

  fn rename_uri(&self, uri: &Url) -> Result<(), ResponseError> {
    let requested = file_path(uri)?;
    if let Some((path, _)) =
      self.document_uris.iter().find(|(_, opened)| *opened == uri)
      && self.session.absolute(&requested).map_err(fault)? != *path
    {
      return Err(fault(Error::Superseded));
    }
    Ok(())
  }

  fn prepare_rename(
    &mut self,
    params: &lsp::TextDocumentPositionParams,
  ) -> Reply<Option<lsp::PrepareRenameResponse>> {
    if let Err(error) = self.rename_uri(&params.text_document.uri) {
      return Box::pin(std::future::ready(Err(error)));
    }
    let position = params.position;
    self.query(
      Some(&params.text_document.uri),
      RequestScope::Names,
      move |application, id, cancelled| {
        let id = id.expect("document request");
        let document = application.workspace().document(id).ok_or(
          crate::interface::application::engine::Error::UnknownDocument(id),
        )?;
        let offset = Lines::new(document.source()).offset(position).ok_or(
          crate::interface::application::engine::Error::InvalidPosition,
        )?;
        let path = application
          .path(id)
          .ok_or_else(|| Error::Message("Unknown document path".into()))?
          .to_path_buf();
        let Some(target) =
          application.prepare_rename(&path, offset, cancelled)?
        else {
          return Ok(None);
        };
        let source = application
          .workspace()
          .document(target.location.snapshot.document)
          .ok_or(
            crate::interface::application::engine::Error::UnknownDocument(
              target.location.snapshot.document,
            ),
          )?
          .source();
        let range = Lines::new(source).range(&target.location.bytes).ok_or(
          crate::interface::application::engine::Error::InvalidPosition,
        )?;
        Ok(Some(lsp::PrepareRenameResponse::RangeWithPlaceholder {
          range,
          placeholder: target.placeholder,
        }))
      },
    )
  }

  fn rename(
    &mut self,
    params: lsp::RenameParams,
  ) -> Reply<Option<lsp::WorkspaceEdit>> {
    let versions = self.versions.clone();
    let uris = self.document_uris.clone();
    let document_changes = self.support.document_changes;
    if let Err(error) =
      self.rename_uri(&params.text_document_position.text_document.uri)
    {
      return Box::pin(std::future::ready(Err(error)));
    }
    self.query(
      Some(&params.text_document_position.text_document.uri),
      RequestScope::Names,
      move |application, id, cancelled| {
        let id = id.expect("document request");
        let document = application.workspace().document(id).ok_or(
          crate::interface::application::engine::Error::UnknownDocument(id),
        )?;
        let offset = Lines::new(document.source())
          .offset(params.text_document_position.position)
          .ok_or(
            crate::interface::application::engine::Error::InvalidPosition,
          )?;
        let path = application
          .path(id)
          .ok_or_else(|| Error::Message("Unknown document path".into()))?
          .to_path_buf();
        let edits =
          application.rename(&path, offset, &params.new_name, cancelled)?;
        workspace_edit(application, edits, &uris, &versions, document_changes)
          .map(Some)
      },
    )
  }

  fn document_links(
    &mut self,
    params: &lsp::DocumentLinkParams,
  ) -> Reply<Option<Vec<lsp::DocumentLink>>> {
    self.query(
      Some(&params.text_document.uri),
      RequestScope::Analysis,
      |application, id, cancelled| {
        let id = id.expect("document request");
        let links = application.document_links(id, cancelled)?;
        let source = application
          .workspace()
          .document(id)
          .ok_or(
            crate::interface::application::engine::Error::UnknownDocument(id),
          )?
          .source();
        let positions = Lines::new(source);
        links
          .into_iter()
          .map(|(location, path)| {
            let range = positions.range(&location.bytes).ok_or_else(|| {
              Error::Message("Invalid document link range".into())
            })?;
            Ok(lsp::DocumentLink {
              range,
              target: Some(uri(&path)?),
              tooltip: None,
              data: None,
            })
          })
          .collect::<Result<Vec<_>, Error>>()
          .map(Some)
      },
    )
  }
}

fn workspace_edit(
  application: &Application,
  edits: Vec<crate::interface::application::RenameEdit>,
  uris: &BTreeMap<PathBuf, Url>,
  versions: &BTreeMap<PathBuf, i32>,
  document_changes: bool,
) -> Result<lsp::WorkspaceEdit, Error> {
  let mut documents: BTreeMap<PathBuf, Vec<lsp::TextEdit>> = BTreeMap::new();
  for edit in edits {
    let id = edit.location.snapshot.document;
    let document = application.workspace().document(id).ok_or(
      crate::interface::application::engine::Error::UnknownDocument(id),
    )?;
    let range = Lines::new(document.source())
      .range(&edit.location.bytes)
      .ok_or(crate::interface::application::engine::Error::InvalidPosition)?;
    let path = application
      .path(id)
      .ok_or_else(|| Error::Message("Unknown edited document path".into()))?;
    if let Some(uri) = uris.get(path)
      && uri
        .to_file_path()
        .ok()
        .is_none_or(|client_path| application.absolute(&client_path) != path)
    {
      return Err(Error::Superseded);
    }
    documents
      .entry(path.to_path_buf())
      .or_default()
      .push(lsp::TextEdit {
        range,
        new_text: edit.new_text,
      });
  }
  let mut output = lsp::WorkspaceEdit::default();
  if document_changes {
    output.document_changes = Some(lsp::DocumentChanges::Edits(
      documents
        .into_iter()
        .map(|(path, edits)| {
          Ok(lsp::TextDocumentEdit {
            text_document: lsp::OptionalVersionedTextDocumentIdentifier {
              uri: uris.get(&path).cloned().map_or_else(|| uri(&path), Ok)?,
              version: versions.get(&path).copied(),
            },
            edits: edits.into_iter().map(lsp::OneOf::Left).collect(),
          })
        })
        .collect::<Result<_, Error>>()?,
    ));
  } else {
    output.changes = Some(
      documents
        .into_iter()
        .map(|(path, edits)| {
          Ok((
            uris.get(&path).cloned().map_or_else(|| uri(&path), Ok)?,
            edits,
          ))
        })
        .collect::<Result<_, Error>>()?,
    );
  }
  Ok(output)
}

fn log(client: &ClientSocket, message: impl std::fmt::Display) {
  let _ = client.notify::<notif::LogMessage>(lsp::LogMessageParams {
    typ: lsp::MessageType::ERROR,
    message: message.to_string(),
  });
}

#[expect(
  deprecated,
  reason = "SymbolInformation requires initializing its deprecated field; None omits it from the response."
)]
fn symbols<'a>(
  application: &Application,
  locations: impl Iterator<Item = SymbolDetails<'a>>,
) -> Result<Vec<lsp::SymbolInformation>, Error> {
  let mut symbols = Vec::new();
  for entry in locations {
    let symbol = entry.symbol;
    let location = entry.location;
    let Some(doc) =
      application.workspace().document(location.snapshot.document)
    else {
      continue;
    };
    let lines = Lines::new(doc.source());
    let Some(range) = lines.range(&location.bytes) else {
      continue;
    };
    let path = application
      .path(location.snapshot.document)
      .ok_or_else(|| Error::Message("Unknown document path".into()))?;
    symbols.push(lsp::SymbolInformation {
      name: String::from_utf8_lossy(&symbol.name).into_owned(),
      kind: symbol_kind(symbol.kind),
      location: lsp::Location {
        uri: uri(path)?,
        range,
      },
      tags: None,
      deprecated: None,
      container_name: entry
        .container
        .map(|parent| String::from_utf8_lossy(&parent.name).into_owned()),
    });
  }
  Ok(symbols)
}

fn symbol_kind(kind: SymbolKind) -> lsp::SymbolKind {
  match kind {
    SymbolKind::Function => lsp::SymbolKind::FUNCTION,
    SymbolKind::Variable => lsp::SymbolKind::VARIABLE,
  }
}

#[expect(
  deprecated,
  reason = "DocumentSymbol requires initializing its deprecated field; None omits it from the response."
)]
fn document_symbols(
  application: &Application,
  id: DocumentId,
  format: SymbolFormat,
) -> Result<lsp::DocumentSymbolResponse, Error> {
  let entries = application.document_symbols(id)?;
  if matches!(format, SymbolFormat::Flat) {
    return symbols(application, entries.map(|entry| entry.details))
      .map(lsp::DocumentSymbolResponse::Flat);
  }
  let document = application.workspace().document(id).ok_or(Error::Engine(
    crate::interface::application::engine::Error::UnknownDocument(id),
  ))?;
  let lines = Lines::new(document.source());
  let entries: Vec<_> = entries.collect();
  let mut children: BTreeMap<Option<usize>, Vec<lsp::DocumentSymbol>> =
    BTreeMap::new();
  for (index, entry) in entries.into_iter().enumerate().rev() {
    let details = entry.details;
    let mut nested = children.remove(&Some(index)).unwrap_or_default();
    let Some((range, selection_range)) = lines
      .range(&details.range.as_ref().unwrap_or(&details.location).bytes)
      .zip(lines.range(&details.location.bytes))
    else {
      children.entry(entry.parent).or_default().extend(nested);
      continue;
    };
    nested.reverse();
    children
      .entry(entry.parent)
      .or_default()
      .push(lsp::DocumentSymbol {
        name: String::from_utf8_lossy(&details.symbol.name).into_owned(),
        detail: None,
        kind: symbol_kind(details.symbol.kind),
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children: (!nested.is_empty()).then_some(nested),
      });
  }
  let mut roots = children.remove(&None).unwrap_or_default();
  roots.reverse();
  Ok(lsp::DocumentSymbolResponse::Nested(roots))
}

fn router(server: Server) -> Router<Server> {
  let mut router = Router::new(server);
  router
    .request::<req::Initialize, _>(|server, params| {
      std::future::ready(server.initialize(params))
    })
    .request::<req::Shutdown, _>(|server, ()| {
      let result = server.session.invalidate().map_err(fault);
      server.shutdown.store(true, Ordering::Relaxed);
      std::future::ready(result)
    })
    .request::<req::SemanticTokensFullRequest, _>(|server, params| {
      server.semantic(&params)
    })
    .request::<req::GotoDefinition, _>(|server, params| {
      server.definitions(&params)
    })
    .request::<req::PrepareRenameRequest, _>(|server, params| {
      server.prepare_rename(&params)
    })
    .request::<req::Rename, _>(Server::rename)
    .request::<req::DocumentLinkRequest, _>(|server, params| {
      server.document_links(&params)
    })
    .request::<req::DocumentSymbolRequest, _>(|server, params| {
      let format = server.support.symbols;
      server.query(
        Some(&params.text_document.uri),
        RequestScope::Document,
        move |application, id, _| {
          document_symbols(application, id.expect("document request"), format)
            .map(Some)
        },
      )
    })
    .request::<req::WorkspaceSymbolRequest, _>(|server, params| {
      server.query(None, RequestScope::Document, move |application, _, _| {
        symbols(application, application.workspace_symbols(&params.query))
          .map(|symbols| Some(lsp::WorkspaceSymbolResponse::Flat(symbols)))
      })
    })
    .notification::<notif::Initialized>(|server, _| {
      server.initialized();
      ControlFlow::Continue(())
    })
    .notification::<notif::DidOpenTextDocument>(|server, params| {
      let result = server.open(params);
      server.updated(result);
      ControlFlow::Continue(())
    })
    .notification::<notif::DidChangeTextDocument>(|server, params| {
      let result = server.change(params);
      server.updated(result);
      ControlFlow::Continue(())
    })
    .notification::<notif::DidCloseTextDocument>(|server, params| {
      let result = server.close(&params);
      server.updated(result);
      ControlFlow::Continue(())
    })
    .notification::<notif::DidChangeWorkspaceFolders>(|server, params| {
      let result = server.folders(&params);
      server.updated(result);
      ControlFlow::Continue(())
    })
    .notification::<notif::DidChangeWatchedFiles>(|server, params| {
      let result = params
        .changes
        .iter()
        .map(|event| file_path(&event.uri))
        .collect::<Result<Vec<_>, _>>()
        .and_then(|paths| server.session.files_changed(&paths).map_err(fault));
      server.updated(result);
      ControlFlow::Continue(())
    })
    .notification::<notif::DidSaveTextDocument>(|server, params| {
      let result = file_path(&params.text_document.uri)
        .and_then(|path| server.session.files_changed(&[path]).map_err(fault));
      server.updated(result);
      ControlFlow::Continue(())
    })
    .unhandled_notification(|_, _| ControlFlow::Continue(()));
  router
}

pub async fn run(
  settings: Settings,
  input: impl tokio::io::AsyncRead + Unpin,
  output: impl tokio::io::AsyncWrite + Unpin,
) -> Result<bool, Error> {
  let session = Arc::new(Session::new(settings)?);
  let shutdown = Arc::new(AtomicBool::new(false));
  let ended = shutdown.clone();
  let (main_loop, _) = MainLoop::new_server(move |client| {
    ServiceBuilder::new()
      .layer(async_lsp::server::LifecycleLayer::default())
      .service(Requests::new(router(Server {
        session,
        client,
        versions: BTreeMap::new(),
        document_uris: BTreeMap::new(),
        support: ClientSupport::default(),
        types: Vec::new(),
        modifiers: Vec::new(),
        semantic_tokens: false,
        shutdown,
      })))
  });
  main_loop
    .run_buffered(input.compat(), output.compat_write())
    .await
    .map_err(|error| Error::Message(error.to_string()))?;
  Ok(ended.load(Ordering::Relaxed))
}

pub fn stdio() -> Result<bool, Error> {
  let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()?;
  let result = runtime.block_on(run(
    Settings::new(std::env::current_dir()?),
    tokio::io::stdin(),
    tokio::io::stdout(),
  ));
  runtime.shutdown_background();
  result
}

// Keep notifications flowing while application workers wait for CPU capacity.
struct Requests<S> {
  inner: S,
  pending: HashMap<RequestId, (AbortHandle, Weak<()>)>,
}

impl<S> Requests<S> {
  fn new(inner: S) -> Self {
    Self {
      inner,
      pending: HashMap::new(),
    }
  }
}

impl<S> Service<AnyRequest> for Requests<S>
where
  S: LspService<Error = ResponseError>,
  S::Future: Send + 'static,
  S::Response: Send + 'static,
{
  type Response = S::Response;
  type Error = ResponseError;
  type Future = Reply<Self::Response>;

  fn poll_ready(
    &mut self,
    cx: &mut Context<'_>,
  ) -> Poll<Result<(), Self::Error>> {
    self.inner.poll_ready(cx)
  }

  fn call(&mut self, request: AnyRequest) -> Self::Future {
    self
      .pending
      .retain(|_, (_, lifetime)| lifetime.strong_count() != 0);
    if self.pending.contains_key(&request.id) {
      return Box::pin(std::future::ready(Err(ResponseError::new(
        ErrorCode::INVALID_REQUEST,
        "Request ID is already active",
      ))));
    }
    let lifetime = Arc::new(());
    let (handle, registration) = AbortHandle::new_pair();
    self
      .pending
      .insert(request.id.clone(), (handle, Arc::downgrade(&lifetime)));
    let future = Abortable::new(self.inner.call(request), registration);
    Box::pin(async move {
      let result = future.await;
      drop(lifetime);
      result.unwrap_or_else(|_| {
        Err(ResponseError::new(
          ErrorCode::REQUEST_CANCELLED,
          "Request cancelled",
        ))
      })
    })
  }
}

impl<S> LspService for Requests<S>
where
  S: LspService<Error = ResponseError>,
  S::Future: Send + 'static,
  S::Response: Send + 'static,
{
  fn notify(
    &mut self,
    notification: AnyNotification,
  ) -> ControlFlow<async_lsp::Result<()>> {
    if notification.method != notif::Cancel::METHOD {
      return self.inner.notify(notification);
    }
    let params: lsp::CancelParams =
      match serde_json::from_value(notification.params) {
        Ok(params) => params,
        Err(error) => return ControlFlow::Break(Err(error.into())),
      };
    if let Some((handle, _)) = self.pending.get(&params.id) {
      handle.abort();
    }
    ControlFlow::Continue(())
  }

  fn emit(&mut self, event: AnyEvent) -> ControlFlow<async_lsp::Result<()>> {
    self.inner.emit(event)
  }
}

const TYPES: &[&str] = &[
  "comment",
  "keyword",
  "operator",
  "string",
  "number",
  "variable",
  "parameter",
  "function",
  "command",
];
const MODIFIERS: &[&str] = &[
  "declaration",
  "definition",
  "modification",
  "readonly",
  "defaultLibrary",
  "environment",
];

struct Lines<'a> {
  source: &'a str,
  starts: Vec<usize>,
  ends: Vec<usize>,
  excess: Vec<(usize, usize)>,
}
impl<'a> Lines<'a> {
  fn new(source: &'a str) -> Self {
    let mut total = 0;
    let excess = source
      .char_indices()
      .filter_map(|(offset, character)| {
        if character.is_ascii() {
          return None;
        }
        total += character.len_utf8() - character.len_utf16();
        Some((offset + character.len_utf8(), total))
      })
      .collect();
    let bytes = source.as_bytes();
    let mut starts = vec![0];
    let mut ends = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
      if matches!(bytes[offset], b'\r' | b'\n') {
        ends.push(offset);
        if bytes[offset] == b'\r' && bytes.get(offset + 1) == Some(&b'\n') {
          offset += 1;
        }
        starts.push(offset + 1);
      }
      offset += 1;
    }
    ends.push(source.len());
    Self {
      source,
      starts,
      ends,
      excess,
    }
  }

  fn offset(&self, position: lsp::Position) -> Option<usize> {
    let line = usize::try_from(position.line).ok()?;
    let Some(&start) = self.starts.get(line) else {
      return Some(self.source.len());
    };
    let end = self.ends[line];
    let target = usize::try_from(position.character).ok()?;
    let mut column = 0;
    for (offset, character) in self.source[start..end].char_indices() {
      if column == target {
        return Some(start + offset);
      }
      column += character.len_utf16();
      if column > target {
        return None;
      }
    }
    Some(end)
  }

  fn range(
    &self,
    bytes: &std::ops::Range<usize>,
  ) -> Option<async_lsp::lsp_types::Range> {
    let (sl, sc) = self.position(bytes.start)?;
    let (el, ec) = self.position(bytes.end)?;
    Some(async_lsp::lsp_types::Range::new(
      async_lsp::lsp_types::Position::new(sl, sc),
      async_lsp::lsp_types::Position::new(el, ec),
    ))
  }
  fn excess_at(&self, offset: usize) -> usize {
    let index = self.excess.partition_point(|(end, _)| *end <= offset);
    index.checked_sub(1).map_or(0, |index| self.excess[index].1)
  }
  fn position(&self, offset: usize) -> Option<(u32, u32)> {
    if offset > self.source.len() || !self.source.is_char_boundary(offset) {
      return None;
    }
    let line = self
      .starts
      .partition_point(|start| *start <= offset)
      .checked_sub(1)?;
    let offset = offset.min(self.ends[line]);
    Some((
      u32::try_from(line).ok()?,
      u32::try_from(
        offset
          - self.starts[line]
          - (self.excess_at(offset) - self.excess_at(self.starts[line])),
      )
      .ok()?,
    ))
  }
}

fn encode(
  source: &str,
  spans: &[SemanticSpan],
  types: &[SemanticTokenType],
  modifiers: &[SemanticTokenModifier],
) -> Vec<SemanticToken> {
  let mut events: BTreeMap<usize, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
  for (index, span) in spans.iter().enumerate() {
    let range = &span.location.bytes;
    if range.is_empty()
      || range.end > source.len()
      || !source.is_char_boundary(range.start)
      || !source.is_char_boundary(range.end)
      || !types
        .iter()
        .any(|name| name.as_str() == token_kind(span.kind))
    {
      continue;
    }
    events.entry(range.start).or_default().0.push(index);
    events.entry(range.end).or_default().1.push(index);
  }
  let mut active = BTreeSet::new();
  let mut ranges = Vec::new();
  let mut previous = 0;
  for (offset, (starts, ends)) in events {
    if previous < offset
      && let Some((_, _, index)) = active.last()
    {
      ranges.push((previous, offset, *index));
    }
    for index in ends {
      let span = &spans[index];
      active.remove(&(
        span.semantic,
        std::cmp::Reverse(span.location.bytes.len()),
        index,
      ));
    }
    for index in starts {
      let span = &spans[index];
      active.insert((
        span.semantic,
        std::cmp::Reverse(span.location.bytes.len()),
        index,
      ));
    }
    previous = offset;
  }
  let lines = Lines::new(source);
  let mut tokens: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
  for (start, end, index) in ranges {
    let span = &spans[index];
    let Some(kind) = types
      .iter()
      .position(|name| name.as_str() == token_kind(span.kind))
      .and_then(|i| u32::try_from(i).ok())
    else {
      continue;
    };
    let bits = span
      .modifiers
      .iter()
      .filter_map(|modifier| {
        modifiers
          .iter()
          .position(|name| name.as_str() == token_modifier(*modifier))
      })
      .fold(0, |bits, index| bits | (1 << index));
    let mut offset = start;
    while offset < end {
      let row = lines.starts.partition_point(|start| *start <= offset) - 1;
      let text = &source[offset..end.min(lines.ends[row]).max(offset)];
      if let Some((line, column)) = lines.position(offset)
        && let Ok(length) = u32::try_from(text.encode_utf16().count())
        && length > 0
      {
        if let Some(last) = tokens.last_mut()
          && last.0 == line
          && last.1 + last.2 == column
          && last.3 == kind
          && last.4 == bits
        {
          last.2 += length;
        } else {
          tokens.push((line, column, length, kind, bits));
        }
      }
      offset = lines.starts.get(row + 1).copied().unwrap_or(end).min(end);
    }
  }
  encode_relative(tokens)
}

fn encode_relative(
  tokens: Vec<(u32, u32, u32, u32, u32)>,
) -> Vec<SemanticToken> {
  let mut encoded = Vec::with_capacity(tokens.len());
  let (mut last_line, mut last_column) = (0, 0);
  for (line, column, length, kind, bits) in tokens {
    encoded.push(SemanticToken {
      delta_line: line - last_line,
      delta_start: if line == last_line {
        column - last_column
      } else {
        column
      },
      length,
      token_type: kind,
      token_modifiers_bitset: bits,
    });
    last_line = line;
    last_column = column;
  }
  encoded
}

const fn token_kind(kind: SemanticKind) -> &'static str {
  match kind {
    SemanticKind::Comment => "comment",
    SemanticKind::Keyword => "keyword",
    SemanticKind::Operator => "operator",
    SemanticKind::String => "string",
    SemanticKind::Number => "number",
    SemanticKind::Variable => "variable",
    SemanticKind::Parameter => "parameter",
    SemanticKind::Function => "function",
    SemanticKind::Command => "command",
  }
}
const fn token_modifier(modifier: Modifier) -> &'static str {
  match modifier {
    Modifier::Declaration => "declaration",
    Modifier::Definition => "definition",
    Modifier::Modification => "modification",
    Modifier::Readonly => "readonly",
    Modifier::DefaultLibrary => "defaultLibrary",
    Modifier::Environment => "environment",
  }
}
