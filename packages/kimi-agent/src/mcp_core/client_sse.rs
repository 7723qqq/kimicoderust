use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::_base::utils::abort::{AbortError, AbortSignal, abort_error};
use crate::_base::utils::fetch_like::{FetchInit, FetchLike};
use crate::mcp_core::errors::MCP_STARTUP_FAILED;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;

use super::client_http::OAuthClientProvider;
use super::client_remote::{SseError, build_mcp_remote_headers, classify_remote_startup_error};
use super::client_shared::*;
use super::config_schema::McpServerSseConfig;
use super::types::{MCPClient, MCPToolDefinition, MCPToolResult};

pub struct SseMcpClientOptions {
    pub client_name: Option<String>,
    pub client_version: Option<String>,
    pub startup_timeout_ms: Option<u64>,
    pub tool_call_timeout_ms: Option<u64>,
    pub env_lookup: Option<Arc<dyn Fn(&str) -> Option<String> + Send + Sync>>,
    pub fetch: Option<Arc<dyn FetchLike>>,
    pub oauth_provider: Option<Arc<dyn OAuthClientProvider>>,
}

impl Default for SseMcpClientOptions {
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

struct SseTransportState {
    started: bool,
    closed: bool,
    post_url: Option<String>,
    reader_finished: bool,
    reader_task: Option<tokio::task::JoinHandle<()>>,
    on_close: Option<Arc<dyn Fn() + Send + Sync>>,
    on_error: Option<Arc<dyn Fn(WireAsyncError) + Send + Sync>>,
    last_error: Option<String>,
}

struct SseTransport {
    fetch: Arc<dyn FetchLike>,
    url: String,
    headers: Option<HashMap<String, String>>,
    oauth_provider: Option<Arc<dyn OAuthClientProvider>>,
    next_id: Mutex<i64>,
    pending: RpcPending,
    state: Arc<Mutex<SseTransportState>>,
    endpoint_notify: Arc<tokio::sync::Notify>,
}

impl SseTransport {
    async fn headers(&self) -> Result<HashMap<String, String>, WireAsyncError> {
        let mut headers = self.headers.clone().unwrap_or_default();
        if let Some(provider) = &self.oauth_provider {
            if let Some(token) = provider.authorize().await? {
                headers.insert("Authorization".to_string(), format!("Bearer {token}"));
            }
        }
        Ok(headers)
    }

    async fn post_json(&self, body: Value, options: Option<McpRequestOptions>) -> Result<String, WireAsyncError> {
        let post_url = self
            .state
            .lock()
            .post_url
            .clone()
            .ok_or_else(|| "Not connected".to_string())?;
        let options = options.unwrap_or_default();
        let mut headers = self.headers().await?;
        headers.insert("content-type".to_string(), "application/json".to_string());
        let response = self
            .fetch
            .fetch(
                &post_url,
                FetchInit {
                    headers: headers.into_iter().collect(),
                    method: Some("POST".to_string()),
                    body: Some(serde_json::to_string(&body)?),
                    signal: options.signal.map(|signal| signal.cancellation_token()),
                    manual_redirect: false,
                    timeout_ms: options.timeout,
                },
            )
            .await?;
        if matches!(response.status, 401 | 403) {
            return Err(Box::new(SseError::new(
                Some(response.status),
                format!("HTTP {}", response.status),
            )));
        }
        if !response.ok {
            return Err(Box::new(SseError::new(
                Some(response.status),
                format!("HTTP {}: {}", response.status, response.status_text),
            )));
        }
        response.text().await
    }

    fn set_on_close(&self, listener: Arc<dyn Fn() + Send + Sync>) {
        self.state.lock().on_close = Some(listener);
    }

    fn set_on_error(&self, listener: Arc<dyn Fn(WireAsyncError) + Send + Sync>) {
        self.state.lock().on_error = Some(listener);
    }

    fn fire_error(&self, error: WireAsyncError) {
        self.state.lock().last_error = Some(error.to_string());
        if let Some(listener) = self.state.lock().on_error.clone() {
            listener(error);
        }
    }

    fn fire_close(&self) {
        if let Some(listener) = self.state.lock().on_close.clone() {
            listener();
        }
    }
}

#[async_trait]
impl McpTransport for SseTransport {
    async fn start(&self) -> Result<(), WireAsyncError> {
        {
            let state = self.state.lock();
            if state.started {
                return Ok(());
            }
            if state.closed {
                return Err("SSE transport is closed".into());
            }
        }
        let mut headers = self.headers().await?;
        headers.insert("accept".to_string(), "text/event-stream".to_string());
        let response = self
            .fetch
            .fetch(
                &self.url,
                FetchInit {
                    headers: headers.into_iter().collect(),
                    method: Some("GET".to_string()),
                    body: None,
                    signal: None,
                    manual_redirect: false,
                    timeout_ms: None,
                },
            )
            .await?;
        if matches!(response.status, 401 | 403) {
            return Err(Box::new(SseError::new(
                Some(response.status),
                format!("HTTP {}", response.status),
            )));
        }
        if !response.ok {
            return Err(Box::new(SseError::new(
                Some(response.status),
                format!("HTTP {}: {}", response.status, response.status_text),
            )));
        }
        let Some(stream) = response.stream() else {
            return Err("SSE response did not include a stream".into());
        };
        self.state.lock().started = true;
        let state = self.state.clone();
        let pending = self.pending.clone();
        let base_url = self.url.clone();
        let endpoint_notify = self.endpoint_notify.clone();
        let task_state = state.clone();
        let task_notify = endpoint_notify.clone();
        let task = tokio::spawn(async move {
            futures_util::pin_mut!(stream);
            let mut buffer = Vec::new();
            let mut event_name = "message".to_string();
            let mut event_data = String::new();
            let flush_event = |event_name: &mut String, event_data: &mut String, state: &Arc<Mutex<SseTransportState>>, pending: &RpcPending, base_url: &str, endpoint_notify: &tokio::sync::Notify| {
                let mut fatal_error = None;
                if event_name == "endpoint" {
                    let endpoint = event_data.trim();
                    match url::Url::parse(base_url).and_then(|base| base.join(endpoint)) {
                        Ok(url) => {
                            state.lock().post_url = Some(url.to_string());
                            endpoint_notify.notify_one();
                        }
                        Err(error) => fatal_error = Some(error),
                    }
                } else if event_name == "message" {
                    match serde_json::from_str::<Value>(event_data) {
                        Ok(message) => pending.handle(&message),
                        Err(error) => {
                            state.lock().last_error = Some(error.to_string());
                            let listener = state.lock().on_error.clone();
                            if let Some(listener) = listener {
                                listener(Box::new(error));
                            }
                        }
                    }
                }
                *event_name = "message".to_string();
                event_data.clear();
                if let Some(error) = fatal_error {
                    state.lock().last_error = Some(error.to_string());
                    let listener = state.lock().on_error.clone();
                    if let Some(listener) = listener {
                        listener(Box::new(error));
                    }
                    return false;
                }
                true
            };
            'read: while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(chunk) => {
                        buffer.extend(chunk);
                        while let Some((index, separator_len)) = find_sse_event_boundary(&buffer) {
                            let event = String::from_utf8_lossy(&buffer[..index]).into_owned();
                            buffer.drain(..index + separator_len);
                            for line in event.lines() {
                                if let Some(data) = line.strip_prefix("event:") {
                                    *event_name = data.trim().to_string();
                                } else if let Some(data) = line.strip_prefix("data:") {
                                    if !event_data.is_empty() {
                                        event_data.push('\n');
                                    }
                                    event_data.push_str(data.strip_prefix(' ').unwrap_or(data));
                                }
                            }
                            if !flush_event(&mut event_name, &mut event_data, &task_state, &pending, &base_url, &task_notify) {
                                break 'read;
                            }
                        }
                    }
                    Err(error) => {
                        task_state.lock().last_error = Some(error.to_string());
                        let listener = task_state.lock().on_error.clone();
                        if let Some(listener) = listener {
                            listener(error);
                        }
                        break;
                    }
                }
            }
            task_state.lock().reader_finished = true;
            task_notify.notify_one();
            pending.close_all("sse transport closed");
            let listener = task_state.lock().on_close.clone();
            if let Some(listener) = listener {
                listener();
            }
        });
        self.state.lock().reader_task = Some(task);
        loop {
            let (ready, finished, error) = {
                let state = self.state.lock();
                (
                    state.post_url.is_some(),
                    state.reader_finished,
                    state.last_error.clone(),
                )
            };
            if ready {
                return Ok(());
            }
            if finished {
                return Err(error.unwrap_or_else(|| "sse transport closed".to_string()).into());
            }
            endpoint_notify.notified().await;
        }
    }

    async fn request(&self, method: &str, params: Option<Value>, options: Option<McpRequestOptions>) -> Result<Value, WireAsyncError> {
        if self.state.lock().closed {
            return Err("SSE transport is closed".into());
        }
        let id = {
            let mut next = self.next_id.lock();
            let id = *next;
            *next += 1;
            id
        };
        let mut receiver = self.pending.register(id, options.clone());
        let mut body = Map::new();
        body.insert("jsonrpc".to_string(), "2.0".into());
        body.insert("id".to_string(), id.into());
        body.insert("method".to_string(), method.into());
        if let Some(params) = params {
            body.insert("params".to_string(), params);
        }
        let post_options = options.clone().map(|mut options| {
            options.timeout = None;
            options
        });
        let post = self.post_json(body.into(), post_options);
        futures_util::pin_mut!(post);
        let signal = options.and_then(|options| options.signal);
        if let Some(signal) = &signal {
            tokio::select! {
                result = &mut post => {
                    if let Err(error) = result {
                        self.pending.cancel(id);
                        return Err(error);
                    }
                }
                result = &mut receiver => {
                    return result.unwrap_or_else(|_| Err("SSE request closed".into()));
                }
                _ = signal.cancelled() => {
                    self.pending.cancel(id);
                    return Err(sse_request_abort_error(signal));
                }
            }
        } else {
            tokio::select! {
                result = &mut post => {
                    if let Err(error) = result {
                        self.pending.cancel(id);
                        return Err(error);
                    }
                }
                result = &mut receiver => {
                    return result.unwrap_or_else(|_| Err("SSE request closed".into()));
                }
            }
        }
        match signal {
            Some(signal) => {
                tokio::select! {
                    result = receiver => result.unwrap_or_else(|_| Err("SSE request closed".into())),
                    _ = signal.cancelled() => {
                        self.pending.cancel(id);
                        Err(sse_request_abort_error(&signal))
                    }
                }
            }
            None => receiver.await.unwrap_or_else(|_| Err("SSE request closed".into())),
        }
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), WireAsyncError> {
        if self.state.lock().closed {
            return Err("SSE transport is closed".into());
        }
        let mut body = Map::new();
        body.insert("jsonrpc".to_string(), "2.0".into());
        body.insert("method".to_string(), method.into());
        if let Some(params) = params {
            body.insert("params".to_string(), params);
        }
        self.post_json(body.into(), None).await?;
        Ok(())
    }

    async fn close(&self) -> Result<(), WireAsyncError> {
        let task = {
            let mut state = self.state.lock();
            if state.closed {
                return Ok(());
            }
            state.closed = true;
            state.reader_finished = true;
            state.reader_task.take()
        };
        self.endpoint_notify.notify_one();
        if let Some(task) = task {
            task.abort();
        }
        self.pending.close_all("sse transport closed");
        Ok(())
    }

    async fn exit_error(&self) -> Result<Option<WireAsyncError>, WireAsyncError> {
        Ok(self.state.lock().last_error.clone().map(|error| error.into()))
    }
}

fn sse_request_abort_error(signal: &AbortSignal) -> WireAsyncError {
    match signal.reason() {
        Some(reason) => reason
            .downcast_ref::<AbortError>()
            .map(|error| Box::new(error.clone()) as WireAsyncError)
            .unwrap_or_else(|| reason.to_string().into()),
        None => Box::new(abort_error()),
    }
}

fn find_sse_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
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

pub struct SseMcpClient {
    client: Arc<McpJsonRpcClient>,
    transport: Arc<SseTransport>,
    config: McpServerSseConfig,
    client_name: String,
    client_version: String,
    startup_timeout_ms: Option<u64>,
    tool_call_timeout_ms: Option<u64>,
    state: Arc<Mutex<SseClientState>>,
}

struct SseClientState {
    session_evidence_listener: Option<SessionEvidenceListener>,
    last_transport_error: Option<WireAsyncError>,
    pending_session_evidence: Option<SessionEvidence>,
    hard_evidence_fired: bool,
}

impl SseMcpClient {
    pub fn new(config: McpServerSseConfig, options: SseMcpClientOptions) -> Result<Self, WireAsyncError> {
        let env_lookup = options.env_lookup.unwrap_or_else(|| {
            Arc::new(|name| std::env::var(name).ok())
        });
        let headers = build_mcp_remote_headers(&config, env_lookup)?;
        let transport = Arc::new(SseTransport {
            fetch: options.fetch.unwrap_or_else(|| Arc::new(ReqwestFetch::new())),
            url: config.url.clone().unwrap(),
            headers,
            oauth_provider: options.oauth_provider,
            next_id: Mutex::new(0),
            pending: RpcPending::new(),
            state: Arc::new(Mutex::new(SseTransportState {
                started: false,
                closed: false,
                post_url: None,
                reader_finished: false,
                reader_task: None,
                on_close: None,
                on_error: None,
                last_error: None,
            })),
            endpoint_notify: Arc::new(tokio::sync::Notify::new()),
        });
        Ok(Self {
            client: Arc::new(McpJsonRpcClient::new(transport.clone())),
            transport,
            config,
            client_name: options.client_name.unwrap_or_else(|| KIMI_MCP_CLIENT_NAME.to_string()),
            client_version: options.client_version.unwrap_or_else(|| KIMI_MCP_CLIENT_VERSION.to_string()),
            startup_timeout_ms: options.startup_timeout_ms,
            tool_call_timeout_ms: options.tool_call_timeout_ms,
            state: Arc::new(Mutex::new(SseClientState {
                session_evidence_listener: None,
                last_transport_error: None,
                pending_session_evidence: None,
                hard_evidence_fired: false,
            })),
        })
    }

    pub async fn connect(&self) -> Result<(), WireAsyncError> {
        if self.client.closed() {
            return Err(Box::new(Error2::new(MCP_STARTUP_FAILED, "MCP SSE client is closed")));
        }
        if self.client.started() {
            return Ok(());
        }
        self.install_transport_hooks();
        let result = self
            .client
            .connect(&self.client_name, &self.client_version, self.startup_timeout_ms)
            .await;
        if let Err(error) = result {
            return Err(classify_remote_startup_error(&self.config, error));
        }
        if self.client.closed() {
            let _ = self.client.close().await;
            return Err(Box::new(Error2::new(
                MCP_STARTUP_FAILED,
                "MCP SSE client was closed during startup",
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

    fn install_transport_hooks(&self) {
        let weak = Arc::downgrade(&self.state);
        self.transport.set_on_close(Arc::new(move || {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let this = SseClientStateHandle {
                client: self.client.clone(),
                state,
            };
            if this.client_closed() || !this.client_ready() {
                return;
            }
            this.fire(SessionEvidence {
                kind: SessionEvidenceKind::Hard,
                reason: UnexpectedCloseReason {
                    error: this.last_error(),
                    stderr: None,
                },
            });
        }));
        let weak = Arc::downgrade(&self.state);
        self.transport.set_on_error(Arc::new(move |error| {
            if let Some(state) = weak.upgrade() {
                let this = SseClientStateHandle {
                client: self.client.clone(),
                state,
            };
                this.record_error(&error);
            }
        }));
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
        if is_terminal_sse_transport_error(error.as_ref()) {
            self.fire_session_evidence(SessionEvidence {
                kind: SessionEvidenceKind::Hard,
                reason: UnexpectedCloseReason {
                    error: Some(error.to_string().into()),
                    stderr: None,
                },
            });
        }
    }
}

struct SseClientStateHandle {
    client: Arc<McpJsonRpcClient>,
    state: Arc<Mutex<SseClientState>>,
}

impl SseClientStateHandle {
    fn client_closed(&self) -> bool {
        self.client.closed()
    }

    fn client_ready(&self) -> bool {
        self.client.ready()
    }

    fn last_error(&self) -> Option<WireAsyncError> {
        self.state
            .lock()
            .last_transport_error
            .as_ref()
            .map(|error| error.to_string().into())
    }

    fn fire(&self, evidence: SessionEvidence) {
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
        if !self.client_ready() || self.client_closed() {
            return;
        }
        if is_terminal_sse_transport_error(error.as_ref()) {
            self.fire(SessionEvidence {
                kind: SessionEvidenceKind::Hard,
                reason: UnexpectedCloseReason {
                    error: Some(error.to_string().into()),
                    stderr: None,
                },
            });
        }
    }
}

#[async_trait]
impl MCPClient for SseMcpClient {
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

pub fn is_terminal_sse_transport_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if error.downcast_ref::<super::client_remote::UnauthorizedError>().is_some()
        || error.downcast_ref::<Error2>().is_some_and(|error| error.name() == "UnauthorizedError")
        || error.downcast_ref::<McpSdkError>().is_some_and(|error| error.name == "UnauthorizedError")
    {
        return true;
    }
    if let Some(error) = error.downcast_ref::<SseError>() {
        return error.code.is_some();
    }
    false
}
