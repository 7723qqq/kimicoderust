use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::future::FutureExt;
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::lsp::{LSP_SERVER_FAILED, LSP_UNSUPPORTED_OPERATION};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::configSection::LspServerConfig;
use super::lsp::{LspOperation, LspProviderQuery, LspQueryResult};
use super::lspConnection::{LspConnection, LspProtocolErrorHandler};
use super::translate::{normalize_hover_response, normalize_locations_response, path_to_uri};

pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_KILL_GRACE_MS: u64 = 2_000;
pub const DEFAULT_CANCEL_GRACE_MS: u64 = 2_000;
pub const DEFAULT_INITIALIZE_TIMEOUT_MS: u64 = 10_000;

pub struct LspInstance {
    connection: LspConnection,
    config: LspServerConfig,
    workspace_root: String,
    tail: Mutex<()>,
    disposed: Arc<AtomicBool>,
}

impl LspInstance {
    pub async fn create(
        config: LspServerConfig,
        workspace_root: String,
        signal: Option<CancellationToken>,
        on_protocol_error: Option<LspProtocolErrorHandler>,
    ) -> Result<Self, Error2> {
        let mut command = Command::new(&config.command);
        command
            .args(config.args.as_deref().unwrap_or(&[]))
            .current_dir(&workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(env) = &config.env {
            command.envs(env);
        }
        let child = command.spawn().map_err(|error| {
            Error2::new(LSP_SERVER_FAILED, format!("failed to start LSP server \"{}\": {error}", config.command))
        })?;
        let connection = LspConnection::from_child(child, Some(server_request_handler()), on_protocol_error)
            .map_err(|error| Error2::new(LSP_SERVER_FAILED, error.to_string()))?;
        let instance = Self {
            connection,
            config,
            workspace_root,
            tail: Mutex::new(()),
            disposed: Arc::new(AtomicBool::new(false)),
        };
        if let Err(error) = instance.initialize(signal).await {
            instance
                .connection
                .kill_ladder(Duration::from_millis(instance.config.kill_grace_ms.unwrap_or(DEFAULT_KILL_GRACE_MS)))
                .await;
            return Err(error);
        }
        Ok(instance)
    }

    pub fn is_closed(&self) -> bool {
        self.disposed.load(Ordering::Acquire)
    }

    pub async fn query(
        &self,
        request: LspProviderQuery,
        signal: Option<CancellationToken>,
    ) -> Result<LspQueryResult, Error2> {
        let _guard = self.tail.lock().await;
        self.do_query(request, signal).await
    }

    pub async fn dispose(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        let timeout = Duration::from_millis(
            self.config.shutdown_timeout_ms.unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_MS),
        );
        if let Ok(Ok(_)) = tokio::time::timeout(
            timeout,
            self.connection.request("shutdown", None, None),
        )
        .await
        {
            let _ = self.connection.notify("exit", None).await;
        }
        self.connection.dispose();
        self.connection
            .kill_ladder(Duration::from_millis(
                self.config.kill_grace_ms.unwrap_or(DEFAULT_KILL_GRACE_MS),
            ))
            .await;
    }

    async fn initialize(&self, signal: Option<CancellationToken>) -> Result<(), Error2> {
        let timeout_ms = self.config.initialize_timeout_ms.unwrap_or(DEFAULT_INITIALIZE_TIMEOUT_MS);
        let root_uri = path_to_uri(&self.workspace_root);
        let workspace_name = Path::new(&self.workspace_root)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut params = json!({
            "processId": null,
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": workspace_name }],
            "capabilities": {
                "textDocument": { "hover": { "contentFormat": ["plaintext", "markdown"] } },
                "workspace": { "configuration": true }
            }
        });
        if let Some(options) = &self.config.initialization_options {
            params["initializationOptions"] = options.clone();
        }
        let response = match tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            self.connection.request("initialize", Some(params), signal),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                self.connection.dispose();
                return Err(Error2::new(LSP_SERVER_FAILED, error.to_string()));
            }
            Err(_) => {
                self.connection.dispose();
                return Err(Error2::new(
                    LSP_SERVER_FAILED,
                    format!(
                        "LSP server \"{}\" did not answer initialize within {timeout_ms}ms",
                        self.config.command
                    ),
                ));
            }
        };
        let result: super::protocol::LspInitializeResult = serde_json::from_value(response)
            .map_err(|error| Error2::new(LSP_SERVER_FAILED, error.to_string()))?;
        if let Some(encoding) = result.capabilities.position_encoding
            && encoding != "utf-16"
        {
            return Err(Error2::new(
                LSP_UNSUPPORTED_OPERATION,
                format!(
                    "LSP server \"{}\" uses position encoding \"{encoding}\"; only utf-16 is supported",
                    self.config.command
                ),
            ));
        }
        self.connection
            .notify("initialized", None)
            .await
            .map_err(|error| Error2::new(LSP_SERVER_FAILED, error.to_string()))
    }

    async fn do_query(
        &self,
        request: LspProviderQuery,
        signal: Option<CancellationToken>,
    ) -> Result<LspQueryResult, Error2> {
        let uri = path_to_uri(&request.file_path);
        let content = tokio::fs::read_to_string(&request.file_path)
            .await
            .map_err(|error| Error2::new(LSP_SERVER_FAILED, error.to_string()))?;
        self.connection
            .notify(
                "textDocument/didOpen",
                Some(json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": request.language_id,
                        "version": 1,
                        "text": content,
                    }
                })),
            )
            .await
            .map_err(|error| Error2::new(LSP_SERVER_FAILED, error.to_string()))?;
        let cancel_task = signal.as_ref().map(|signal| {
            let connection = self.connection.clone();
            let disposed = self.disposed.clone();
            let grace = self.config.cancel_grace_ms.unwrap_or(DEFAULT_CANCEL_GRACE_MS);
            let shutdown_timeout = self
                .config
                .shutdown_timeout_ms
                .unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_MS);
            tokio::spawn(signal.clone().cancelled_owned().then(move |_| async move {
                tokio::time::sleep(Duration::from_millis(grace)).await;
                if !disposed.swap(true, Ordering::AcqRel) {
                    if let Ok(Ok(_)) = tokio::time::timeout(
                        Duration::from_millis(shutdown_timeout),
                        connection.request("shutdown", None, None),
                    )
                    .await
                    {
                        let _ = connection.notify("exit", None).await;
                    }
                    connection.dispose();
                    connection.kill_ladder(Duration::from_millis(grace)).await;
                }
            }))
        });
        let params = json!({
            "textDocument": { "uri": uri },
            "position": request.position,
        });
        let response = self.request_operation(&request, params, signal).await;
        if let Some(task) = cancel_task {
            task.abort();
        }
        let _ = self
            .connection
            .notify(
                "textDocument/didClose",
                Some(json!({ "textDocument": { "uri": uri } })),
            )
            .await;
        translate_response(&request, response?)
    }

    async fn request_operation(
        &self,
        request: &LspProviderQuery,
        params: Value,
        signal: Option<CancellationToken>,
    ) -> Result<Value, Error2> {
        let (method, params) = match request.operation {
            LspOperation::GoToDefinition => ("textDocument/definition", params),
            LspOperation::FindReferences => {
                let mut params = params;
                params["context"] = json!({ "includeDeclaration": true });
                ("textDocument/references", params)
            }
            LspOperation::GoToImplementation => ("textDocument/implementation", params),
            LspOperation::Hover => ("textDocument/hover", params),
        };
        self.connection
            .request(method, Some(params), signal)
            .await
            .map_err(|error| Error2::new(LSP_SERVER_FAILED, error.to_string()))
    }
}

fn translate_response(request: &LspProviderQuery, response: Value) -> Result<LspQueryResult, Error2> {
    match request.operation {
        LspOperation::GoToDefinition | LspOperation::FindReferences | LspOperation::GoToImplementation => {
            Ok(LspQueryResult::Locations {
                locations: normalize_locations_response(&response)
                    .map_err(|error| Error2::new(LSP_SERVER_FAILED, error))?,
            })
        }
        LspOperation::Hover => Ok(LspQueryResult::Hover {
            hover: normalize_hover_response(&response).map_err(|error| Error2::new(LSP_SERVER_FAILED, error))?,
        }),
    }
}

fn server_request_handler() -> super::lspConnection::LspServerRequestHandler {
    Arc::new(|method, params| {
        async move {
            match method.as_str() {
                "workspace/configuration" => Ok(Value::Array(
                    params
                        .get("items")
                        .and_then(Value::as_array)
                        .map(|items| vec![Value::Null; items.len()])
                        .unwrap_or_default(),
                )),
                "workspace/applyEdit" => Err("workspace/applyEdit is not supported".to_string()),
                _ => Ok(Value::Null),
            }
        }
        .boxed()
    })
}
