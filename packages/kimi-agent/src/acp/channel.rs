//! Outbound ACP messages and the server→client request back-channel.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::acp::types::{JsonRpcRequest, JsonRpcResponse};
use crate::primitives::deadline::Deadline;

/// One outbound JSON-RPC message.
#[derive(Debug, Clone)]
pub enum AcpOutbound {
    /// Answer to a client request.
    Response(JsonRpcResponse),
    /// Server-initiated notification (`session/update`).
    Notification(JsonRpcRequest),
    /// Server-initiated request (`session/request_permission`); the client's
    /// answer returns through [`AcpChannel::resolve`].
    Request(JsonRpcRequest),
}

type Sink = Arc<std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<AcpOutbound>>>>;

/// Outbound channel shared by the dispatcher, the stdio writer loop and the
/// permission bridge.
#[derive(Clone, Default)]
pub struct AcpChannel {
    sink: Sink,
    next_id: Arc<AtomicU64>,
    waiters: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
}

impl AcpChannel {
    pub fn new() -> Self {
        Self {
            sink: Arc::new(std::sync::RwLock::new(None)),
            next_id: Arc::new(AtomicU64::new(1)),
            waiters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Install the writer sink.
    pub fn set_sink(&self, sender: tokio::sync::mpsc::UnboundedSender<AcpOutbound>) {
        *self.sink.write().unwrap_or_else(|e| e.into_inner()) = Some(sender);
    }

    /// Drop the writer sink so the writer loop can end.
    pub fn clear_sink(&self) {
        *self.sink.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn send(&self, message: AcpOutbound) -> bool {
        let guard = self.sink.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(sender) => sender.send(message).is_ok(),
            None => false,
        }
    }

    /// Fire-and-forget notification.
    pub fn notify(&self, method: &str, params: serde_json::Value) {
        self.send(AcpOutbound::Notification(JsonRpcResponse::notification(
            method, params,
        )));
    }

    /// Send a request and wait for the client's response.
    ///
    /// Unbounded: the reference has no generic timeout for a host round-trip,
    /// and only the caller knows whether this particular one has a bound.
    /// Human-answered requests use [`Self::request_human`] instead.
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.request_with(method, params, None, None).await
    }

    /// A server→client request answered by a human (approval, question).
    ///
    /// Unbounded like every other host round-trip; `cancel` lets a cancelled
    /// turn stop waiting immediately.
    pub async fn request_human(
        &self,
        method: &str,
        params: serde_json::Value,
        cancel: Option<&CancellationToken>,
    ) -> Result<serde_json::Value, String> {
        self.request_with(method, params, None, cancel).await
    }

    /// Send a request and wait at most `limit` for the answer, giving up
    /// immediately when `cancel` fires.
    pub async fn request_with(
        &self,
        method: &str,
        params: serde_json::Value,
        limit: Option<Duration>,
        cancel: Option<&CancellationToken>,
    ) -> Result<serde_json::Value, String> {
        enum Waited {
            Answer(Result<serde_json::Value, oneshot::error::RecvError>),
            Cancelled,
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, mut rx) = oneshot::channel();
        self.waiters.lock().await.insert(id, tx);
        let sent = self.send(AcpOutbound::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(id)),
            method: method.to_string(),
            params: Some(params),
        }));
        if !sent {
            self.waiters.lock().await.remove(&id);
            return Err("ACP client is not connected".into());
        }

        let deadline = match limit {
            Some(limit) => Deadline::new(method.to_string(), limit),
            None => Deadline::unbounded(method.to_string()),
        };
        let outcome = deadline
            .wait("waiting for the ACP client to answer", async {
                match cancel {
                    Some(token) => {
                        tokio::select! {
                            _ = token.cancelled() => Waited::Cancelled,
                            answer = &mut rx => Waited::Answer(answer),
                        }
                    }
                    None => Waited::Answer((&mut rx).await),
                }
            })
            .await;

        let value = match outcome {
            Err(_elapsed) => {
                // Drop the waiter: a late answer has nowhere to land, and the
                // request is already reported as timed out.
                self.waiters.lock().await.remove(&id);
                return Err(format!(
                    "{method} timed out after {}s waiting for the ACP client",
                    deadline.remaining().as_secs()
                ));
            }
            Ok(Waited::Cancelled) => {
                self.waiters.lock().await.remove(&id);
                return Err(format!("{method} cancelled before the ACP client answered"));
            }
            Ok(Waited::Answer(Err(_))) => {
                return Err("ACP client disconnected before answering".into());
            }
            Ok(Waited::Answer(Ok(value))) => value,
        };

        if let Some(result) = value.get("result") {
            return Ok(result.clone());
        }
        if let Some(error) = value.get("error") {
            let code = error
                .get("code")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(-32603);
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Unknown client error");
            return Err(format!("ACP client error ({code}): {message}"));
        }
        Err("Invalid ACP response: expected result or error".to_string())
    }

    /// Stop sending and fail every pending server→client request.
    ///
    /// Called when stdin reaches EOF: the client can no longer answer, so a
    /// waiter must resolve now rather than wait out its bound. Dropping the
    /// senders wakes each `request` with "client disconnected".
    pub async fn close(&self) {
        self.clear_sink();
        let mut waiters = self.waiters.lock().await;
        let count = waiters.len();
        waiters.clear();
        if count > 0 {
            tracing::warn!(
                count,
                "the ACP client channel closed with requests still in flight"
            );
        }
    }

    /// Resolve a pending server→client request; returns false for an unknown
    /// or already-resolved id.
    pub async fn resolve(&self, id: u64, value: serde_json::Value) -> bool {
        let waiter = { self.waiters.lock().await.remove(&id) };
        match waiter {
            Some(tx) => tx.send(value).is_ok(),
            None => false,
        }
    }

    // ── Reverse RPC: agent → client (fs / terminal) ─────────────────────
    //
    // These mirror the ACP spec's client-side methods. They are thin wrappers
    // over [`Self::request`]; the caller decides whether the client advertised
    // the matching capability (see `AcpClientCapabilities`).

    /// `fs/read_text_file`: the client reads the file on the agent's behalf.
    pub async fn read_text_file(&self, session_id: &str, path: &str) -> Result<String, String> {
        let result = self
            .request(
                "fs/read_text_file",
                serde_json::json!({ "sessionId": session_id, "path": path }),
            )
            .await?;
        result
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "fs/read_text_file: response is missing `content`".to_string())
    }

    /// `fs/write_text_file`: the client writes the file on the agent's behalf.
    pub async fn write_text_file(
        &self,
        session_id: &str,
        path: &str,
        content: &str,
    ) -> Result<(), String> {
        self.request(
            "fs/write_text_file",
            serde_json::json!({
                "sessionId": session_id,
                "path": path,
                "content": content,
            }),
        )
        .await
        .map(|_| ())
    }

    /// `terminal/create`: the client spawns the terminal and returns its id.
    /// `env` and `output_byte_limit` are the ACP `env` / `outputByteLimit`
    /// fields — the client's terminal is a different process tree, so the
    /// engine's spawn environment and output budget have to travel with the
    /// request (v2 `AcpProcessService.spawn`, acpTerminalRunner.ts:52-70).
    pub async fn create_terminal(
        &self,
        session_id: &str,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        output_byte_limit: usize,
    ) -> Result<String, String> {
        let env: Vec<serde_json::Value> = env
            .iter()
            .map(|(name, value)| serde_json::json!({ "name": name, "value": value }))
            .collect();
        let result = self
            .request(
                "terminal/create",
                serde_json::json!({
                    "sessionId": session_id,
                    "command": command,
                    "args": args,
                    "env": env,
                    "cwd": cwd,
                    "outputByteLimit": output_byte_limit,
                }),
            )
            .await?;
        result
            .get("terminalId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "terminal/create: response is missing `terminalId`".to_string())
    }

    /// `terminal/output`: the terminal's buffered output so far.
    pub async fn terminal_output(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Result<String, String> {
        let result = self
            .request(
                "terminal/output",
                serde_json::json!({
                    "sessionId": session_id,
                    "terminalId": terminal_id,
                }),
            )
            .await?;
        Ok(result
            .get("output")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// `terminal/wait_for_exit`: blocks until the terminal exits. Unbounded:
    /// the reference has no generic bound for a host round-trip.
    pub async fn wait_for_terminal_exit(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Result<serde_json::Value, String> {
        self.request_with(
            "terminal/wait_for_exit",
            serde_json::json!({
                "sessionId": session_id,
                "terminalId": terminal_id,
            }),
            None,
            None,
        )
        .await
    }

    /// `terminal/kill`: terminate the terminal's command.
    pub async fn kill_terminal(&self, session_id: &str, terminal_id: &str) -> Result<(), String> {
        self.request(
            "terminal/kill",
            serde_json::json!({
                "sessionId": session_id,
                "terminalId": terminal_id,
            }),
        )
        .await
        .map(|_| ())
    }

    /// `terminal/release`: let the client reclaim the terminal.
    pub async fn release_terminal(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Result<(), String> {
        self.request(
            "terminal/release",
            serde_json::json!({
                "sessionId": session_id,
                "terminalId": terminal_id,
            }),
        )
        .await
        .map(|_| ())
    }

    /// Extract the id of a client response line (`{"id":N,"result"|"error":…}`).
    pub fn response_id(value: &serde_json::Value) -> Option<u64> {
        if value.get("method").is_some() {
            return None;
        }
        value.get("id").and_then(|id| id.as_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_request_resolves_with_client_answer() {
        let channel = AcpChannel::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        channel.set_sink(tx);

        let pending = {
            let channel = channel.clone();
            tokio::spawn(async move {
                channel
                    .request(
                        "session/request_permission",
                        serde_json::json!({ "sessionId": "s1" }),
                    )
                    .await
            })
        };

        let outbound = rx.recv().await.expect("one outbound message");
        let id = match outbound {
            AcpOutbound::Request(request) => request.id.unwrap().as_u64().unwrap(),
            other => panic!("unexpected message: {other:?}"),
        };
        assert!(
            channel
                .resolve(
                    id,
                    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": { "outcome": "cancelled" } } })
                )
                .await
        );

        let answer = pending.await.unwrap().expect("request resolves");
        assert_eq!(answer["outcome"]["outcome"], "cancelled");
        assert!(!channel.resolve(id, serde_json::json!({})).await);
    }

    #[tokio::test]
    async fn client_responses_are_unwrapped_through_dispatcher() {
        for (body, expected) in [
            (
                serde_json::json!({ "result": { "content": "alpha" } }),
                Ok(serde_json::json!({ "content": "alpha" })),
            ),
            (
                serde_json::json!({ "result": null }),
                Ok(serde_json::Value::Null),
            ),
            (
                serde_json::json!({ "error": { "code": -32603, "message": "client failed" } }),
                Err("ACP client error (-32603): client failed".to_string()),
            ),
            (
                serde_json::json!({ "result": { "content": "alpha" }, "error": { "code": -32603, "message": "client failed" } }),
                Ok(serde_json::json!({ "content": "alpha" })),
            ),
            (
                serde_json::json!({}),
                Err("Invalid ACP response: expected result or error".to_string()),
            ),
        ] {
            let server = crate::acp::AcpServer::in_memory().unwrap();
            let channel = server.channel();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            server.set_notification_sink(tx);
            let pending = tokio::spawn(async move {
                channel
                    .request(
                        "fs/read_text_file",
                        serde_json::json!({ "sessionId": "s1", "path": "/work/example.txt" }),
                    )
                    .await
            });
            let AcpOutbound::Request(request) = rx.recv().await.unwrap() else {
                panic!("expected reverse request");
            };
            let mut response = body;
            response["jsonrpc"] = serde_json::json!("2.0");
            response["id"] = request.id.unwrap();
            assert!(server.handle_line(&response.to_string()).await.is_none());
            assert_eq!(pending.await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn test_request_without_client_errors() {
        let channel = AcpChannel::new();
        let err = channel
            .request("session/request_permission", serde_json::json!({}))
            .await
            .expect_err("no sink means no client");
        assert_eq!(err, "ACP client is not connected");
    }

    #[tokio::test]
    async fn a_client_that_never_answers_times_out() {
        let channel = AcpChannel::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        channel.set_sink(tx);
        let error = channel
            .request_with(
                "session/request_permission",
                serde_json::json!({}),
                Some(Duration::from_millis(20)),
                None,
            )
            .await
            .expect_err("no answer must not wait forever");
        assert!(error.contains("session/request_permission"), "{error}");
        assert!(error.contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn a_cancelled_turn_stops_waiting_for_the_client() {
        let channel = AcpChannel::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        channel.set_sink(tx);
        let cancel = CancellationToken::new();
        let pending = {
            let channel = channel.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                channel
                    .request_human(
                        "session/request_permission",
                        serde_json::json!({}),
                        Some(&cancel),
                    )
                    .await
            })
        };
        cancel.cancel();
        let error = pending.await.unwrap().expect_err("cancelled request fails");
        assert!(error.contains("cancelled"), "{error}");
        assert!(
            channel.waiters.lock().await.is_empty(),
            "the waiter must not outlive the wait"
        );
    }

    #[tokio::test]
    async fn closing_the_channel_fails_pending_requests() {
        let channel = AcpChannel::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        channel.set_sink(tx);
        let pending = {
            let channel = channel.clone();
            tokio::spawn(async move {
                channel
                    .request_with(
                        "fs/read_text_file",
                        serde_json::json!({}),
                        Some(Duration::from_secs(60)),
                        None,
                    )
                    .await
            })
        };
        // Let the request register its waiter before the channel closes.
        tokio::time::sleep(Duration::from_millis(10)).await;
        channel.close().await;
        let error = pending.await.unwrap().expect_err("the client is gone");
        assert!(error.contains("disconnected"), "{error}");
    }

    #[test]
    fn test_response_id_ignores_requests_and_notifications() {
        assert_eq!(
            AcpChannel::response_id(&serde_json::json!({ "id": 4, "result": {} })),
            Some(4)
        );
        assert_eq!(
            AcpChannel::response_id(&serde_json::json!({ "id": 4, "method": "ping" })),
            None
        );
        assert_eq!(
            AcpChannel::response_id(&serde_json::json!({ "method": "session/cancel" })),
            None
        );
    }

    async fn answer_next(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<AcpOutbound>,
        channel: &AcpChannel,
        answer: serde_json::Value,
    ) -> (String, serde_json::Value) {
        let outbound = rx.recv().await.expect("one outbound request");
        match outbound {
            AcpOutbound::Request(request) => {
                let id = request.id.unwrap().as_u64().unwrap();
                assert!(
                    channel
                        .resolve(
                            id,
                            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": answer })
                        )
                        .await
                );
                (request.method, request.params.unwrap_or_default())
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_read_text_file_reverse_rpc() {
        let channel = AcpChannel::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        channel.set_sink(tx);
        let pending = {
            let channel = channel.clone();
            tokio::spawn(async move { channel.read_text_file("s1", "/work/a.txt").await })
        };
        let (method, params) =
            answer_next(&mut rx, &channel, serde_json::json!({ "content": "alpha" })).await;
        assert_eq!(method, "fs/read_text_file");
        assert_eq!(params["sessionId"], "s1");
        assert_eq!(params["path"], "/work/a.txt");
        assert_eq!(pending.await.unwrap().unwrap(), "alpha");
    }

    #[tokio::test]
    async fn test_create_terminal_reverse_rpc() {
        let channel = AcpChannel::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        channel.set_sink(tx);
        let pending = {
            let channel = channel.clone();
            tokio::spawn(async move {
                channel
                    .create_terminal(
                        "s1",
                        "bash",
                        &["-lc".to_string(), "ls".to_string()],
                        Some("/work"),
                        &[("NO_COLOR".to_string(), "1".to_string())],
                        4 * 1024 * 1024,
                    )
                    .await
            })
        };
        let (method, params) = answer_next(
            &mut rx,
            &channel,
            serde_json::json!({ "terminalId": "term-7" }),
        )
        .await;
        assert_eq!(method, "terminal/create");
        assert_eq!(params["command"], "bash");
        assert_eq!(params["args"][1], "ls");
        assert_eq!(params["cwd"], "/work");
        assert_eq!(params["env"][0]["name"], "NO_COLOR");
        assert_eq!(params["env"][0]["value"], "1");
        assert_eq!(params["outputByteLimit"], 4 * 1024 * 1024);
        assert_eq!(pending.await.unwrap().unwrap(), "term-7");
    }

    #[tokio::test]
    async fn test_read_text_file_requires_content() {
        let channel = AcpChannel::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        channel.set_sink(tx);
        let pending = {
            let channel = channel.clone();
            tokio::spawn(async move { channel.read_text_file("s1", "a").await })
        };
        answer_next(&mut rx, &channel, serde_json::json!({})).await;
        assert!(pending.await.unwrap().is_err());
    }
}
