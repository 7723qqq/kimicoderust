use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::native::lsp::{LspFraming, path_to_uri};
use crate::native::shell_path_bridge::ShellPathBridge;
use crate::turn_loop::types::ExecutableToolResult;

const MAX_LOCATIONS: usize = 100;

const MAX_RESULT_CHARS: usize = 16_000;

const DEFAULT_INITIALIZE_TIMEOUT_MS: u64 = 10_000;

const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 5_000;

const DEFAULT_KILL_GRACE_MS: u64 = 2_000;

const DEFAULT_CANCEL_GRACE_MS: u64 = 2_000;

const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

const LSP_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_LSP";

pub fn lsp_enabled() -> bool {
    crate::env::env_switch_default_off(LSP_FLAG_ENV)
}

const OPERATIONS: &[&str] = &[
    "goToDefinition",
    "findReferences",
    "goToImplementation",
    "hover",
];

#[derive(Debug, Clone)]
struct LspServerConfig {
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    extension_to_language: Vec<(String, String)>,
    initialization_options: Option<Value>,
    initialize_timeout_ms: u64,
    shutdown_timeout_ms: u64,
    kill_grace_ms: u64,
    cancel_grace_ms: u64,
}

impl LspServerConfig {
    fn language_for(&self, extension: &str) -> Option<&str> {
        self.extension_to_language
            .iter()
            .find(|(ext, _)| ext == extension)
            .map(|(_, language)| language.as_str())
    }
}

fn load_servers() -> Vec<LspServerConfig> {
    let Ok((_config, path)) = crate::config::KimiConfig::discover() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = text.parse::<toml::Value>() else {
        return Vec::new();
    };
    let Some(servers) = value
        .get("lsp")
        .and_then(|lsp| lsp.get("servers"))
        .and_then(toml::Value::as_table)
    else {
        return Vec::new();
    };

    servers.values().filter_map(parse_server).collect()
}

fn parse_server(value: &toml::Value) -> Option<LspServerConfig> {
    let table = value.as_table()?;
    let command = table.get("command")?.as_str()?.to_string();

    let args = table
        .get("args")
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default();

    let env = table
        .get("env")
        .and_then(toml::Value::as_table)
        .map(|items| {
            items
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();

    let extension_to_language = table
        .get("extensionToLanguage")
        .and_then(toml::Value::as_table)
        .map(|items| {
            items
                .iter()
                .filter_map(|(ext, language)| {
                    language
                        .as_str()
                        .map(|language| (ext.to_ascii_lowercase(), language.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();

    Some(LspServerConfig {
        command,
        args,
        env,
        extension_to_language,
        initialization_options: table
            .get("initializationOptions")
            .and_then(|options| serde_json::to_value(options).ok()),
        initialize_timeout_ms: table
            .get("initializeTimeoutMs")
            .and_then(toml::Value::as_integer)
            .filter(|ms| *ms >= 0)
            .map_or(DEFAULT_INITIALIZE_TIMEOUT_MS, |ms| ms as u64),
        shutdown_timeout_ms: table
            .get("shutdownTimeoutMs")
            .and_then(toml::Value::as_integer)
            .filter(|ms| *ms >= 0)
            .map_or(DEFAULT_SHUTDOWN_TIMEOUT_MS, |ms| ms as u64),
        kill_grace_ms: table
            .get("killGraceMs")
            .and_then(toml::Value::as_integer)
            .filter(|ms| *ms >= 0)
            .map_or(DEFAULT_KILL_GRACE_MS, |ms| ms as u64),
        cancel_grace_ms: table
            .get("cancelGraceMs")
            .and_then(toml::Value::as_integer)
            .filter(|ms| *ms >= 0)
            .map_or(DEFAULT_CANCEL_GRACE_MS, |ms| ms as u64),
    })
}

fn final_extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn err(content: impl Into<String>) -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: content.into(),
        is_error: true,
        note: None,
        display: None,
    }
}

fn ok(content: impl Into<String>) -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: content.into(),
        is_error: false,
        note: None,
        display: None,
    }
}

pub async fn execute_lsp_tool(
    workspace_root: &Path,
    bridge: &ShellPathBridge,
    args: &Value,
) -> Option<ExecutableToolResult> {
    if !lsp_enabled() {
        return Some(err(format!(
            "The lsp tool is disabled. Set {LSP_FLAG_ENV}=1 to enable it."
        )));
    }
    execute_lsp_tool_enabled(workspace_root, bridge, args).await
}

async fn execute_lsp_tool_enabled(
    workspace_root: &Path,
    bridge: &ShellPathBridge,
    args: &Value,
) -> Option<ExecutableToolResult> {
    let operation = match args.get("operation").and_then(Value::as_str) {
        Some(operation) => operation,
        None => return Some(err("Missing required parameter 'operation'")),
    };
    if !OPERATIONS.contains(&operation) {
        return Some(err(format!(
            "Invalid enum value. Expected 'goToDefinition' | 'findReferences' | 'goToImplementation' | 'hover', received '{operation}'"
        )));
    }

    let raw_path = match args.get("file_path").and_then(Value::as_str) {
        Some(path) => path,
        None => return Some(err("Missing required parameter 'file_path'")),
    };
    let line = match positive_position(args.get("line"), "line") {
        Ok(value) => value,
        Err(message) => return Some(err(message)),
    };
    let character = match positive_position(args.get("character"), "character") {
        Ok(value) => value,
        Err(message) => return Some(err(message)),
    };

    let target_path = super::NativeToolset::candidate_path(workspace_root, bridge, raw_path);

    if !target_path.exists() {
        return Some(err(format!(
            "Target file does not exist: {}",
            target_path.display()
        )));
    }
    if !within_workspace(workspace_root, &target_path) {
        return Some(err(format!(
            "{raw_path} is outside the session workspace; the lsp tool only queries files inside it."
        )));
    }

    let extension = final_extension(&target_path);
    let servers = load_servers();
    let (server, language_id) = match servers.iter().find_map(|server| {
        server
            .language_for(&extension)
            .map(|language| (server, language))
    }) {
        Some(found) => found,
        None => {
            return Some(err(format!(
                "no LSP provider is configured for extension \"{extension}\" (file: {raw_path}). \
                 Configure one in the [lsp] config section, e.g. `servers.typescript = \
                 {{ command = \"typescript-language-server\", args = [\"--stdio\"], \
                 extensionToLanguage = {{ ts = \"typescript\" }} }}`."
            )));
        }
    };

    let file_text = match std::fs::read_to_string(&target_path) {
        Ok(text) => text,
        Err(error) => {
            return Some(err(format!(
                "Failed to read target file {}: {error}",
                target_path.display()
            )));
        }
    };

    execute_query(
        workspace_root,
        &target_path,
        language_id,
        &file_text,
        operation,
        line - 1,
        character - 1,
        server,
    )
    .await
}

struct LspQuery<'a> {
    operation: &'a str,
    target_path: &'a Path,
    language_id: &'a str,
    file_text: &'a str,
    line: u64,
    character: u64,
}

struct QueryError {
    message: String,
    transport_closed: bool,
    timed_out: bool,
}

impl QueryError {
    fn protocol(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transport_closed: false,
            timed_out: false,
        }
    }

    fn transport(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transport_closed: true,
            timed_out: false,
        }
    }

    fn timed_out(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transport_closed: false,
            timed_out: true,
        }
    }
}

static INSTANCES: LazyLock<tokio::sync::Mutex<HashMap<String, LspInstance>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

struct LspInstance {
    session: LspSession,
    config: LspServerConfig,
}

impl LspInstance {
    async fn dispose(self, grace: Duration) {
        let LspInstance {
            mut session,
            config,
        } = self;
        let shutdown_timeout = Duration::from_millis(config.shutdown_timeout_ms);
        if let Ok(id) = session.send_request("shutdown", Value::Null).await {
            let _ = session.wait_response(id, shutdown_timeout).await;
        }
        let _ = session.send_notification("exit", json!({})).await;
        session.kill(grace).await;
    }
}

struct LspSession {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    buffer: Vec<u8>,
    next_id: i64,
}

impl LspSession {
    fn spawn(server: &LspServerConfig, workspace_root: &Path) -> Result<Self, String> {
        let mut command = Command::new(&server.command);
        command
            .args(&server.args)
            .current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in &server.env {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|error| {
            format!(
                "Failed to start language server '{}': {error}",
                server.command
            )
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            "Failed to initialize LSP stdio pipes: missing child stdin".to_string()
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            "Failed to initialize LSP stdio pipes: missing child stdout".to_string()
        })?;
        Ok(Self {
            child,
            stdin,
            stdout,
            buffer: Vec::new(),
            next_id: 1,
        })
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    async fn send_payload(&mut self, payload: &Value) -> Result<(), String> {
        let frame = LspFraming::encode(payload)
            .map_err(|error| format!("Failed to encode LSP frame: {error}"))?;
        self.stdin
            .write_all(&frame)
            .await
            .map_err(|error| format!("Failed to write to language server: {error}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| format!("Failed to flush to language server: {error}"))
    }

    async fn send_request(&mut self, method: &str, params: Value) -> Result<i64, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_payload(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        Ok(id)
    }

    async fn send_notification(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send_payload(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn read_message(&mut self) -> Result<Option<Value>, String> {
        if let Some(message) = LspFraming::decode(&mut self.buffer)? {
            return Ok(Some(message));
        }
        let mut chunk = [0u8; 4096];
        loop {
            let read = self
                .stdout
                .read(&mut chunk)
                .await
                .map_err(|error| format!("Failed to read from language server: {error}"))?;
            if read == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&chunk[..read]);
            if let Some(message) = LspFraming::decode(&mut self.buffer)? {
                return Ok(Some(message));
            }
        }
    }

    async fn answer_server_request(&mut self, message: &Value) -> Result<(), String> {
        let Some(id) = message.get("id") else {
            return Ok(());
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match method {
            "workspace/applyEdit" => {
                self.send_payload(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": "workspace/applyEdit is not supported",
                    },
                }))
                .await
            }
            "workspace/configuration" => {
                let count = message
                    .get("params")
                    .and_then(|params| params.get("items"))
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                self.send_payload(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": vec![Value::Null; count],
                }))
                .await
            }
            _ => {
                self.send_payload(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": Value::Null,
                }))
                .await
            }
        }
    }

    async fn wait_response(
        &mut self,
        target_id: i64,
        timeout: Duration,
    ) -> Result<Value, QueryError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, self.read_message()).await {
                Ok(Ok(Some(message))) => {
                    let is_response =
                        message.get("method").is_none() && message.get("id").is_some();
                    if is_response {
                        if message.get("id").and_then(Value::as_i64) == Some(target_id) {
                            return Ok(message);
                        }

                        continue;
                    }
                    self.answer_server_request(&message)
                        .await
                        .map_err(QueryError::transport)?;
                }
                Ok(Ok(None)) => {
                    return Err(QueryError::transport("LSP transport closed"));
                }
                Ok(Err(error)) => return Err(QueryError::transport(error)),
                Err(_) => break,
            }
        }
        Err(QueryError::timed_out(format!(
            "Timeout waiting for LSP response ID {target_id}"
        )))
    }

    async fn kill(mut self, grace: Duration) {
        let _ = self.stdin.shutdown().await;
        if tokio::time::timeout(grace, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
    }
}

fn instance_key(server: &LspServerConfig, workspace_root: &Path) -> String {
    let root =
        std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    format!("{}|{}", server.command, root.display())
}

async fn create_instance(
    server: &LspServerConfig,
    workspace_root: &Path,
) -> Result<LspInstance, String> {
    let mut session = LspSession::spawn(server, workspace_root)?;
    if let Err(error) = initialize_session(&mut session, server, workspace_root).await {
        session
            .kill(Duration::from_millis(server.kill_grace_ms))
            .await;
        return Err(error);
    }
    Ok(LspInstance {
        session,
        config: server.clone(),
    })
}

async fn initialize_session(
    session: &mut LspSession,
    server: &LspServerConfig,
    workspace_root: &Path,
) -> Result<(), String> {
    let root_uri = path_to_uri(workspace_root);
    let workspace_name = workspace_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let mut params = json!({
        "processId": null,
        "rootUri": root_uri,
        "workspaceFolders": [{ "uri": root_uri, "name": workspace_name }],
        "capabilities": {
            "textDocument": { "hover": { "contentFormat": ["plaintext", "markdown"] } },
            "workspace": { "configuration": true }
        }
    });
    if let Some(options) = &server.initialization_options {
        params["initializationOptions"] = options.clone();
    }

    let timeout = Duration::from_millis(server.initialize_timeout_ms);
    let id = session
        .send_request("initialize", params)
        .await
        .map_err(|error| format!("Failed to send initialize request: {error}"))?;
    let response = session.wait_response(id, timeout).await.map_err(|error| {
        format!(
            "LSP server \"{}\" did not answer initialize within {}ms: {}",
            server.command, server.initialize_timeout_ms, error.message
        )
    })?;
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown LSP error");
        return Err(format!("LSP initialize failed: {message}"));
    }

    let position_encoding = response
        .get("result")
        .and_then(|result| result.get("capabilities"))
        .and_then(|capabilities| capabilities.get("positionEncoding"))
        .and_then(Value::as_str);
    if position_encoding.is_some_and(|encoding| encoding != "utf-16") {
        return Err(format!(
            "LSP server \"{}\" uses position encoding \"{}\"; only utf-16 is supported",
            server.command,
            position_encoding.unwrap_or_default()
        ));
    }

    session
        .send_notification("initialized", json!({}))
        .await
        .map_err(|error| format!("Failed to send initialized notification: {error}"))
}

async fn execute_query(
    workspace_root: &Path,
    target_path: &Path,
    language_id: &str,
    file_text: &str,
    operation: &str,
    line: u64,
    character: u64,
    server: &LspServerConfig,
) -> Option<ExecutableToolResult> {
    let request = LspQuery {
        operation,
        target_path,
        language_id,
        file_text,
        line,
        character,
    };
    let key = instance_key(server, workspace_root);
    let mut instances = INSTANCES.lock().await;
    let mut retried = false;
    loop {
        let mut instance = match instances.remove(&key) {
            Some(instance) => instance,
            None => match create_instance(server, workspace_root).await {
                Ok(instance) => instance,
                Err(error) => return Some(err(error)),
            },
        };
        match query_instance(&mut instance, &request).await {
            Ok(rendered) => {
                instances.insert(key, instance);
                return Some(ok(rendered));
            }
            Err(error) => {
                if !error.transport_closed && !error.timed_out {
                    instances.insert(key, instance);
                    return Some(err(format!("LSP {operation} failed: {}", error.message)));
                }
                let retry = error.transport_closed && !retried;
                let grace = if error.timed_out {
                    server.cancel_grace_ms
                } else {
                    server.kill_grace_ms
                };
                instance.dispose(Duration::from_millis(grace)).await;
                if retry {
                    retried = true;
                    continue;
                }
                return Some(err(format!("LSP {operation} failed: {}", error.message)));
            }
        }
    }
}

async fn query_instance(
    instance: &mut LspInstance,
    request: &LspQuery<'_>,
) -> Result<String, QueryError> {
    if !instance.session.is_running() {
        return Err(QueryError::transport("LSP transport closed"));
    }
    let uri = path_to_uri(request.target_path);
    instance
        .session
        .send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": request.language_id,
                    "version": 1,
                    "text": request.file_text,
                }
            }),
        )
        .await
        .map_err(QueryError::transport)?;

    let result = request_operation(&mut instance.session, request, &uri).await;
    let _ = instance
        .session
        .send_notification(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await;
    result
}

async fn request_operation(
    session: &mut LspSession,
    request: &LspQuery<'_>,
    uri: &str,
) -> Result<String, QueryError> {
    let position = json!({ "line": request.line, "character": request.character });
    let (method, params) = match request.operation {
        "goToDefinition" => (
            "textDocument/definition",
            json!({ "textDocument": { "uri": uri }, "position": position }),
        ),
        "findReferences" => (
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": position,
                "context": { "includeDeclaration": true }
            }),
        ),
        "goToImplementation" => (
            "textDocument/implementation",
            json!({ "textDocument": { "uri": uri }, "position": position }),
        ),
        "hover" => (
            "textDocument/hover",
            json!({ "textDocument": { "uri": uri }, "position": position }),
        ),
        other => {
            return Err(QueryError::protocol(format!(
                "Unsupported LSP operation: '{other}'"
            )));
        }
    };
    let id = session
        .send_request(method, params)
        .await
        .map_err(QueryError::transport)?;
    let response = session.wait_response(id, QUERY_TIMEOUT).await?;
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown LSP error");
        return Err(QueryError::protocol(message.to_string()));
    }
    Ok(render_result(
        request.operation,
        response.get("result").unwrap_or(&Value::Null),
    ))
}

fn positive_position(value: Option<&Value>, name: &str) -> Result<u64, String> {
    let value = value
        .ok_or_else(|| format!("Missing required parameter '{name}'"))?
        .as_i64()
        .ok_or_else(|| format!("\"{name}\" must be an integer"))?;
    if value < 1 {
        return Err(format!("\"{name}\" must be greater than or equal to 1"));
    }
    Ok(value as u64)
}

fn within_workspace(root: &Path, target: &Path) -> bool {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let target = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    target.starts_with(&root)
}

fn render_result(operation: &str, result: &Value) -> String {
    if operation == "hover" {
        return render_hover(result);
    }
    match normalize_locations(result) {
        Ok(locations) => render_locations(&locations),
        Err(message) => message,
    }
}

struct Location {
    path: String,
    line: u64,
    character: u64,
}

fn normalize_locations(response: &Value) -> Result<Vec<Location>, String> {
    if response.is_null() {
        return Ok(Vec::new());
    }
    let entries: Vec<&Value> = match response {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    entries
        .into_iter()
        .map(|entry| {
            let (uri, range) = match (entry.get("targetUri"), entry.get("targetRange")) {
                (Some(uri), Some(range)) => (uri, range),
                _ => match (entry.get("uri"), entry.get("range")) {
                    (Some(uri), Some(range)) => (uri, range),
                    _ => {
                        return Err(format!("malformed LSP location response: {entry}"));
                    }
                },
            };
            let uri = uri
                .as_str()
                .ok_or_else(|| format!("malformed LSP location response: {entry}"))?;
            let start = range
                .get("start")
                .ok_or_else(|| format!("malformed LSP location response: {entry}"))?;
            let line = start
                .get("line")
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("malformed LSP location response: {entry}"))?;
            let character = start
                .get("character")
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("malformed LSP location response: {entry}"))?;
            Ok(Location {
                path: uri_to_path(uri),
                line: line + 1,
                character: character + 1,
            })
        })
        .collect()
}

fn render_locations(locations: &[Location]) -> String {
    if locations.is_empty() {
        return "No locations found.".to_string();
    }
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for location in locations.iter().take(MAX_LOCATIONS) {
        let entry = format!("{}:{}:{}", location.path, location.line, location.character);
        match groups.iter_mut().find(|(path, _)| path == &location.path) {
            Some((_, entries)) => entries.push(entry),
            None => groups.push((location.path.clone(), vec![entry])),
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for (path, entries) in groups {
        lines.push(format!("{path}:"));
        for entry in entries {
            lines.push(format!("  {entry}"));
        }
    }
    if locations.len() > MAX_LOCATIONS {
        lines.push(format!(
            "… {} more locations omitted",
            locations.len() - MAX_LOCATIONS
        ));
    }
    truncate(&lines.join("\n"))
}

fn render_hover(result: &Value) -> String {
    if result.is_null() {
        return "No hover information available at this position.".to_string();
    }
    truncate(&hover_text(result.get("contents").unwrap_or(&Value::Null)))
}

fn hover_text(contents: &Value) -> String {
    match contents {
        Value::String(text) => text.clone(),
        Value::Array(entries) => entries
            .iter()
            .map(hover_text)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => contents
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn truncate(text: &str) -> String {
    let length = text.chars().count();
    if length <= MAX_RESULT_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_RESULT_CHARS).collect();
    format!(
        "{kept}\n… output truncated ({} more characters)",
        length - MAX_RESULT_CHARS
    )
}

fn uri_to_path(uri: &str) -> String {
    if !uri.starts_with("file://") {
        return uri.to_string();
    }
    url::Url::parse(uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| uri.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn bridge() -> ShellPathBridge {
        ShellPathBridge::new(&crate::native::shell::resolve_shell(None).program)
    }

    fn local_path(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn test_final_extension() {
        assert_eq!(final_extension(Path::new("src/main.rs")), "rs");
        assert_eq!(final_extension(Path::new("src/index.TS")), "ts");
        assert_eq!(final_extension(Path::new("README")), "");
    }

    #[tokio::test]
    async fn test_lsp_tool_missing_params() {
        let dir = tempfile::Builder::new()
            .prefix("kimi-lsp-test")
            .tempdir()
            .unwrap();
        let root = dir.path().to_path_buf();
        let res1 = execute_lsp_tool_enabled(&root, &bridge(), &json!({}))
            .await
            .unwrap();
        assert!(res1.is_error);
        assert!(
            res1.content
                .contains("Missing required parameter 'operation'")
        );

        let res2 =
            execute_lsp_tool_enabled(&root, &bridge(), &json!({ "operation": "goToDefinition" }))
                .await
                .unwrap();
        assert!(res2.is_error);
        assert!(
            res2.content
                .contains("Missing required parameter 'file_path'")
        );
    }

    #[tokio::test]
    async fn test_lsp_tool_rejects_zero_based_positions() {
        let dir = tempfile::Builder::new()
            .prefix("kimi-lsp-test")
            .tempdir()
            .unwrap();
        let root = dir.path().to_path_buf();
        let res = execute_lsp_tool_enabled(
            &root,
            &bridge(),
            &json!({
                "operation": "goToDefinition",
                "file_path": "src/main.rs",
                "line": 0,
                "character": 1
            }),
        )
        .await
        .unwrap();
        assert!(res.is_error);
        assert!(
            res.content
                .contains("\"line\" must be greater than or equal to 1")
        );
    }

    #[tokio::test]
    async fn test_lsp_tool_nonexistent_file() {
        let dir = tempfile::Builder::new()
            .prefix("kimi-lsp-test")
            .tempdir()
            .unwrap();
        let root = dir.path().to_path_buf();
        let res = execute_lsp_tool_enabled(
            &root,
            &bridge(),
            &json!({
                "operation": "goToDefinition",
                "file_path": "non_existent_file.rs",
                "line": 1,
                "character": 1
            }),
        )
        .await
        .unwrap();
        assert!(res.is_error);
        assert!(res.content.contains("Target file does not exist"));
    }

    #[tokio::test]
    async fn test_lsp_tool_unsupported_operation() {
        let dir = tempfile::Builder::new()
            .prefix("kimi-lsp-test")
            .tempdir()
            .unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "pub fn placeholder() {}\n").unwrap();
        let res = execute_lsp_tool_enabled(
            dir.path(),
            &bridge(),
            &json!({
                "operation": "documentSymbols",
                "file_path": local_path(&file),
                "line": 1,
                "character": 1
            }),
        )
        .await
        .unwrap();
        assert!(res.is_error);
        assert!(res.content.contains("Invalid enum value"));
    }

    #[tokio::test]
    async fn test_lsp_tool_rejects_an_unsupported_file_extension() {
        let dir = tempfile::Builder::new()
            .prefix("kimi-lsp-extension")
            .tempdir()
            .unwrap();
        let file = dir.path().join("notes.unknownext");
        std::fs::write(&file, "not code\n").unwrap();
        let res = execute_lsp_tool_enabled(
            dir.path(),
            &bridge(),
            &json!({
                "operation": "goToDefinition",
                "file_path": local_path(&file),
                "line": 1,
                "character": 1
            }),
        )
        .await
        .unwrap();
        assert!(res.is_error);
        assert!(
            res.content
                .contains("no LSP provider is configured for extension \"unknownext\""),
            "expected the unsupported-extension diagnostic, got: {}",
            res.content
        );
        assert!(
            !res.content.contains("Unsupported LSP operation"),
            "the extension failure must not be reported as a generic operation error: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn test_lsp_tool_rejects_paths_outside_the_workspace() {
        let root = tempfile::Builder::new()
            .prefix("kimi-lsp-root")
            .tempdir()
            .unwrap();
        let outside = tempfile::Builder::new()
            .prefix("kimi-lsp-outside")
            .tempdir()
            .unwrap();
        let file = outside.path().join("lib.rs");
        std::fs::write(&file, "pub fn placeholder() {}\n").unwrap();
        let res = execute_lsp_tool_enabled(
            root.path(),
            &bridge(),
            &json!({
                "operation": "goToDefinition",
                "file_path": local_path(&file),
                "line": 1,
                "character": 1
            }),
        )
        .await
        .unwrap();
        assert!(res.is_error);
        assert!(res.content.contains("outside the session workspace"));
    }

    #[test]
    fn test_render_locations_groups_by_file_and_caps_at_100() {
        let locations: Vec<Location> = (0..MAX_LOCATIONS + 5)
            .map(|index| Location {
                path: if index % 2 == 0 {
                    "src/a.rs".to_string()
                } else {
                    "src/b.rs".to_string()
                },
                line: index as u64 + 1,
                character: 1,
            })
            .collect();
        let rendered = render_locations(&locations);
        assert!(rendered.starts_with("src/a.rs:\n  src/a.rs:1:1"));
        assert!(rendered.contains("src/b.rs:\n  src/b.rs:2:1"));
        assert!(rendered.contains("… 5 more locations omitted"));
    }

    #[test]
    fn test_render_locations_empty() {
        assert_eq!(render_locations(&[]), "No locations found.");
    }

    #[test]
    fn test_render_hover_text() {
        assert_eq!(
            render_hover(&json!({ "contents": { "kind": "markdown", "value": "fn main()" } })),
            "fn main()"
        );
        assert_eq!(
            render_hover(&json!({ "contents": ["one", { "language": "rust", "value": "two" }] })),
            "one\ntwo"
        );
        assert_eq!(
            render_hover(&Value::Null),
            "No hover information available at this position."
        );
    }

    #[test]
    fn test_normalize_locations_accepts_location_links() {
        let locations = normalize_locations(&json!({
            "targetUri": "file:///tmp/main.rs",
            "targetRange": { "start": { "line": 4, "character": 2 }, "end": { "line": 4, "character": 6 } }
        }))
        .unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].line, 5);
        assert_eq!(locations[0].character, 3);
    }

    #[test]
    fn test_truncate_caps_at_16000_characters() {
        let text = "a".repeat(MAX_RESULT_CHARS + 10);
        let truncated = truncate(&text);
        assert!(truncated.contains("… output truncated (10 more characters)"));
        assert_eq!(
            truncated.chars().take(MAX_RESULT_CHARS).count(),
            MAX_RESULT_CHARS
        );
    }

    #[test]
    fn test_parse_server_reads_config_shape() {
        let value: toml::Value = r#"
command = "typescript-language-server"
args = ["--stdio"]
extensionToLanguage = { ts = "typescript" }
initializeTimeoutMs = 1234
killGraceMs = 250
cancelGraceMs = 300
"#
        .parse()
        .unwrap();
        let server = parse_server(&value).unwrap();
        assert_eq!(server.command, "typescript-language-server");
        assert_eq!(server.args, vec!["--stdio"]);
        assert_eq!(server.language_for("ts"), Some("typescript"));
        assert_eq!(server.initialize_timeout_ms, 1234);
        assert_eq!(server.kill_grace_ms, 250);
        assert_eq!(server.cancel_grace_ms, 300);
    }

    #[test]
    fn test_parse_server_uses_v2_timeout_defaults() {
        let value: toml::Value = r#"
command = "rust-analyzer"
extensionToLanguage = { rs = "rust" }
"#
        .parse()
        .unwrap();
        let server = parse_server(&value).unwrap();
        assert_eq!(server.initialize_timeout_ms, DEFAULT_INITIALIZE_TIMEOUT_MS);
        assert_eq!(server.shutdown_timeout_ms, DEFAULT_SHUTDOWN_TIMEOUT_MS);
        assert_eq!(server.kill_grace_ms, DEFAULT_KILL_GRACE_MS);
        assert_eq!(server.cancel_grace_ms, DEFAULT_CANCEL_GRACE_MS);
    }
}
