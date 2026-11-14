//! Newline-delimited JSON-RPC 2.0 over a backend child's stdio, ported from v2
//! `session/subagent/backend/codexWire.ts`.
//!
//! The same transport carries the ACP backend, which is the same protocol with a
//! different method vocabulary, so the wire is shared.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::process::{read_line, write_line};

pub const CODE_X_WIRE_CLOSED: &str = "codex app-server transport closed";

type NotificationHandler = Arc<dyn Fn(&Value) + Send + Sync>;

#[derive(Clone)]
pub struct JsonRpcWire {
    stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    handlers: Arc<StdMutex<HashMap<String, Vec<NotificationHandler>>>>,
    next_id: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    stop: CancellationToken,
}

impl JsonRpcWire {
    /// v2 `new CodexWire(process)`: takes the child's stdout plus the shared
    /// stdin, and starts reading frames.
    pub fn new(
        stdout: Option<ChildStdout>,
        stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    ) -> Self {
        let pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let handlers: Arc<StdMutex<HashMap<String, Vec<NotificationHandler>>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicU64::new(1));
        let closed = Arc::new(AtomicBool::new(false));
        let stop = CancellationToken::new();

        if let Some(stdout) = stdout {
            let pending = Arc::clone(&pending);
            let handlers = Arc::clone(&handlers);
            let closed = Arc::clone(&closed);
            let stop_task = stop.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    tokio::select! {
                        _ = stop_task.cancelled() => break,
                        line = read_line(&mut reader) => {
                            let Some(line) = line else { break };
                            if line.trim().is_empty() { continue; }
                            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                            dispatch(message, &pending, &handlers);
                        }
                    }
                }
                closed.store(true, Ordering::Relaxed);
                reject_all(&pending, CODE_X_WIRE_CLOSED);
            });
        } else {
            closed.store(true, Ordering::Relaxed);
        }

        Self {
            stdin,
            pending,
            handlers,
            next_id,
            closed,
            stop,
        }
    }

    /// v2 `request(method, params, signal)`. `cancel` replaces v2's
    /// `AbortSignal`; `None` waits for the reply.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, String> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(CODE_X_WIRE_CLOSED.to_string());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, mut receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, sender);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(error) = self.write(&message).await {
            self.take_pending(id);
            return Err(error);
        }
        match cancel {
            Some(cancel) => {
                let cancelled = cancel.cancelled();
                tokio::select! {
                    _ = cancelled => {
                        self.take_pending(id);
                        Err("codex request aborted".to_string())
                    }
                    reply = receiver => reply.map_err(|_| CODE_X_WIRE_CLOSED.to_string())?,
                }
            }
            None => receiver
                .await
                .map_err(|_| CODE_X_WIRE_CLOSED.to_string())?,
        }
    }

    /// v2 `notify(method, params)`.
    pub async fn notify(&self, method: &str, params: Value) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let _ = self.write(&message).await;
    }

    /// v2 `onNotification(method, handler)`.
    pub fn on_notification(
        &self,
        method: &str,
        handler: Arc<dyn Fn(&Value) + Send + Sync>,
    ) {
        self.handlers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(method.to_string())
            .or_default()
            .push(handler);
    }

    /// v2 `dispose`: stop reading, drop stdin, fail every waiter.
    pub fn dispose(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.stop.cancel();
        self.reject_all();
        let stdin = Arc::clone(&self.stdin);
        tokio::spawn(async move {
            let guard = stdin.lock().await;
            drop(guard);
        });
    }

    fn take_pending(&self, id: u64) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
    }

    fn reject_all(&self) {
        reject_all(&self.pending, CODE_X_WIRE_CLOSED);
    }

    async fn write(&self, message: &Value) -> Result<(), String> {
        let line = format!("{}\n", serde_json::to_string(message).unwrap_or_default());
        let mut guard = self.stdin.lock().await;
        write_line(&mut guard, &line).await
    }
}

fn dispatch(
    message: Value,
    pending: &StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    handlers: &StdMutex<HashMap<String, Vec<NotificationHandler>>>,
) {
    let Some(object) = message.as_object() else { return };
    if let Some(id) = object.get("id").and_then(Value::as_u64) {
        let Some(sender) = pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id)
        else {
            return;
        };
        let reply = match object.get("error") {
            Some(Value::Null) | None => Ok(object.get("result").cloned().unwrap_or(Value::Null)),
            Some(Value::String(message)) => Err(message.clone()),
            Some(Value::Object(error)) => {
                Err(error.get("message").and_then(Value::as_str).unwrap_or("codex error").to_string())
            }
            Some(_) => Err("codex error".to_string()),
        };
        let _ = sender.send(reply);
        return;
    }
    if let Some(method) = object.get("method").and_then(Value::as_str) {
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        let handlers = handlers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(method)
            .cloned();
        if let Some(handlers) = handlers {
            for handler in handlers {
                handler(&params);
            }
        }
    }
}

fn reject_all(pending: &StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>, reason: &str) {
    let mut guard = pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for (_, sender) in guard.drain() {
        let _ = sender.send(Err(reason.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> {
        Arc::new(StdMutex::new(HashMap::new()))
    }

    fn handlers() -> Arc<StdMutex<HashMap<String, Vec<NotificationHandler>>>> {
        Arc::new(StdMutex::new(HashMap::new()))
    }

    #[test]
    fn a_result_message_resolves_its_request() {
        let pending = pending();
        let (sender, mut receiver) = oneshot::channel();
        pending.lock().unwrap().insert(1, sender);
        dispatch(
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "threadId": "t-1" } }),
            &pending,
            &handlers(),
        );
        let reply = receiver.try_recv().unwrap().unwrap();
        assert_eq!(reply["threadId"], "t-1");
        assert!(pending.lock().unwrap().is_empty());
    }

    #[test]
    fn an_error_message_rejects_with_its_message() {
        let pending = pending();
        let (sender, mut receiver) = oneshot::channel();
        pending.lock().unwrap().insert(7, sender);
        dispatch(
            json!({ "jsonrpc": "2.0", "id": 7, "error": { "message": "boom" } }),
            &pending,
            &handlers(),
        );
        let error = receiver.try_recv().unwrap().unwrap_err();
        assert_eq!(error, "boom");
    }

    #[test]
    fn a_notification_reaches_only_its_own_method_handlers() {
        let handlers = handlers();
        let calls = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&calls);
        handlers
            .lock()
            .unwrap()
            .entry("turn/updated".to_string())
            .or_default()
            .push(Arc::new(move |_| {
                counted.fetch_add(1, Ordering::Relaxed);
            }));
        dispatch(
            json!({ "jsonrpc": "2.0", "method": "turn/updated", "params": { "text": "hi" } }),
            &pending(),
            &handlers,
        );
        dispatch(
            json!({ "jsonrpc": "2.0", "method": "turn/complete", "params": {} }),
            &pending(),
            &handlers,
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn dispose_fails_every_pending_request() {
        let pending = pending();
        let mut receivers = Vec::new();
        for id in 1..=3 {
            let (sender, mut receiver) = oneshot::channel();
            pending.lock().unwrap().insert(id, sender);
            receivers.push(receiver);
        }
        reject_all(&pending, CODE_X_WIRE_CLOSED);
        assert!(pending.lock().unwrap().is_empty());
        for mut receiver in receivers {
            assert_eq!(receiver.try_recv().unwrap().unwrap_err(), CODE_X_WIRE_CLOSED);
        }
    }
}
