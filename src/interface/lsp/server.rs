use super::encoding::{
  MODIFIERS, OpenDocument, Positions, SymbolFormat, TYPES, document_offset,
  document_source, document_symbols, encode, fault, file_path, invalid,
  locations, symbols, uri, workspace_edit,
};
use crate::interface::application::engine::DocumentId;
use crate::interface::application::{
  Application, Error, RequestScope, Session,
};
use async_lsp::lsp_types::{
  self as lsp, notification as notif, request as req,
};
use async_lsp::router::Router;
use async_lsp::{ClientSocket, ErrorCode, ResponseError};
use std::collections::BTreeMap;
use std::future::Future;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use url::Url;

type Reply<T> = Pin<Box<dyn Future<Output = Result<T, ResponseError>> + Send>>;

#[derive(Default)]
struct ClientSupport {
  refresh: bool,
  watch: bool,
  document_changes: bool,
  symbols: SymbolFormat,
  /// Present only when the client accepts full, relative-format tokens.
  semantic_tokens: Option<lsp::SemanticTokensLegend>,
}

pub(super) struct Server {
  session: Arc<Session>,
  client: ClientSocket,
  opened: BTreeMap<PathBuf, OpenDocument>,
  support: ClientSupport,
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
    let roots = if let Some(folders) = &params.workspace_folders {
      self.file_paths(folders.iter().map(|folder| &folder.uri))
    } else {
      self.file_paths(params.root_uri.iter())
    };
    for root in roots {
      self.session.add_input(&root).map_err(fault)?;
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
    self.support.semantic_tokens = params
      .capabilities
      .text_document
      .and_then(|text| text.semantic_tokens)
      .filter(|capability| {
        matches!(
          capability.requests.full,
          Some(
            lsp::SemanticTokensFullOptions::Bool(true)
              | lsp::SemanticTokensFullOptions::Delta { .. }
          )
        ) && capability.formats.contains(&lsp::TokenFormat::RELATIVE)
      })
      .map(|capability| lsp::SemanticTokensLegend {
        token_types: TYPES
          .iter()
          .filter(|name| {
            capability
              .token_types
              .iter()
              .any(|kind| kind.as_str() == **name)
          })
          .map(|name| lsp::SemanticTokenType::new(name))
          .collect(),
        token_modifiers: MODIFIERS
          .iter()
          .filter(|name| {
            capability
              .token_modifiers
              .iter()
              .any(|kind| kind.as_str() == **name)
          })
          .map(|name| lsp::SemanticTokenModifier::new(name))
          .collect(),
      });
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
      references_provider: Some(lsp::OneOf::Left(true)),
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
      semantic_tokens_provider: self.support.semantic_tokens.clone().map(
        |legend| {
          lsp::SemanticTokensOptions {
            legend,
            full: Some(lsp::SemanticTokensFullOptions::Bool(true)),
            ..Default::default()
          }
          .into()
        },
      ),
      ..Default::default()
    }
  }

  fn log(&self, message: impl std::fmt::Display) {
    log(&self.client, message);
  }

  fn initialized(&self) {
    if self.support.refresh && self.support.semantic_tokens.is_some() {
      match self.session.updates() {
        Ok(mut updates) => {
          let client = self.client.clone();
          let shutdown = self.shutdown.clone();
          tokio::spawn(async move {
            while updates.changed().await.is_ok() {
              if shutdown.load(Ordering::Relaxed) {
                break;
              }
              if let Err(error) =
                client.request::<req::SemanticTokensRefresh>(()).await
              {
                log(&client, error);
              }
            }
          });
        }
        Err(error) => self.log(error),
      }
    }
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
    if self.support.refresh && self.support.semantic_tokens.is_some() {
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
      .document_path(&file_path(&document.uri)?)
      .map_err(fault)?;
    if self.opened.contains_key(&path)
      || self.opened_path(&document.uri).is_some()
    {
      return Err(invalid("Document is already open"));
    }
    self
      .session
      .set_overlay(&path, Some(document.text))
      .map_err(fault)?;
    self.opened.insert(
      path,
      OpenDocument {
        uri: document.uri,
        version: document.version,
      },
    );
    Ok(())
  }

  fn change(
    &mut self,
    params: lsp::DidChangeTextDocumentParams,
  ) -> Result<(), ResponseError> {
    file_path(&params.text_document.uri)?;
    let Some((path, document)) = self
      .opened
      .iter_mut()
      .find(|(_, document)| document.uri == params.text_document.uri)
    else {
      return Err(invalid("Document is not open"));
    };
    if params.text_document.version <= document.version {
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
    self.session.set_overlay(path, Some(text)).map_err(fault)?;
    document.version = params.text_document.version;
    Ok(())
  }

  fn close(
    &mut self,
    params: &lsp::DidCloseTextDocumentParams,
  ) -> Result<(), ResponseError> {
    file_path(&params.text_document.uri)?;
    let path = self
      .opened_path(&params.text_document.uri)
      .cloned()
      .ok_or_else(|| invalid("Document is not open"))?;
    self.session.set_overlay(&path, None).map_err(fault)?;
    self.opened.remove(&path);
    Ok(())
  }

  fn folders(
    &mut self,
    params: &lsp::DidChangeWorkspaceFoldersParams,
  ) -> Result<(), ResponseError> {
    let event = &params.event;
    let removed =
      self.file_paths(event.removed.iter().map(|folder| &folder.uri));
    let added = self.file_paths(event.added.iter().map(|folder| &folder.uri));
    for path in removed {
      self.session.remove_input(&path).map_err(fault)?;
    }
    for path in added {
      self.session.add_input(&path).map_err(fault)?;
    }
    Ok(())
  }

  /// Notifications cannot reject individual URIs; log invalid ones.
  fn file_paths<'a>(
    &self,
    uris: impl Iterator<Item = &'a Url>,
  ) -> Vec<PathBuf> {
    uris
      .filter_map(|uri| file_path(uri).map_err(|error| self.log(error)).ok())
      .collect()
  }

  fn opened_path(&self, uri: &Url) -> Option<&PathBuf> {
    self
      .opened
      .iter()
      .find_map(|(path, document)| (document.uri == *uri).then_some(path))
  }

  /// Rejects URIs retargeted since the document was opened.
  fn document_path(&self, uri: &Url) -> Result<PathBuf, ResponseError> {
    let requested = file_path(uri)?;
    if let Some(path) = self.opened_path(uri)
      && self.session.absolute(&requested).map_err(fault)? != *path
    {
      return Err(fault(Error::Superseded));
    }
    Ok(requested)
  }

  fn document_query<T: Send + 'static>(
    &self,
    uri: &Url,
    scope: fn(PathBuf) -> RequestScope,
    operation: impl FnOnce(
      &mut Application,
      DocumentId,
      &AtomicBool,
    ) -> Result<T, Error>
    + Send
    + 'static,
  ) -> Reply<T> {
    let path = match self.document_path(uri) {
      Ok(path) => path,
      Err(error) => return Box::pin(std::future::ready(Err(error))),
    };
    self.query(scope(path.clone()), move |application, cancelled| {
      let id = application.document_id(&path).ok_or_else(|| {
        Error::Message(format!("Cannot load {}", path.display()))
      })?;
      operation(application, id, cancelled)
    })
  }

  /// Name requests address the document by path at a source offset.
  fn name_query<T: Send + 'static>(
    &self,
    position: &lsp::TextDocumentPositionParams,
    operation: impl FnOnce(
      &mut Application,
      &Path,
      usize,
      &AtomicBool,
    ) -> Result<T, Error>
    + Send
    + 'static,
  ) -> Reply<T> {
    let offset = position.position;
    self.document_query(
      &position.text_document.uri,
      RequestScope::Names,
      move |application, id, cancelled| {
        let offset = document_offset(application, id, offset)?;
        let path = application
          .path(id)
          .ok_or_else(|| Error::Message("Unknown document path".into()))?
          .to_path_buf();
        operation(application, &path, offset, cancelled)
      },
    )
  }

  fn query<T: Send + 'static>(
    &self,
    scope: RequestScope,
    operation: impl FnOnce(&mut Application, &AtomicBool) -> Result<T, Error>
    + Send
    + 'static,
  ) -> Reply<T> {
    let job = self.session.request(scope, operation);
    let client = self.client.clone();
    let session = self.session.clone();
    Box::pin(async move {
      let outcome = job.await;
      if outcome.result.is_ok()
        || matches!(outcome.result, Err(Error::Superseded))
      {
        let _ = session.start_index();
      }
      for failure in outcome.failures {
        log(
          &client,
          format!("{}: {}", failure.path.display(), failure.message),
        );
      }
      outcome.result.map_err(fault)
    })
  }

  fn semantic(
    &self,
    params: &lsp::SemanticTokensParams,
  ) -> Reply<Option<lsp::SemanticTokensResult>> {
    let Some(legend) = self.support.semantic_tokens.clone() else {
      return Box::pin(std::future::ready(Err(ResponseError::new(
        ErrorCode::METHOD_NOT_FOUND,
        "Semantic tokens were not negotiated",
      ))));
    };
    self.document_query(
      &params.text_document.uri,
      RequestScope::Analysis,
      move |application, id, cancelled| {
        let spans = application.classify(id, cancelled)?;
        Ok(Some(lsp::SemanticTokensResult::Tokens(
          lsp::SemanticTokens {
            result_id: None,
            data: encode(document_source(application, id)?, &spans, &legend),
          },
        )))
      },
    )
  }
  fn definitions(
    &self,
    params: &lsp::GotoDefinitionParams,
  ) -> Reply<Option<lsp::GotoDefinitionResponse>> {
    let position = params.text_document_position_params.position;
    let opened = self.opened.clone();
    self.document_query(
      &params.text_document_position_params.text_document.uri,
      RequestScope::Results,
      move |application, id, cancelled| {
        let offset = document_offset(application, id, position)?;
        let definitions = application.definitions(id, offset, cancelled)?;
        if definitions.is_empty() {
          return Ok(None);
        }
        locations(application, &opened, &definitions)
          .map(|locations| Some(lsp::GotoDefinitionResponse::Array(locations)))
      },
    )
  }

  fn references(
    &self,
    params: &lsp::ReferenceParams,
  ) -> Reply<Option<Vec<lsp::Location>>> {
    let include_declaration = params.context.include_declaration;
    let opened = self.opened.clone();
    self.name_query(
      &params.text_document_position,
      move |application, path, offset, cancelled| {
        let references = application.references(
          path,
          offset,
          include_declaration,
          cancelled,
        )?;
        if references.is_empty() {
          return Ok(None);
        }
        locations(application, &opened, &references).map(Some)
      },
    )
  }

  fn prepare_rename(
    &self,
    params: &lsp::TextDocumentPositionParams,
  ) -> Reply<Option<lsp::PrepareRenameResponse>> {
    self.name_query(params, |application, path, offset, cancelled| {
      let Some(target) = application.prepare_rename(path, offset, cancelled)?
      else {
        return Ok(None);
      };
      let range = Positions::new(application).result_range(&target.location)?;
      Ok(Some(lsp::PrepareRenameResponse::RangeWithPlaceholder {
        range,
        placeholder: target.placeholder,
      }))
    })
  }

  fn rename(
    &self,
    params: lsp::RenameParams,
  ) -> Reply<Option<lsp::WorkspaceEdit>> {
    let opened = self.opened.clone();
    let document_changes = self.support.document_changes;
    let new_name = params.new_name;
    self.name_query(
      &params.text_document_position,
      move |application, path, offset, cancelled| {
        let edits = application.rename(path, offset, &new_name, cancelled)?;
        workspace_edit(application, edits, &opened, document_changes).map(Some)
      },
    )
  }

  fn document_links(
    &self,
    params: &lsp::DocumentLinkParams,
  ) -> Reply<Option<Vec<lsp::DocumentLink>>> {
    let opened = self.opened.clone();
    self.document_query(
      &params.text_document.uri,
      RequestScope::Results,
      move |application, id, cancelled| {
        let links = application.document_links(id, cancelled)?;
        let mut positions = Positions::new(application);
        links
          .into_iter()
          .map(|(location, path)| {
            Ok(lsp::DocumentLink {
              range: positions.result_range(&location)?,
              target: Some(uri(application, &opened, &path)?),
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

pub(super) fn log(client: &ClientSocket, message: impl std::fmt::Display) {
  let _ = client.notify::<notif::LogMessage>(lsp::LogMessageParams {
    typ: lsp::MessageType::ERROR,
    message: message.to_string(),
  });
}

pub(super) fn router(
  session: Arc<Session>,
  client: ClientSocket,
  shutdown: Arc<AtomicBool>,
) -> Router<Server> {
  let mut router = Router::new(Server {
    session,
    client,
    opened: BTreeMap::new(),
    support: ClientSupport::default(),
    shutdown,
  });
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
    .request::<req::References, _>(|server, params| server.references(&params))
    .request::<req::PrepareRenameRequest, _>(|server, params| {
      server.prepare_rename(&params)
    })
    .request::<req::Rename, _>(|server, params| server.rename(params))
    .request::<req::DocumentLinkRequest, _>(|server, params| {
      server.document_links(&params)
    })
    .request::<req::DocumentSymbolRequest, _>(|server, params| {
      let format = server.support.symbols;
      let opened = server.opened.clone();
      server.document_query(
        &params.text_document.uri,
        RequestScope::Document,
        move |application, id, _| {
          document_symbols(application, &opened, id, format).map(Some)
        },
      )
    })
    .request::<req::WorkspaceSymbolRequest, _>(|server, params| {
      let opened = server.opened.clone();
      server.query(RequestScope::Workspace, move |application, _| {
        symbols(
          application,
          &opened,
          application.workspace_symbols(&params.query),
        )
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
      let paths =
        server.file_paths(params.changes.iter().map(|event| &event.uri));
      let result = server.session.files_changed(&paths).map_err(fault);
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
