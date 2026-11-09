//! MCP Streamable HTTP transport (`transport = "http"`).
//!
//! Mirrors the v2 `HttpMcpClient` / `StreamableHTTPClientTransport` wire
//! behavior (`mcpCore/client-http.ts`): every JSON-RPC message is POSTed to
//! the server URL, the reply may be a single JSON body or an SSE stream, and
//! a `Mcp-Session-Id` handed back by the server is echoed on later requests.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::Mutex;

use super::client_shared;
use super::client_shared::{Budget, WeakEvidenceListener, fire_or_buffer_weak_evidence};
use super::errors::McpError;
use crate::primitives::teardown::{Disposer, Ledger, TeardownReason};

/// Protocol version that introduced the Streamable HTTP transport.
pub const STREAMABLE_HTTP_PROTOCOL_VERSION: &str = "2025-03-26";
const SESSION_ID_HEADER: &str = "mcp-session-id";

pub struct McpHttpTransport {
    url: String,
    headers: HashMap<String, String>,
    client: reqwest::Client,
    /// `Mcp-Session-Id` returned by the server, echoed on every later request.
    /// Shared with the standalone GET stream task.
    session_id: Arc<Mutex<Option<String>>>,
    next_id: AtomicU64,
    /// Per-request timeout resolved from `toolTimeoutMs`; `None` keeps the
    /// shared built-in default (`client_shared::DEFAULT_REQUEST_TIMEOUT`).
    request_timeout: Option<Duration>,
    /// Set by an explicit `shutdown`; no new request or stream may start
    /// afterwards, and the open stream is aborted.
    closed: Arc<AtomicBool>,
    /// Weak session-evidence listener, at most one (v2
    /// `SessionEvidenceListener`). Weak evidence means the resumable stream
    /// gave up: the session may still answer, so the manager probes it.
    weak_evidence: WeakEvidenceListener,
    /// Signal buffered before a listener registers, replayed on registration
    /// (v2 `pendingSessionEvidence`, client-http.ts:43).
    pending_weak_evidence: Arc<Mutex<Option<String>>>,
    /// The standalone GET SSE stream (v2 `_startOrAuthSse`). Spawned only when
    /// the server answers `notifications/initialized` with 202.
    stream: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Stops the stream task when the transport shuts down.
    stream_stop: tokio::sync::watch::Sender<()>,
    /// Owns the close steps in reverse execution order (v2 `client-http.ts:86-90`:
    /// mark closed, stop the stream, abort it). A ledger makes the close
    /// idempotent and guarantees every step runs even if an earlier one fails.
    dispose: Mutex<Ledger>,
}

impl McpHttpTransport {
    pub async fn connect(url: &str, headers: HashMap<String, String>) -> Result<Self, McpError> {
        // Fail at connect time on a non-http(s) or unparseable URL instead of
        // at first call.
        client_shared::validate_http_url(url).map_err(McpError::transport)?;
        let client = client_shared::build_http_client(false).map_err(McpError::transport)?;
        let (stream_stop, _) = tokio::sync::watch::channel(());
        let closed = Arc::new(AtomicBool::new(false));
        let stream: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
        // Registered in reverse execution order: the ledger drains backwards,
        // so the transport is marked closed first, then the stream is
        // signalled, then its task is aborted.
        let mut dispose = Ledger::new("mcp-http-transport");
        {
            let stream = Arc::clone(&stream);
            dispose
                .register_labeled(
                    "abort-stream",
                    Disposer::new(move |_| {
                        Box::pin(async move {
                            if let Some(handle) = stream.lock().await.take() {
                                handle.abort();
                            }
                            Ok(())
                        })
                    }),
                )
                .expect("a fresh ledger accepts registrations");
        }
        {
            let stop = stream_stop.clone();
            dispose
                .register_labeled(
                    "stop-stream",
                    Disposer::new(move |_| {
                        Box::pin(async move {
                            let _ = stop.send(());
                            Ok(())
                        })
                    }),
                )
                .expect("a fresh ledger accepts registrations");
        }
        {
            let closed = Arc::clone(&closed);
            dispose
                .register_labeled(
                    "mark-closed",
                    Disposer::new(move |_| {
                        Box::pin(async move {
                            closed.store(true, Ordering::SeqCst);
                            Ok(())
                        })
                    }),
                )
                .expect("a fresh ledger accepts registrations");
        }
        Ok(Self {
            url: url.to_string(),
            headers,
            client,
            session_id: Arc::new(Mutex::new(None)),
            next_id: AtomicU64::new(1),
            request_timeout: None,
            closed,
            weak_evidence: Arc::new(Mutex::new(None)),
            pending_weak_evidence: Arc::new(Mutex::new(None)),
            stream,
            stream_stop,
            dispose: Mutex::new(dispose),
        })
    }

    /// Register the weak-evidence listener; a signal buffered before
    /// registration is replayed so it is never dropped.
    pub async fn on_weak_evidence(&self, listener: Box<dyn Fn(String) + Send + Sync>) {
        let pending = self.pending_weak_evidence.lock().await.take();
        if let Some(reason) = pending {
            listener(reason);
            return;
        }
        *self.weak_evidence.lock().await = Some(listener);
    }

    /// Apply a per-request timeout (v2 `buildRequestOptions(toolCallTimeoutMs)`).
    pub fn set_request_timeout(&mut self, timeout: Option<Duration>) {
        self.request_timeout = timeout;
    }

    /// Mark the transport closed. In-flight POSTs finish their own round trip;
    /// the standalone GET stream is aborted and no new request may start.
    pub async fn shutdown(&self) {
        self.dispose
            .lock()
            .await
            .teardown(TeardownReason::ScopeClose)
            .await;
    }

    /// Open the standalone GET SSE stream (v2 `_startOrAuthSse`,
    /// streamableHttp.js:78-112), at most once per transport.
    async fn start_stream(&self) {
        let mut slot = self.stream.lock().await;
        if slot.is_some() {
            return;
        }
        *slot = Some(tokio::spawn(run_sse_stream(StreamContext {
            url: self.url.clone(),
            headers: self.headers.clone(),
            http: self.client.clone(),
            session_id: self.session_id.clone(),
            closed: self.closed.clone(),
            stop: self.stream_stop.subscribe(),
            weak_evidence: self.weak_evidence.clone(),
            pending: self.pending_weak_evidence.clone(),
        })));
    }

    fn ensure_open(&self) -> Result<(), McpError> {
        if self.closed.load(Ordering::SeqCst) {
            Err(McpError::closed("MCP HTTP transport is closed"))
        } else {
            Ok(())
        }
    }

    /// POST one JSON-RPC envelope, recording the session id and bounding the
    /// round trip with the caller's deadline.
    async fn post(&self, payload: &Value, budget: Budget) -> Result<reqwest::Response, McpError> {
        let mut req = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(session) = self.session_id.lock().await.clone() {
            req = req.header(SESSION_ID_HEADER, session);
        }

        let resp = match tokio::time::timeout_at(budget.deadline, req.json(payload).send()).await {
            Ok(send_result) => send_result.map_err(|e| {
                McpError::transport(format!(
                    "Failed to post to MCP HTTP endpoint '{}': {e}",
                    self.url
                ))
            })?,
            Err(_) => return Err(McpError::timeout(timeout_message(budget.wait))),
        };

        if let Some(session) = resp
            .headers()
            .get(SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().await = Some(session.to_string());
        }
        Ok(resp)
    }

    /// POST one JSON-RPC message and return its `result`.
    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        self.ensure_open()?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let budget = Budget::for_request(self.request_timeout);
        let resp = self.post(&payload, budget).await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(McpError::http_status(
                status.as_u16(),
                format!("MCP HTTP request failed with status HTTP {status}"),
            ));
        }

        let is_event_stream = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
        if is_event_stream {
            read_sse_response(resp, id, budget).await
        } else {
            let body = match tokio::time::timeout_at(budget.deadline, resp.json::<Value>()).await {
                Ok(parse_result) => parse_result.map_err(|e| {
                    McpError::transport(format!("Failed to parse MCP HTTP response: {e}"))
                })?,
                Err(_) => return Err(McpError::timeout(timeout_message(budget.wait))),
            };
            unwrap_json_rpc(body)
        }
    }

    /// POST a JSON-RPC notification (no id). Streamable HTTP servers answer
    /// these with 2xx (often 202 Accepted with an empty body); the body is
    /// intentionally not read as a JSON-RPC response.
    pub async fn send_notification(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.ensure_open()?;
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let budget = Budget::for_request(self.request_timeout);
        let resp = self.post(&payload, budget).await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(McpError::http_status(
                status.as_u16(),
                format!("MCP notification failed with status HTTP {status}"),
            ));
        }
        // v2 starts the standalone stream only when the server accepts the
        // `initialized` notification with 202 Accepted
        // (streamableHttp.js:370-377); any other 2xx means no server stream.
        if method == "notifications/initialized" && status.as_u16() == 202 {
            self.start_stream().await;
        }
        Ok(())
    }
}

/// v2 SDK `StreamableHTTPClientTransport` reconnection defaults
/// (streamableHttp.js:6-11): 1 s initial, ×1.5 per attempt, 30 s cap, 2
/// retries. Exhausting them is what the manager sees as weak evidence
/// (`Maximum reconnection attempts (N) exceeded.`).
const STREAM_INITIAL_DELAY_MS: u64 = 1_000;
const STREAM_DELAY_GROW_FACTOR: f64 = 1.5;
const STREAM_MAX_DELAY_MS: u64 = 30_000;
const STREAM_MAX_RETRIES: u32 = 2;

fn stream_retry_delay(attempt: u32) -> Duration {
    let delay = STREAM_INITIAL_DELAY_MS as f64 * STREAM_DELAY_GROW_FACTOR.powi(attempt as i32);
    Duration::from_millis((delay.min(STREAM_MAX_DELAY_MS as f64)) as u64)
}

/// Everything the stream task needs, cloned out of the transport when the
/// stream starts.
struct StreamContext {
    url: String,
    headers: HashMap<String, String>,
    http: reqwest::Client,
    session_id: Arc<Mutex<Option<String>>>,
    closed: Arc<AtomicBool>,
    stop: tokio::sync::watch::Receiver<()>,
    weak_evidence: WeakEvidenceListener,
    pending: Arc<Mutex<Option<String>>>,
}

/// Read the standalone GET SSE stream, reconnecting on a drop with the SDK's
/// backoff and giving up with weak evidence once the attempts are exhausted
/// (v2 `_handleSseStream` + `_scheduleReconnection`,
/// streamableHttp.js:120-245). A 405 means the server does not offer the
/// optional stream at all, which is not an error.
async fn run_sse_stream(mut context: StreamContext) {
    let mut attempts: u32 = 0;
    let mut last_event_id: Option<String> = None;
    loop {
        let mut request = context
            .http
            .get(&context.url)
            .header("Accept", "text/event-stream")
            .header("mcp-protocol-version", STREAMABLE_HTTP_PROTOCOL_VERSION);
        for (key, value) in &context.headers {
            request = request.header(key, value);
        }
        if let Some(session) = context.session_id.lock().await.clone() {
            request = request.header(SESSION_ID_HEADER, session);
        }
        if let Some(id) = last_event_id.as_deref() {
            request = request.header("last-event-id", id);
        }

        match request.send().await {
            Ok(resp) if resp.status().as_u16() == 405 => return,
            Ok(resp) if resp.status().is_success() => {
                // A live stream resets the counter, exactly as the SDK does by
                // restarting at attempt 0 after every successful open.
                attempts = 0;
                let mut stream = resp.bytes_stream().eventsource();
                loop {
                    let item = tokio::select! {
                        _ = context.stop.changed() => return,
                        item = stream.next() => item,
                    };
                    match item {
                        Some(Ok(event)) => {
                            if !event.id.is_empty() {
                                last_event_id = Some(event.id.clone());
                            }
                            if event.data.is_empty() {
                                continue;
                            }
                            if !event.event.is_empty() && event.event != "message" {
                                continue;
                            }
                            reply_to_server_message(
                                &context.http,
                                &context.url,
                                &context.headers,
                                &context.session_id,
                                &event.data,
                            )
                            .await;
                        }
                        Some(Err(_)) | None => break,
                    }
                }
            }
            Ok(_) | Err(_) => {}
        }

        if context.closed.load(Ordering::SeqCst) {
            return;
        }
        if attempts >= STREAM_MAX_RETRIES {
            fire_or_buffer_weak_evidence(
                &context.weak_evidence,
                &context.pending,
                format!("Maximum reconnection attempts ({STREAM_MAX_RETRIES}) exceeded."),
            )
            .await;
            return;
        }
        tokio::select! {
            _ = context.stop.changed() => return,
            _ = tokio::time::sleep(stream_retry_delay(attempts)) => {}
        }
        attempts += 1;
    }
}

/// Answer a server-initiated request on the GET stream: `ping` with an empty
/// result, anything else with JSON-RPC "method not found" — the same answers
/// the SDK client gives when no handler is registered for the method.
async fn reply_to_server_message(
    http: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
    session_id: &Arc<Mutex<Option<String>>>,
    data: &str,
) {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return;
    };
    let (Some(id), Some(method)) = (
        value.get("id").cloned(),
        value.get("method").and_then(|m| m.as_str()),
    ) else {
        return;
    };
    let payload = if method == "ping" {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "Method not found" },
        })
    };
    let mut request = http
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    for (key, value) in headers {
        request = request.header(key, value);
    }
    if let Some(session) = session_id.lock().await.clone() {
        request = request.header(SESSION_ID_HEADER, session);
    }
    // Fire and forget: a slow reply must not stall reading the stream.
    tokio::spawn(async move {
        if let Ok(resp) = request.json(&payload).send().await {
            let _ = resp.bytes().await;
        }
    });
}

fn timeout_message(wait: Duration) -> String {
    format!("MCP request timed out after {}ms", wait.as_millis())
}

/// Read the SSE stream until the message answering `id` arrives; unrelated
/// notifications are ignored, mirroring the SDK transport.
async fn read_sse_response(
    resp: reqwest::Response,
    id: u64,
    budget: Budget,
) -> Result<Value, McpError> {
    let mut stream = resp.bytes_stream().eventsource();
    loop {
        let item = tokio::time::timeout_at(budget.deadline, stream.next())
            .await
            .map_err(|_| McpError::timeout(timeout_message(budget.wait)))?;
        let Some(item) = item else {
            return Err(McpError::closed(
                "MCP HTTP SSE stream closed before a response arrived",
            ));
        };
        let event = item.map_err(|e| McpError::transport(format!("MCP HTTP SSE error: {e}")))?;
        if !event.event.is_empty() && event.event != "message" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
            return unwrap_json_rpc(value);
        }
    }
}

fn unwrap_json_rpc(value: Value) -> Result<Value, McpError> {
    if let Some(err) = value.get("error") {
        return Err(McpError::json_rpc(format!("MCP Server Error: {err}")));
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    /// One recorded request, so tests can assert header propagation and the
    /// initialize/initialized request sequence.
    #[derive(Debug, Clone)]
    pub struct RecordedRequest {
        pub method: String,
        pub has_id: bool,
        pub session: Option<String>,
        pub authorization: Option<String>,
    }

    /// Spawn a mock Streamable HTTP server.
    ///
    /// `mode` selects the response framing: `"json"` replies with a JSON body,
    /// `"sse"` replies with an SSE stream, `"status"` fails every request with
    /// HTTP 500, `"hang"` accepts the connection without ever replying,
    /// `"bad-schema"` advertises a tool whose inputSchema is not an object,
    /// `"401-on-call"` completes the handshake normally but rejects every
    /// `tools/call` with HTTP 401, and `"image"` answers `tools/call` with an
    /// image content block instead of text.
    ///
    /// The returned vector records every request in arrival order.
    pub async fn spawn_mock_http_server(
        mode: &'static str,
    ) -> (
        String,
        Arc<Mutex<Vec<RecordedRequest>>>,
        oneshot::Sender<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/mcp");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_task = seen.clone();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    res = listener.accept() => {
                        let Ok((mut socket, _)) = res else { break };
                        let seen = seen_task.clone();
                        tokio::spawn(async move {
                            if mode == "hang" {
                                tokio::time::sleep(Duration::from_secs(30)).await;
                                return;
                            }
                            let mut buf = vec![0u8; 8192];
                            let mut total = 0;
                            let pos = loop {
                                let n = match socket.read(&mut buf[total..]).await {
                                    Ok(0) | Err(_) => return,
                                    Ok(n) => n,
                                };
                                total += n;
                                if let Some(pos) =
                                    buf[..total].windows(4).position(|w| w == b"\r\n\r\n")
                                {
                                    break pos;
                                }
                                if total == buf.len() {
                                    return;
                                }
                            };

                            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                            let mut content_len = 0usize;
                            let mut session: Option<String> = None;
                            let mut authorization: Option<String> = None;
                            for line in head.lines() {
                                if let Some((k, v)) = line.split_once(':') {
                                    let key = k.trim().to_ascii_lowercase();
                                    if key == "content-length" {
                                        content_len = v.trim().parse().unwrap_or(0);
                                    }
                                    if key == SESSION_ID_HEADER {
                                        session = Some(v.trim().to_string());
                                    }
                                    if key == "authorization" {
                                        authorization = Some(v.trim().to_string());
                                    }
                                }
                            }
                            let mut body = buf[pos + 4..total].to_vec();
                            while body.len() < content_len {
                                let mut chunk = vec![0u8; content_len - body.len()];
                                match socket.read(&mut chunk).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => body.extend_from_slice(&chunk[..n]),
                                }
                            }
                            if mode == "status" {
                                let _ = socket
                                    .write_all(b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\n\r\n")
                                    .await;
                                return;
                            }
                            // The standalone GET SSE stream (`_startOrAuthSse`):
                            // a server that does not offer it answers 405, which
                            // the transport treats as "no stream" rather than as
                            // evidence. `sse-500` refuses it repeatedly so the
                            // reconnection ladder runs out; `sse-open` keeps a
                            // live, empty stream open.
                            if head.starts_with("GET ") {
                                match mode {
                                    "sse-500" => {
                                        let _ = socket
                                            .write_all(
                                                b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\n\r\n",
                                            )
                                            .await;
                                    }
                                    "sse-open" => {
                                        let _ = socket
                                            .write_all(
                                                b"HTTP/1.1 200 OK\r\n\
                                                  Content-Type: text/event-stream\r\n\r\n",
                                            )
                                            .await;
                                        let mut discard = [0u8; 256];
                                        while let Ok(n) = socket.read(&mut discard).await {
                                            if n == 0 {
                                                break;
                                            }
                                        }
                                    }
                                    _ => {
                                        let _ = socket
                                            .write_all(
                                                b"HTTP/1.1 405 Method Not Allowed\r\n\
                                                  Content-Length: 0\r\n\r\n",
                                            )
                                            .await;
                                    }
                                }
                                return;
                            }
                            if mode == "401" {
                                let _ = socket
                                    .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                                    .await;
                                return;
                            }

                            let json: Value =
                                serde_json::from_slice(&body).unwrap_or(Value::Null);
                            let id = json.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                            let method = json
                                .get("method")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            seen.lock().await.push(RecordedRequest {
                                method: method.to_string(),
                                has_id: json.get("id").is_some(),
                                session,
                                authorization,
                            });

                            // Notifications (e.g. notifications/initialized)
                            // get a 202 with no body.
                            if json.get("id").is_none() {
                                let _ = socket
                                    .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                                    .await;
                                return;
                            }
                            if mode == "401-on-call" && method == "tools/call" {
                                let _ = socket
                                    .write_all(
                                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
                                    )
                                    .await;
                                return;
                            }
                            // v2 `withTimeout` wraps connect **and** discovery
                            // (connection-manager.ts:594-611): a handshake that
                            // answers but never lists tools must still hit the
                            // startup budget.
                            if mode == "hang-tools" && method == "tools/list" {
                                tokio::time::sleep(Duration::from_secs(30)).await;
                                return;
                            }
                            let result = match method {
                                "initialize" => json!({
                                    "protocolVersion": STREAMABLE_HTTP_PROTOCOL_VERSION,
                                    "capabilities": { "tools": {} },
                                    "serverInfo": { "name": "mock-http-server", "version": "1.0.0" }
                                }),
                                "tools/list" => {
                                    if mode == "bad-schema" {
                                        json!({
                                            "tools": [
                                                {
                                                    "name": "echo",
                                                    "description": "Echo input message",
                                                    "inputSchema": "not-an-object"
                                                }
                                            ]
                                        })
                                    } else {
                                        json!({
                                            "tools": [
                                                {
                                                    "name": "echo",
                                                    "description": "Echo input message",
                                                    "inputSchema": {
                                                        "type": "object",
                                                        "properties": { "message": { "type": "string" } }
                                                    }
                                                }
                                            ]
                                        })
                                    }
                                }
                                "tools/call" => {
                                    if mode == "image" {
                                        json!({
                                            "content": [{
                                                "type": "image",
                                                "mimeType": "image/png",
                                                "data": "iVBORw0KGgo="
                                            }],
                                            "isError": false
                                        })
                                    } else {
                                        json!({
                                            "content": [{ "type": "text", "text": "ok" }],
                                            "isError": false
                                        })
                                    }
                                }
                                _ => json!({}),
                            };
                            let payload =
                                json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
                            // A handshake slow enough that a removal can land
                            // between "attempt started" and "handshake done"
                            // (v2's in-flight-invalidation case).
                            if mode == "slow-initialize" && method == "initialize" {
                                tokio::time::sleep(Duration::from_millis(300)).await;
                            }
                            let session_header = if method == "initialize" {
                                "Mcp-Session-Id: sess-1\r\n"
                            } else {
                                ""
                            };
                            let resp = if mode == "sse" {
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                                     {session_header}Connection: close\r\n\r\n\
                                     event: message\ndata: {payload}\n\n"
                                )
                            } else {
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                     {session_header}Connection: close\r\n\
                                     Content-Length: {}\r\n\r\n{payload}",
                                    payload.len()
                                )
                            };
                            let _ = socket.write_all(resp.as_bytes()).await;
                            let _ = socket.flush().await;
                        });
                    }
                }
            }
        });

        (url, seen, shutdown_tx)
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::spawn_mock_http_server;
    use super::*;
    use crate::mcp::client::McpClient;
    use serde_json::json;

    /// The close path owns every step exactly once: a second shutdown must not
    /// re-run the stream abort / stop signal and must stay closed.
    #[tokio::test]
    async fn shutdown_is_idempotent_and_marks_closed() {
        let transport = McpHttpTransport::connect("http://127.0.0.1:1/mcp", HashMap::new())
            .await
            .ok()
            .expect("transport");
        assert!(!transport.closed.load(Ordering::SeqCst));
        transport.shutdown().await;
        assert!(transport.closed.load(Ordering::SeqCst));
        transport.shutdown().await;
        assert!(transport.closed.load(Ordering::SeqCst));
        assert_eq!(
            transport.dispose.lock().await.state(),
            crate::primitives::teardown::LedgerState::Disposed
        );
    }

    /// A JSON-framed reply is unwrapped and the session id is echoed.
    #[tokio::test]
    async fn test_http_transport_json_roundtrip() {
        let (url, seen, _shutdown) = spawn_mock_http_server("json").await;
        let client = McpClient::connect_http("http-srv", &url, HashMap::new(), None)
            .await
            .expect("HTTP connect failed");
        assert_eq!(client.transport_type(), "http");

        let tools = client.list_tools().await.expect("list_tools failed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let res = client
            .call_tool("echo", &json!({ "message": "hi" }))
            .await
            .expect("call_tool failed");
        assert!(!res.is_error);
        assert_eq!(res.content[0].text.as_deref(), Some("ok"));

        // initialize carried no session id; the initialized notification and
        // every later request echo the one the server handed back.
        let requests = seen.lock().await.clone();
        assert!(requests.iter().any(|r| r.method == "initialize"));
        assert!(
            requests
                .iter()
                .any(|r| r.method == "notifications/initialized")
        );
        assert!(requests.iter().any(|r| r.method == "tools/list"));
        assert!(requests.iter().any(|r| r.method == "tools/call"));
        let initialize = requests
            .iter()
            .position(|r| r.method == "initialize")
            .unwrap();
        let initialized = requests
            .iter()
            .position(|r| r.method == "notifications/initialized")
            .unwrap();
        assert!(initialize < initialized);
        assert_eq!(requests[initialize].session, None);
        assert_eq!(requests[initialized].session.as_deref(), Some("sess-1"));
        assert_eq!(
            requests
                .iter()
                .find(|r| r.method == "tools/list")
                .unwrap()
                .session
                .as_deref(),
            Some("sess-1")
        );
    }

    /// An SSE-framed reply is read until the matching message arrives.
    #[tokio::test]
    async fn test_http_transport_sse_roundtrip() {
        let (url, _seen, _shutdown) = spawn_mock_http_server("sse").await;
        let client = McpClient::connect_http("http-sse", &url, HashMap::new(), None)
            .await
            .expect("HTTP connect failed");

        let tools = client.list_tools().await.expect("list_tools failed");
        assert_eq!(tools[0].name, "echo");
        let res = client
            .call_tool("echo", &json!({}))
            .await
            .expect("call_tool failed");
        assert_eq!(res.content[0].text.as_deref(), Some("ok"));
    }

    /// A non-2xx reply surfaces the status instead of a parse error.
    #[tokio::test]
    async fn test_http_transport_status_error() {
        let (url, _seen, _shutdown) = spawn_mock_http_server("status").await;
        let err = match McpClient::connect_http("http-fail", &url, HashMap::new(), None).await {
            Ok(_) => panic!("500 must fail the handshake"),
            Err(e) => e,
        };
        let message = err.to_string();
        assert!(
            message.contains("MCP HTTP request failed with status HTTP 500"),
            "unexpected error: {message}"
        );
        assert!(matches!(err, McpError::HttpStatus { status: 500, .. }));
    }

    /// A server that accepts the connection but never replies must fail with
    /// the resolved per-request timeout.
    #[tokio::test]
    async fn test_http_transport_timeout() {
        let (url, _seen, _shutdown) = spawn_mock_http_server("hang").await;
        let mut transport = McpHttpTransport::connect(&url, HashMap::new())
            .await
            .expect("transport build failed");
        transport.set_request_timeout(Some(Duration::from_millis(150)));
        let err = transport
            .send_request("initialize", json!({}))
            .await
            .expect_err("request must time out");
        assert_eq!(err.to_string(), "MCP request timed out after 150ms");
        assert!(matches!(err, McpError::Timeout(_)));
    }

    /// A malformed URL fails at connect time.
    #[tokio::test]
    async fn test_http_transport_rejects_invalid_url() {
        let err = match McpHttpTransport::connect("not-a-url", HashMap::new()).await {
            Ok(_) => panic!("invalid url must fail"),
            Err(e) => e,
        };
        assert!(
            err.to_string().starts_with("Invalid MCP url 'not-a-url'"),
            "{err}"
        );

        let err = match McpHttpTransport::connect("file:///etc/passwd", HashMap::new()).await {
            Ok(_) => panic!("file url must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("unsupported scheme"), "{err}");
    }

    /// The handshake is `initialize` (request, with id) immediately followed
    /// by `notifications/initialized` (notification, no id).
    #[tokio::test]
    async fn test_http_handshake_sends_initialized_notification() {
        let (url, seen, _shutdown) = spawn_mock_http_server("json").await;
        let _client = McpClient::connect_http("http-srv", &url, HashMap::new(), None)
            .await
            .expect("HTTP connect failed");

        let requests = seen.lock().await.clone();
        let init = requests
            .iter()
            .find(|r| r.method == "initialize")
            .expect("initialize request recorded");
        assert!(init.has_id, "initialize must carry an id");
        let notification = requests
            .iter()
            .find(|r| r.method == "notifications/initialized")
            .expect("initialized notification recorded");
        assert!(
            !notification.has_id,
            "notifications/initialized must not carry an id"
        );
        assert!(
            requests.iter().position(|r| r.method == "initialize")
                < requests
                    .iter()
                    .position(|r| r.method == "notifications/initialized"),
            "initialize must precede the initialized notification"
        );
    }

    /// v2 `_scheduleReconnection` (streamableHttp.js:139-160): the standalone
    /// GET stream reconnects twice (1 s, then 1.5 s) and, when it still cannot
    /// get through, reports weak evidence — the signal that parks the entry in
    /// `probing` instead of failing it outright.
    #[tokio::test]
    async fn test_http_stream_reports_weak_evidence_when_reconnection_gives_up() {
        let (url, _seen, _shutdown) = spawn_mock_http_server("sse-500").await;
        let client = McpClient::connect_http("weak", &url, HashMap::new(), None)
            .await
            .expect("HTTP connect failed");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        client
            .on_weak_evidence(Box::new(move |reason| {
                let _ = tx.send(reason);
            }))
            .await;

        let reason = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the reconnection ladder gives up")
            .expect("a reason is reported");
        assert_eq!(reason, "Maximum reconnection attempts (2) exceeded.");
        client.close().await;
    }

    /// A server that does not offer the optional stream answers 405, which is
    /// not evidence of a dead session: no weak signal, no retries.
    #[tokio::test]
    async fn test_http_stream_absent_is_not_weak_evidence() {
        let (url, _seen, _shutdown) = spawn_mock_http_server("json").await;
        let client = McpClient::connect_http("no-stream", &url, HashMap::new(), None)
            .await
            .expect("HTTP connect failed");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        client
            .on_weak_evidence(Box::new(move |reason| {
                let _ = tx.send(reason);
            }))
            .await;

        assert!(
            tokio::time::timeout(Duration::from_millis(700), rx.recv())
                .await
                .is_err(),
            "a 405 must not be reported as weak evidence"
        );
        client.close().await;
    }

    /// While the GET stream is open the session is healthy: no weak signal is
    /// reported, and closing the client ends the stream task silently.
    #[tokio::test]
    async fn test_http_stream_keeps_a_live_session_silent() {
        let (url, _seen, _shutdown) = spawn_mock_http_server("sse-open").await;
        let client = McpClient::connect_http("streaming", &url, HashMap::new(), None)
            .await
            .expect("HTTP connect failed");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        client
            .on_weak_evidence(Box::new(move |reason| {
                let _ = tx.send(reason);
            }))
            .await;

        assert!(
            tokio::time::timeout(Duration::from_millis(1200), rx.recv())
                .await
                .is_err(),
            "an open stream is not evidence of a dead session"
        );
        client.close().await;
    }
}
