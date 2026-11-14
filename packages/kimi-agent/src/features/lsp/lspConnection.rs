use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use super::framing::{MessageDecoder, encode_frame};
use crate::_base::errors::unexpected_error::on_unexpected_error;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("LSP server transport closed")]
pub struct LspTransportClosedError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("LSP request aborted")]
pub struct LspRequestAbortedError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("LSP server error {code}: {message}")]
pub struct LspServerError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LspConnectionError {
    #[error(transparent)]
    TransportClosed(#[from] LspTransportClosedError),
    #[error(transparent)]
    RequestAborted(#[from] LspRequestAbortedError),
    #[error(transparent)]
    Server(#[from] LspServerError),
    #[error("{0}")]
    Transport(String),
}

pub type LspServerRequestHandler =
    Arc<dyn Fn(String, Value) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;
pub type LspProtocolErrorHandler = Arc<dyn Fn(String) + Send + Sync>;

struct PendingRequest {
    resolve: oneshot::Sender<Result<Value, LspConnectionError>>,
}

struct LspConnectionInner {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: std::sync::Mutex<HashMap<String, PendingRequest>>,
    next_id: AtomicI64,
    closed: AtomicBool,
    on_server_request: LspServerRequestHandler,
    on_protocol_error: LspProtocolErrorHandler,
}

#[derive(Clone)]
pub struct LspConnection {
    inner: Arc<LspConnectionInner>,
}

impl LspConnection {
    pub fn from_child(
        mut child: Child,
        on_server_request: Option<LspServerRequestHandler>,
        on_protocol_error: Option<LspProtocolErrorHandler>,
    ) -> Result<Self, std::io::Error> {
        let stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "missing LSP server stdin")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "missing LSP server stdout")
        })?;
        let connection = Self {
            inner: Arc::new(LspConnectionInner {
                child: Mutex::new(child),
                stdin: Mutex::new(stdin),
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicI64::new(1),
                closed: AtomicBool::new(false),
                on_server_request: on_server_request.unwrap_or_else(|| {
                    Arc::new(|_, _| async { Ok(Value::Null) }.boxed())
                }),
                on_protocol_error: on_protocol_error.unwrap_or_else(|| Arc::new(|_| {})),
            }),
        };
        tokio::spawn(read_loop(connection.inner.clone(), stdout));
        Ok(connection)
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        signal: Option<CancellationToken>,
    ) -> Result<Value, LspConnectionError> {
        if self.is_closed() {
            return Err(LspTransportClosedError.into());
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::AcqRel);
        let key = id.to_string();
        let (resolve, response) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(key.clone(), PendingRequest { resolve });
        if signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
            self.inner.pending.lock().unwrap().remove(&key);
            let _ = self.notify("$/cancelRequest", Some(json!({ "id": id }))).await;
            return Err(LspRequestAbortedError.into());
        }
        if let Err(error) = self
            .write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
        {
            self.inner.pending.lock().unwrap().remove(&key);
            return Err(error);
        }
        match signal {
            None => response
                .await
                .map_err(|_| LspConnectionError::TransportClosed(LspTransportClosedError))?,
            Some(signal) => {
                tokio::select! {
                    result = response => result.map_err(|_| LspConnectionError::TransportClosed(LspTransportClosedError))?,
                    _ = signal.cancelled() => {
                        self.inner.pending.lock().unwrap().remove(&key);
                        let _ = self.notify("$/cancelRequest", Some(json!({ "id": id }))).await;
                        Err(LspRequestAbortedError.into())
                    }
                }
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), LspConnectionError> {
        if self.is_closed() {
            return Ok(());
        }
        self.write(json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    pub fn dispose(&self) {
        close(&self.inner);
    }

    pub(crate) async fn kill_ladder(&self, grace: Duration) {
        {
            let mut child = self.inner.child.lock().await;
            let _ = child.start_kill();
            if tokio::time::timeout(grace, child.wait()).await.is_err() {
                let _ = child.kill().await;
            }
        }
        self.dispose();
    }

    async fn write(&self, message: Value) -> Result<(), LspConnectionError> {
        if self.is_closed() {
            return Ok(());
        }
        let body = serde_json::to_string(&message).map_err(|error| LspConnectionError::Transport(error.to_string()))?;
        let frame = encode_frame(&body).map_err(|error| LspConnectionError::Transport(error.to_string()))?;
        let mut stdin = self.inner.stdin.lock().await;
        tokio::io::AsyncWriteExt::write_all(&mut *stdin, &frame)
            .await
            .map_err(|_| LspConnectionError::TransportClosed(LspTransportClosedError))?;
        tokio::io::AsyncWriteExt::flush(&mut *stdin)
            .await
            .map_err(|_| LspTransportClosedError.into())
    }
}

async fn read_loop(inner: Arc<LspConnectionInner>, mut stdout: ChildStdout) {
    let mut decoder = MessageDecoder::new();
    let mut chunk = [0_u8; 8192];
    loop {
        if inner.closed.load(Ordering::Acquire) {
            break;
        }
        match tokio::io::AsyncReadExt::read(&mut stdout, &mut chunk).await {
            Ok(0) | Err(_) => {
                close(&inner);
                break;
            }
            Ok(read) => {
                let messages = match decoder.feed(&chunk[..read]) {
                    Ok(messages) => messages,
                    Err(_) => {
                        close_on_protocol_error(&inner, "frame header is not decodable");
                        break;
                    }
                };
                for message in messages {
                    let parsed = match serde_json::from_str::<Value>(&message) {
                        Ok(value) if value.is_object() => value,
                        Ok(_) => {
                            close_on_protocol_error(&inner, "frame is not a JSON-RPC object");
                            return;
                        }
                        Err(_) => {
                            close_on_protocol_error(&inner, "frame is not valid JSON");
                            return;
                        }
                    };
                    handle_message(&inner, parsed).await;
                }
            }
        }
    }
}

async fn handle_message(inner: &Arc<LspConnectionInner>, message: Value) {
    let has_method = message.get("method").and_then(Value::as_str).is_some();
    let has_response = message.get("result").is_some() || message.get("error").is_some();
    if let Some(id) = message.get("id").cloned()
        && has_response
    {
        let pending = inner.pending.lock().unwrap().remove(&id_key(&id));
        if let Some(pending) = pending {
            if let Some(error) = message.get("error") {
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown LSP server error")
                    .to_string();
                let _ = pending.resolve.send(Err(LspServerError { code, message }.into()));
            } else {
                let _ = pending.resolve.send(Ok(message.get("result").cloned().unwrap_or(Value::Null)));
            }
        }
        return;
    }
    if !has_method {
        return;
    }
    let Some(id) = message.get("id").cloned() else { return };
    let method = message["method"].as_str().unwrap_or_default().to_string();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let inner = inner.clone();
    tokio::spawn(async move {
        let result = (inner.on_server_request)(method, params).await;
        let response = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32603, "message": error }
            }),
        };
        let connection = LspConnection { inner };
        let _ = connection.write(response).await;
    });
}

fn close(inner: &Arc<LspConnectionInner>) {
    if inner.closed.swap(true, Ordering::AcqRel) {
        return;
    }
    let pending = std::mem::take(&mut *inner.pending.lock().unwrap());
    for (_, pending) in pending {
        let _ = pending.resolve.send(Err(LspTransportClosedError.into()));
    }
}

fn close_on_protocol_error(inner: &Arc<LspConnectionInner>, detail: &str) {
    close(inner);
    if let Err(error) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (inner.on_protocol_error)(detail.to_string());
    })) {
        let message = error
            .downcast_ref::<&str>()
            .map(|message| (*message).to_string())
            .or_else(|| error.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown error".to_string());
        on_unexpected_error(message);
    }
}

fn id_key(id: &Value) -> String {
    match id {
        Value::String(id) => id.clone(),
        _ => id.to_string(),
    }
}
