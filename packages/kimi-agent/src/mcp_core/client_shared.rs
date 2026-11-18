use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{FutureExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::utils::abort::AbortSignal;
use crate::_base::utils::fetch_like::{FetchInit, FetchLike, FetchResponse};
use crate::_base::version::get_core_version;
use crate::remote::protocol::errors::{ConnectionClosedError, RequestTimeoutError, RpcError};
use kimi_core::primitives::errors::Error2;
use crate::wire::record::WireAsyncError;

use super::types::{MCPContentBlock, MCPToolDefinition, MCPToolResult};

pub const KIMI_MCP_CLIENT_NAME: &str = "kimi-code";
pub static KIMI_MCP_CLIENT_VERSION: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| get_core_version().to_string());
pub const MCP_LIVENESS_PROBE_TIMEOUT_MS: u64 = 5_000;
pub const MCP_ERROR_METHOD_NOT_FOUND: i64 = -32601;
pub const MCP_ERROR_CONNECTION_CLOSED: i64 = -32000;
pub const MCP_ERROR_REQUEST_TIMEOUT: i64 = -32001;

#[derive(Debug, Clone, PartialEq)]
pub struct McpSdkError {
    pub name: String,
    pub code: Option<i64>,
    pub message: String,
    pub data: Option<Value>,
}

impl McpSdkError {
    pub fn new(name: &str, code: Option<i64>, message: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl fmt::Display for McpSdkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for McpSdkError {}

#[derive(Debug)]
pub struct UnexpectedCloseReason {
    pub error: Option<WireAsyncError>,
    pub stderr: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvidenceKind {
    Hard,
    Weak,
}

#[derive(Debug)]
pub struct SessionEvidence {
    pub kind: SessionEvidenceKind,
    pub reason: UnexpectedCloseReason,
}

pub type SessionEvidenceListener = Arc<dyn Fn(SessionEvidence) + Send + Sync>;
pub type UnexpectedCloseListener = Arc<dyn Fn(UnexpectedCloseReason) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFailureKind {
    Config,
    Protocol,
    AuthStatic,
    AuthOauth,
    Transport,
}

impl McpFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Protocol => "protocol",
            Self::AuthStatic => "auth-static",
            Self::AuthOauth => "auth-oauth",
            Self::Transport => "transport",
        }
    }
}

#[derive(Debug)]
pub struct McpClassifiedStartupError {
    pub failure_kind: McpFailureKind,
    message: String,
    cause: Option<WireAsyncError>,
}

impl McpClassifiedStartupError {
    pub fn new(failure_kind: McpFailureKind, message: impl Into<String>, cause: Option<WireAsyncError>) -> Self {
        Self {
            failure_kind,
            message: message.into(),
            cause,
        }
    }
}

impl fmt::Display for McpClassifiedStartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for McpClassifiedStartupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.cause.as_deref().map(|cause| cause as &(dyn Error + 'static))
    }
}

pub fn classify_mcp_discovery_error(error: WireAsyncError) -> WireAsyncError {
    let deterministic = mcp_error_code(error.as_ref()) == Some(MCP_ERROR_METHOD_NOT_FOUND)
        || is_mcp_malformed_result_error(error.as_ref());
    if !deterministic {
        return error;
    }
    Box::new(McpClassifiedStartupError::new(
        McpFailureKind::Protocol,
        error.to_string(),
        Some(error),
    ))
}

pub fn is_mcp_connection_closed_error(error: &(dyn Error + 'static)) -> bool {
    mcp_error_code(error) == Some(MCP_ERROR_CONNECTION_CLOSED)
}

pub fn is_mcp_transport_failure(error: &(dyn Error + 'static)) -> bool {
    if is_mcp_connection_closed_error(error) {
        return true;
    }
    !is_mcp_error(error)
}

pub fn is_mcp_malformed_result_error(error: &(dyn Error + 'static)) -> bool {
    error.downcast_ref::<McpSdkError>().is_some_and(|error| error.name == "ZodError")
        || error.downcast_ref::<Error2>().is_some_and(|error| error.name() == "ZodError")
}

pub async fn probe_mcp_liveness(client: &dyn super::types::MCPClient, signal: AbortSignal) -> bool {
    match client.ping(Some(signal)).await {
        Ok(()) => true,
        Err(error) if is_mcp_connection_closed_error(error.as_ref()) => false,
        Err(error) if is_mcp_malformed_result_error(error.as_ref()) => true,
        Err(error) => mcp_error_code(error.as_ref())
            .is_some_and(|code| code != MCP_ERROR_REQUEST_TIMEOUT),
    }
}

fn mcp_error_code(error: &(dyn Error + 'static)) -> Option<i64> {
    if error.downcast_ref::<ConnectionClosedError>().is_some() {
        return Some(MCP_ERROR_CONNECTION_CLOSED);
    }
    if error.downcast_ref::<RequestTimeoutError>().is_some() {
        return Some(MCP_ERROR_REQUEST_TIMEOUT);
    }
    if let Some(error) = error.downcast_ref::<McpSdkError>() {
        return error.code;
    }
    if let Some(error) = error.downcast_ref::<RpcError>() {
        return Some(error.code);
    }
    None
}

fn is_mcp_error(error: &(dyn Error + 'static)) -> bool {
    error.downcast_ref::<McpSdkError>().is_some()
        || error.downcast_ref::<RpcError>().is_some()
        || error.downcast_ref::<ConnectionClosedError>().is_some()
        || error.downcast_ref::<RequestTimeoutError>().is_some()
}

#[derive(Clone, Default)]
pub struct McpRequestOptions {
    pub timeout: Option<u64>,
    pub signal: Option<AbortSignal>,
}

pub fn build_request_options(timeout_ms: Option<u64>, signal: Option<AbortSignal>) -> Option<McpRequestOptions> {
    if timeout_ms.is_none() && signal.is_none() {
        return None;
    }
    Some(McpRequestOptions {
        timeout: timeout_ms,
        signal,
    })
}

pub fn to_mcp_tool_definition(tool: &Value) -> Result<MCPToolDefinition, WireAsyncError> {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Box::new(McpSdkError::new("ZodError", None, "tool name is required")) as WireAsyncError
        })?
        .to_string();
    let description = match tool.get("description") {
        Some(description) => description.as_str().ok_or_else(|| {
            Box::new(McpSdkError::new("ZodError", None, "tool description must be a string")) as WireAsyncError
        })?,
        None => "",
    }
    .to_string();
    let input_schema = tool
        .get("inputSchema")
        .filter(|schema| schema.is_object())
        .cloned()
        .ok_or_else(|| {
            Box::new(McpSdkError::new("ZodError", None, "tool inputSchema is required")) as WireAsyncError
        })?;
    Ok(MCPToolDefinition {
        name,
        description,
        input_schema,
    })
}

pub fn to_mcp_tool_result(result: &Value) -> MCPToolResult {
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        return MCPToolResult {
            content: content.iter().map(to_content_block).collect(),
            is_error: result.get("isError").and_then(Value::as_bool) == Some(true),
            structured_content: result.get("structuredContent").cloned(),
            meta: result.get("_meta").and_then(Value::as_object).cloned(),
        };
    }
    if let Some(legacy) = result.get("toolResult") {
        return MCPToolResult {
            content: vec![MCPContentBlock {
                type_: "text".to_string(),
                text: Some(
                    legacy
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| serde_json::to_string(legacy).unwrap()),
                ),
                data: None,
                mime_type: None,
                uri: None,
                resource: None,
                extra: Map::new(),
            }],
            is_error: false,
            structured_content: None,
            meta: None,
        };
    }
    MCPToolResult {
        content: Vec::new(),
        is_error: false,
        structured_content: None,
        meta: None,
    }
}

fn to_content_block(value: &Value) -> MCPContentBlock {
    let mut extra = value.as_object().cloned().unwrap_or_default();
    let type_ = extra
        .remove("type")
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    let text = extra
        .remove("text")
        .and_then(|value| value.as_str().map(str::to_string));
    let data = extra
        .remove("data")
        .and_then(|value| value.as_str().map(str::to_string));
    let mime_type = extra
        .remove("mimeType")
        .and_then(|value| value.as_str().map(str::to_string));
    let uri = extra
        .remove("uri")
        .and_then(|value| value.as_str().map(str::to_string));
    let resource = extra.remove("resource").and_then(|resource| {
        serde_json::from_value(resource).ok()
    });
    MCPContentBlock {
        type_,
        text,
        data,
        mime_type,
        uri,
        resource,
        extra,
    }
}

#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn start(&self) -> Result<(), WireAsyncError>;
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        options: Option<McpRequestOptions>,
    ) -> Result<Value, WireAsyncError>;
    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), WireAsyncError>;
    async fn close(&self) -> Result<(), WireAsyncError>;
    fn set_protocol_version(&self, _version: &str) {}
    fn has_session(&self) -> bool {
        false
    }
    async fn exit_error(&self) -> Result<Option<WireAsyncError>, WireAsyncError> {
        Ok(None)
    }
}

#[derive(serde::Deserialize)]
struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
    capabilities: Map<String, Value>,
    #[serde(rename = "serverInfo")]
    _server_info: InitializeServerInfo,
}

#[derive(serde::Deserialize)]
struct InitializeServerInfo {
    #[serde(rename = "name")]
    _name: String,
    #[serde(rename = "version")]
    _version: String,
}

pub struct McpJsonRpcClient {
    transport: Arc<dyn McpTransport>,
    started: std::sync::atomic::AtomicBool,
    closed: std::sync::atomic::AtomicBool,
    ready: std::sync::atomic::AtomicBool,
    server_capabilities: Mutex<Option<Map<String, Value>>>,
}

impl McpJsonRpcClient {
    pub fn new(transport: Arc<dyn McpTransport>) -> Self {
        Self {
            transport,
            started: std::sync::atomic::AtomicBool::new(false),
            closed: std::sync::atomic::AtomicBool::new(false),
            ready: std::sync::atomic::AtomicBool::new(false),
            server_capabilities: Mutex::new(None),
        }
    }

    pub fn transport(&self) -> &Arc<dyn McpTransport> {
        &self.transport
    }

    pub fn started(&self) -> bool {
        self.started.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn connect(&self, name: &str, version: &str, timeout_ms: Option<u64>) -> Result<(), WireAsyncError> {
        if self.closed() {
            return Err("MCP client is closed".into());
        }
        if self.started() {
            return Ok(());
        }
        if self
            .started
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            return Ok(());
        }
        let result = async {
            self.transport.start().await?;
            if self.transport.has_session() {
                return Ok(());
            }
            let result = self
                .transport
                .request(
                    "initialize",
                    Some(json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": name, "version": version },
                    })),
                    build_request_options(timeout_ms, None),
                )
                .await?;
            let result = serde_json::from_value::<InitializeResult>(result).map_err(|error| {
                Box::new(McpSdkError::new("ZodError", None, error.to_string())) as WireAsyncError
            })?;
            const SUPPORTED: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];
            if !SUPPORTED.contains(&result.protocol_version.as_str()) {
                return Err(format!("Server's protocol version is not supported: {}", result.protocol_version).into());
            }
            self.transport.set_protocol_version(&result.protocol_version);
            *self.server_capabilities.lock() = Some(result.capabilities);
            self.transport.notify("notifications/initialized", None).await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = self.close_started_client().await;
            return Err(error);
        }
        if self.closed() {
            let _ = self.close_started_client().await;
            return Ok(());
        }
        self.ready.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub async fn close(&self) -> Result<(), WireAsyncError> {
        if self.closed.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        self.close_started_client().await
    }

    async fn close_started_client(&self) -> Result<(), WireAsyncError> {
        if !self.started.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        self.transport.close().await
    }

    pub async fn list_tools(&self, timeout_ms: Option<u64>) -> Result<Vec<MCPToolDefinition>, WireAsyncError> {
        self.ensure_ready()?;
        self.ensure_tools_capability("tools/list")?;
        let result = self
            .transport
            .request("tools/list", None, build_request_options(timeout_ms, None))
            .await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                Box::new(McpSdkError::new(
                    "ZodError",
                    None,
                    "tools/list result must carry tools",
                )) as WireAsyncError
            })?;
        tools.iter().map(to_mcp_tool_definition).collect()
    }

    pub async fn call_tool(
        &self,
        name: &str,
        args: Map<String, Value>,
        options: Option<McpRequestOptions>,
    ) -> Result<MCPToolResult, WireAsyncError> {
        self.ensure_ready()?;
        self.ensure_tools_capability("tools/call")?;
        let result = self
            .transport
            .request(
                "tools/call",
                Some(json!({ "name": name, "arguments": args })),
                options,
            )
            .await?;
        Ok(to_mcp_tool_result(&result))
    }

    pub async fn ping(&self, options: Option<McpRequestOptions>) -> Result<(), WireAsyncError> {
        self.ensure_ready()?;
        self.transport.request("ping", None, options).await?;
        Ok(())
    }

    fn ensure_ready(&self) -> Result<(), WireAsyncError> {
        if !self.started() || self.closed() || !self.ready() {
            return Err("Not connected".into());
        }
        Ok(())
    }

    fn ensure_tools_capability(&self, method: &str) -> Result<(), WireAsyncError> {
        let supported = self
            .server_capabilities
            .lock()
            .as_ref()
            .and_then(|capabilities| capabilities.get("tools"))
            .is_some_and(|tools| !tools.is_null());
        if !supported {
            return Err(format!("Server does not support tools (required for {method})").into());
        }
        Ok(())
    }
}

struct PendingCall {
    sender: tokio::sync::oneshot::Sender<Result<Value, WireAsyncError>>,
    timer: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Clone)]
pub struct RpcPending {
    next_id: Arc<Mutex<i64>>,
    pending: Arc<Mutex<HashMap<i64, PendingCall>>>,
}

impl RpcPending {
    pub fn new() -> Self {
        Self {
            next_id: Arc::new(Mutex::new(0)),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn next_id(&self) -> i64 {
        let mut next = self.next_id.lock();
        let id = *next;
        *next += 1;
        id
    }

    pub fn register(
        &self,
        id: i64,
        options: Option<McpRequestOptions>,
    ) -> tokio::sync::oneshot::Receiver<Result<Value, WireAsyncError>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let pending = self.pending.clone();
        let timer = options.and_then(|options| {
            options.timeout.map(|timeout| {
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(timeout)).await;
                    if let Some(pending) = pending.lock().remove(&id) {
                        let mut data = Map::new();
                        data.insert("timeout".to_string(), timeout.into());
                        let _ = pending.sender.send(Err(Box::new(McpSdkError {
                            name: "McpError".to_string(),
                            code: Some(MCP_ERROR_REQUEST_TIMEOUT),
                            message: "MCP error -32001: Request timed out".to_string(),
                            data: Some(Value::Object(data)),
                        })));
                    }
                })
            })
        });
        self.pending.lock().insert(id, PendingCall { sender, timer });
        receiver
    }

    pub fn handle(&self, message: &Value) -> bool {
        let Some(id) = message.get("id").and_then(Value::as_i64) else {
            return false;
        };
        let error_body = match message.get("error") {
            Some(error) => match serde_json::from_value::<crate::remote::protocol::messages::JsonRpcErrorBody>(error.clone()) {
                Ok(error) => Some(error),
                Err(_) => return false,
            },
            None => None,
        };
        if error_body.is_none() && !message.as_object().is_some_and(|message| message.contains_key("result")) {
            return false;
        }
        let pending = self.pending.lock().remove(&id);
        let Some(pending) = pending else {
            return false;
        };
        if let Some(timer) = pending.timer {
            timer.abort();
        }
        if let Some(error) = error_body {
            let _ = pending.sender.send(Err(Box::new(McpSdkError {
                name: "McpError".to_string(),
                code: Some(error.code),
                message: format!("MCP error {}: {}", error.code, error.message),
                data: error.data,
            })));
        } else {
            let _ = pending.sender.send(Ok(message.get("result").cloned().unwrap_or(Value::Null)));
        }
        true
    }

    pub fn cancel(&self, id: i64) {
        if let Some(pending) = self.pending.lock().remove(&id) {
            if let Some(timer) = pending.timer {
                timer.abort();
            }
        }
    }

    pub fn close_all(&self, reason: &str) {
        let _ = reason;
        for (_, pending) in self.pending.lock().drain() {
            if let Some(timer) = pending.timer {
                timer.abort();
            }
            let _ = pending.sender.send(Err(Box::new(McpSdkError::new(
                "McpError",
                Some(MCP_ERROR_CONNECTION_CLOSED),
                "MCP error -32000: Connection closed",
            ))));
        }
    }
}

pub struct ReqwestFetch {
    client: reqwest::Client,
}

impl ReqwestFetch {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl FetchLike for ReqwestFetch {
    async fn fetch(&self, url: &str, init: FetchInit) -> Result<FetchResponse, crate::_base::utils::fetch_like::FetchError> {
        let method = init.method.unwrap_or_else(|| "GET".to_string());
        let mut request = self.client.request(
            reqwest::Method::from_bytes(method.as_bytes())?,
            url,
        );
        for (key, value) in init.headers {
            request = request.header(&key, &value);
        }
        if let Some(body) = init.body {
            request = request.body(body);
        }
        if let Some(timeout) = init.timeout_ms {
            request = request.timeout(std::time::Duration::from_millis(timeout));
        }
        let response = if let Some(signal) = init.signal {
            tokio::select! {
                result = request.send() => result?,
                _ = signal.cancelled() => return Err("fetch aborted".into()),
            }
        } else {
            request.send().await?
        };
        let status = response.status();
        let headers = response
            .headers()
            .iter()
            .map(|(key, value)| {
                (
                    key.as_str().to_string(),
                    value.to_str().unwrap_or("").to_string(),
                )
            })
            .collect::<Vec<_>>();
        Ok(FetchResponse::from_stream(
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            headers,
            response.bytes_stream().map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(|error| error.into())).boxed(),
        ))
    }
}
