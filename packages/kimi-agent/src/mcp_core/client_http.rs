use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::_base::utils::abort::AbortSignal;
use crate::_base::utils::fetch_like::{FetchInit, FetchLike, FetchResponse};
use crate::mcp_core::errors::MCP_STARTUP_FAILED;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;

use super::client_remote::{
    StreamableHTTPError, build_mcp_remote_headers, classify_remote_startup_error,
};
use super::client_shared::*;
use super::config_schema::McpServerHttpConfig;
use super::types::{MCPClient, MCPToolDefinition, MCPToolResult};

#[async_trait]
pub trait OAuthClientProvider: Send + Sync {
    async fn authorize(&self) -> Result<Option<String>, WireAsyncError>;
}

pub struct HttpMcpClientOptions {
    pub client_name: Option<String>,
    pub client_version: Option<String>,
    pub startup_timeout_ms: Option<u64>,
    pub tool_call_timeout_ms: Option<u64>,
    pub env_lookup: Option<Arc<dyn Fn(&str) -> Option<String> + Send + Sync>>,
    pub fetch: Option<Arc<dyn FetchLike>>,
    pub oauth_provider: Option<Arc<dyn OAuthClientProvider>>,
}

impl Default for HttpMcpClientOptions {
    fn default() -> Self {
        Self {
            client_name: None,
            client_version: None,
            startup_timeout_ms: None,
            tool_call_timeout_ms: None,
            env_lookup: None,
            fetch: None,
            oauth_provider: None,
        }
    }
}

struct HttpTransport {
    fetch: Arc<dyn FetchLike>,
    url: String,
    headers: Option<HashMap<String, String>>,
    oauth_provider: Option<Arc<dyn OAuthClientProvider>>,
    next_id: Mutex<i64>,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
}

impl HttpTransport {
    async fn send(&self, id: Option<i64>, method: &str, params: Option<Value>, options: Option<McpRequestOptions>) -> Result<Value, WireAsyncError> {
        let mut headers = self.headers.clone().unwrap_or_default();
        if let Some(provider) = &self.oauth_provider {
            if let Some(token) = provider.authorize().await? {
                if !headers.keys().any(|key| key.eq_ignore_ascii_case("authorization")) {
                    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
                }
            }
        }
        if let Some(session_id) = self.session_id.lock().clone() {
            if !headers.keys().any(|key| key.eq_ignore_ascii_case("mcp-session-id")) {
                headers.insert("mcp-session-id".to_string(), session_id);
            }
        }
        if let Some(version) = self.protocol_version.lock().clone() {
            if !headers.keys().any(|key| key.eq_ignore_ascii_case("mcp-protocol-version")) {
                headers.insert("mcp-protocol-version".to_string(), version);
            }
        }
        headers.insert("content-type".to_string(), "application/json".to_string());
        headers.insert("accept".to_string(), "application/json, text/event-stream".to_string());
        let mut request = Map::new();
        request.insert("jsonrpc".to_string(), "2.0".into());
        if let Some(id) = id {
            request.insert("id".to_string(), id.into());
        }
        request.insert("method".to_string(), method.into());
        if let Some(params) = params {
            request.insert("params".to_string(), params);
        }
        let options = options.unwrap_or_default();
        let init = FetchInit {
            headers: headers.into_iter().collect(),
            method: Some("POST".to_string()),
            body: Some(serde_json::to_string(&request)?),
            signal: options.signal.map(|signal| signal.cancellation_token()),
            manual_redirect: false,
            timeout_ms: options.timeout,
        };
        let response = self.fetch.fetch(&self.url, init).await?;
        if let Some(session_id) = response.header("mcp-session-id").map(str::to_string) {
            *self.session_id.lock() = Some(session_id);
        }
        let status = response.status;
        let ok = response.ok;
        let content_type = response.header("content-type").map(str::to_string);
        if !ok {
            let text = response.text().await.unwrap_or_default();
            return Err(Box::new(StreamableHTTPError::new(
                Some(status),
                format!("Error POSTing to endpoint: {text}"),
            )));
        }
        if status == 202 || id.is_none() {
            let _ = response.text().await?;
            return Ok(Value::Object(Map::new()));
        }
        let media_type = content_type
            .as_deref()
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .unwrap_or("");
        if media_type.eq_ignore_ascii_case("application/json") {
            let text = response.text().await?;
            let value = serde_json::from_str::<Value>(&text).map_err(|error| {
                Box::new(McpSdkError::new("ZodError", None, error.to_string())) as WireAsyncError
            })?;
            return parse_json_rpc_response(value, id.unwrap());
        }
        if media_type.eq_ignore_ascii_case("text/event-stream") {
            return read_sse_json_rpc_response(response, id.unwrap()).await;
        }
        let _ = response.text().await?;
        Err(Box::new(StreamableHTTPError::new(
            None,
            format!("Unexpected content type: {}", content_type.unwrap_or_default()),
        )))
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn start(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }

    async fn request(&self, method: &str, params: Option<Value>, options: Option<McpRequestOptions>) -> Result<Value, WireAsyncError> {
        let id = {
            let mut next = self.next_id.lock();
            let id = *next;
            *next += 1;
            id
        };
        self.send(Some(id), method, params, options).await
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), WireAsyncError> {
        self.send(None, method, params, None).await?;
        Ok(())
    }

    async fn close(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }

    fn set_protocol_version(&self, version: &str) {
        *self.protocol_version.lock() = Some(version.to_string());
    }

    fn has_session(&self) -> bool {
        self.session_id.lock().is_some()
    }
}

fn parse_json_rpc_response(value: Value, expected_id: i64) -> Result<Value, WireAsyncError> {
    let messages = match value {
        Value::Array(messages) => messages,
        value => vec![value],
    };
    for message in messages {
        if message.get("id").and_then(Value::as_i64) != Some(expected_id) {
            continue;
        }
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(Box::new(McpSdkError::new(
                "ZodError",
                None,
                "Invalid JSON-RPC response",
            )));
        }
        if let Some(error) = message.get("error") {
            let code = error.get("code").and_then(Value::as_i64).ok_or_else(|| {
                Box::new(McpSdkError::new("ZodError", None, "Invalid JSON-RPC error")) as WireAsyncError
            })?;
            let message_text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Err(Box::new(McpSdkError {
                name: "McpError".to_string(),
                code: Some(code),
                message: format!("MCP error {code}: {message_text}"),
                data: error.get("data").cloned(),
            }));
        }
        return message.get("result").cloned().ok_or_else(|| {
            Box::new(McpSdkError::new("ZodError", None, "Invalid JSON-RPC response")) as WireAsyncError
        });
    }
    Err(Box::new(McpSdkError::new(
        "ZodError",
        None,
        "JSON-RPC response id does not match request",
    )))
}

async fn read_sse_json_rpc_response(response: FetchResponse, expected_id: i64) -> Result<Value, WireAsyncError> {
    let Some(stream) = response.stream() else {
        return Err("Streamable HTTP response did not include a stream".into());
    };
    futures_util::pin_mut!(stream);
    let mut buffer = Vec::new();
    let mut event_name = "message".to_string();
    let mut event_data = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.extend(chunk);
        while let Some((index, separator_len)) = find_http_sse_boundary(&buffer) {
            let event = String::from_utf8_lossy(&buffer[..index]).into_owned();
            buffer.drain(..index + separator_len);
            for line in event.lines() {
                if let Some(data) = line.strip_prefix("event:") {
                    event_name = data.trim().to_string();
                } else if let Some(data) = line.strip_prefix("data:") {
                    if !event_data.is_empty() {
                        event_data.push('\n');
                    }
                    event_data.push_str(data.strip_prefix(' ').unwrap_or(data));
                }
            }
            if event_name == "message" && !event_data.is_empty() {
                if let Ok(message) = serde_json::from_str::<Value>(&event_data) {
                    if message.get("id").and_then(Value::as_i64) == Some(expected_id) {
                        return parse_json_rpc_response(message, expected_id);
                    }
                }
            }
            event_name = "message".to_string();
            event_data.clear();
        }
    }
    Err(Box::new(McpSdkError::new(
        "McpError",
        Some(MCP_ERROR_CONNECTION_CLOSED),
        "MCP error -32000: Connection closed",
    )))
}

fn find_http_sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer.iter().enumerate().find_map(|(index, byte)| {
        if *byte != b'\n' {
            return None;
        }
        if buffer.get(index + 1) == Some(&b'\n') {
            return Some((index, 2));
        }
        if buffer.get(index + 1) == Some(&b'\r') && buffer.get(index + 2) == Some(&b'\n') {
            return Some((index, 3));
        }
        None
    })
}

pub struct HttpMcpClient {
    client: Arc<McpJsonRpcClient>,
    config: McpServerHttpConfig,
    client_name: String,
    client_version: String,
    startup_timeout_ms: Option<u64>,
    tool_call_timeout_ms: Option<u64>,
    state: Arc<Mutex<HttpClientState>>,
}

struct HttpClientState {
    session_evidence_listener: Option<SessionEvidenceListener>,
    last_transport_error: Option<WireAsyncError>,
    pending_session_evidence: Option<SessionEvidence>,
    hard_evidence_fired: bool,
}

impl HttpMcpClient {
    pub fn new(config: McpServerHttpConfig, options: HttpMcpClientOptions) -> Result<Self, WireAsyncError> {
        let env_lookup = options.env_lookup.unwrap_or_else(|| {
            Arc::new(|name| std::env::var(name).ok())
        });
        let headers = build_mcp_remote_headers(&config, env_lookup)?;
        let transport = Arc::new(HttpTransport {
            fetch: options.fetch.unwrap_or_else(|| Arc::new(ReqwestFetch::new())),
            url: config.url.clone().unwrap(),
            headers,
            oauth_provider: options.oauth_provider,
            next_id: Mutex::new(0),
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
        });
        Ok(Self {
            client: Arc::new(McpJsonRpcClient::new(transport)),
            config,
            client_name: options.client_name.unwrap_or_else(|| KIMI_MCP_CLIENT_NAME.to_string()),
            client_version: options.client_version.unwrap_or_else(|| KIMI_MCP_CLIENT_VERSION.to_string()),
            startup_timeout_ms: options.startup_timeout_ms,
            tool_call_timeout_ms: options.tool_call_timeout_ms,
            state: Arc::new(Mutex::new(HttpClientState {
                session_evidence_listener: None,
                last_transport_error: None,
                pending_session_evidence: None,
                hard_evidence_fired: false,
            })),
        })
    }

    pub async fn connect(&self) -> Result<(), WireAsyncError> {
        if self.client.closed() {
            return Err(Box::new(Error2::new(MCP_STARTUP_FAILED, "MCP HTTP client is closed")));
        }
        if self.client.started() {
            return Ok(());
        }
        let result = self
            .client
            .connect(
                &self.client_name,
                &self.client_version,
                self.startup_timeout_ms,
            )
            .await;
        if let Err(error) = result {
            return Err(classify_remote_startup_error(&self.config, error));
        }
        if self.client.closed() {
            let _ = self.client.close().await;
            return Err(Box::new(Error2::new(
                MCP_STARTUP_FAILED,
                "MCP HTTP client was closed during startup",
            )));
        }
        Ok(())
    }

    pub async fn close(&self) -> Result<(), WireAsyncError> {
        self.client.close().await
    }

    pub fn on_session_evidence(&self, listener: SessionEvidenceListener) {
        let pending = {
            let mut state = self.state.lock();
            state.session_evidence_listener = Some(listener.clone());
            state.pending_session_evidence.take()
        };
        if let Some(pending) = pending {
            listener(pending);
        }
    }

    fn fire_session_evidence(&self, evidence: SessionEvidence) {
        let mut evidence = Some(evidence);
        let listener = {
            let mut state = self.state.lock();
            if state.hard_evidence_fired {
                return;
            }
            if evidence.as_ref().is_some_and(|evidence| evidence.kind == SessionEvidenceKind::Hard) {
                state.hard_evidence_fired = true;
            }
            match state.session_evidence_listener.clone() {
                Some(listener) => Some(listener),
                None => {
                    state.pending_session_evidence = evidence.take();
                    None
                }
            }
        };
        if let (Some(listener), Some(evidence)) = (listener, evidence) {
            listener(evidence);
        }
    }

    fn record_error(&self, error: &WireAsyncError) {
        self.state.lock().last_transport_error = Some(error.to_string().into());
        if !self.client.ready() || self.client.closed() {
            return;
        }
        if is_unauthorized_transport_error(error.as_ref()) {
            self.fire_session_evidence(SessionEvidence {
                kind: SessionEvidenceKind::Hard,
                reason: UnexpectedCloseReason {
                    error: Some(error.to_string().into()),
                    stderr: None,
                },
            });
        } else if is_sse_reconnection_exhausted_error(error.as_ref()) {
            self.fire_session_evidence(SessionEvidence {
                kind: SessionEvidenceKind::Weak,
                reason: UnexpectedCloseReason {
                    error: Some(error.to_string().into()),
                    stderr: None,
                },
            });
        }
    }
}

#[async_trait]
impl MCPClient for HttpMcpClient {
    async fn list_tools(&self) -> Result<Vec<MCPToolDefinition>, WireAsyncError> {
        let result = self.client.list_tools(self.startup_timeout_ms).await;
        if let Err(error) = &result {
            self.record_error(error);
        }
        result
    }

    async fn call_tool(
        &self,
        name: &str,
        args: HashMap<String, Value>,
        signal: Option<AbortSignal>,
    ) -> Result<MCPToolResult, WireAsyncError> {
        let result = self
            .client
            .call_tool(
                name,
                args.into_iter().collect(),
                build_request_options(self.tool_call_timeout_ms, signal),
            )
            .await;
        if let Err(error) = &result {
            self.record_error(error);
        }
        result
    }

    async fn ping(&self, signal: Option<AbortSignal>) -> Result<(), WireAsyncError> {
        let result = self
            .client
            .ping(build_request_options(Some(MCP_LIVENESS_PROBE_TIMEOUT_MS), signal))
            .await;
        if let Err(error) = &result {
            self.record_error(error);
        }
        result
    }
}

pub fn is_unauthorized_transport_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<super::client_remote::UnauthorizedError>().is_some()
        || error.downcast_ref::<Error2>().is_some_and(|error| error.name() == "UnauthorizedError")
        || error.downcast_ref::<McpSdkError>().is_some_and(|error| error.name == "UnauthorizedError")
}

pub fn is_sse_reconnection_exhausted_error(error: &(dyn std::error::Error + 'static)) -> bool {
    regex::Regex::new(r"(?i)Maximum reconnection attempts")
        .unwrap()
        .is_match(&error.to_string())
}

pub fn build_mcp_http_headers(
    config: &McpServerHttpConfig,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<HashMap<String, String>>, WireAsyncError> {
    build_mcp_remote_headers(config, env_lookup)
}
