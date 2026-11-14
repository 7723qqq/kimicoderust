use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use futures_util::{Stream, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::_base::utils::abort::AbortSignal;
use crate::session::process::process_runner::IProcess;
use crate::wire::record::WireAsyncError;

#[derive(Debug)]
pub struct CodexWireClosedError;

impl CodexWireClosedError {
    pub fn name(&self) -> &'static str {
        "CodexWireClosedError"
    }
}

impl fmt::Display for CodexWireClosedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("codex app-server transport closed")
    }
}

impl Error for CodexWireClosedError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Number(u64),
    String(String),
}

struct PendingRequest {
    sender: tokio::sync::oneshot::Sender<Result<Value, WireAsyncError>>,
    signal: Option<AbortSignal>,
    on_abort: Arc<dyn Fn() + Send + Sync>,
}

pub type NotificationHandler = Arc<dyn Fn(Value) + Send + Sync>;

struct WireState {
    pending: HashMap<RequestId, PendingRequest>,
    handlers: HashMap<String, Vec<(u64, NotificationHandler)>>,
    next_id: u64,
    next_handler_id: u64,
    closed: bool,
}

pub struct CodexWire {
    process: Arc<dyn IProcess>,
    state: Arc<Mutex<WireState>>,
}

impl CodexWire {
    pub fn new(process: Arc<dyn IProcess>) -> Arc<Self> {
        let this = Arc::new(Self {
            process: process.clone(),
            state: Arc::new(Mutex::new(WireState {
                pending: HashMap::new(),
                handlers: HashMap::new(),
                next_id: 1,
                next_handler_id: 0,
                closed: false,
            })),
        });
        let weak = Arc::downgrade(&this);
        tokio::spawn(async move {
            read_transport(process, weak).await;
        });
        this
    }

    pub async fn request(
        self: &Arc<Self>,
        method: &str,
        params: Option<Value>,
        signal: Option<AbortSignal>,
    ) -> Result<Value, WireAsyncError> {
        let (id, receiver, on_abort) = {
            let mut state = self.state.lock();
            if state.closed {
                return Err(Box::new(CodexWireClosedError));
            }
            let id = state.next_id;
            state.next_id += 1;
            let request_id = RequestId::Number(id);
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let weak = Arc::downgrade(self);
            let abort_id = request_id.clone();
            let on_abort: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                if let Some(this) = weak.upgrade() {
                    let pending = this.state.lock().pending.remove(&abort_id);
                    if let Some(pending) = pending {
                        let _ = pending.sender.send(Err("codex request aborted".into()));
                    }
                }
            });
            state.pending.insert(
                request_id,
                PendingRequest {
                    sender,
                    signal: signal.clone(),
                    on_abort: on_abort.clone(),
                },
            );
            (id, receiver, on_abort)
        };
        if let Some(signal) = signal.as_ref() {
            if signal.aborted() {
                on_abort();
            } else {
                signal.add_event_listener("abort", on_abort.clone(), true);
            }
        }
        if let Err(error) = self
            .write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
        {
            let pending = self.state.lock().pending.remove(&RequestId::Number(id));
            if let Some(pending) = pending {
                if let Some(signal) = pending.signal {
                    signal.remove_event_listener("abort", pending.on_abort);
                }
            }
            return Err(error);
        }
        receiver.await.unwrap_or_else(|_| Err(Box::new(CodexWireClosedError)))
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), WireAsyncError> {
        if self.state.lock().closed {
            return Ok(());
        }
        self.write(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    pub fn on_notification(
        self: &Arc<Self>,
        method: &str,
        handler: NotificationHandler,
    ) -> Box<dyn Fn() + Send + Sync> {
        let id = {
            let mut state = self.state.lock();
            let id = state.next_handler_id;
            state.next_handler_id += 1;
            state
                .handlers
                .entry(method.to_string())
                .or_default()
                .push((id, handler));
            id
        };
        let weak = Arc::downgrade(self);
        let method = method.to_string();
        Box::new(move || {
            if let Some(this) = weak.upgrade()
                && let Some(handlers) = this.state.lock().handlers.get_mut(&method)
            {
                handlers.retain(|(candidate, _)| *candidate != id);
            }
        })
    }

    pub fn dispose(&self) {
        self.on_transport_closed();
    }

    fn handle_message(&self, message: Value) {
        let id = match message.get("id") {
            Some(Value::Number(id)) => id.as_u64().map(RequestId::Number),
            Some(Value::String(id)) => Some(RequestId::String(id.clone())),
            _ => None,
        };
        if let Some(id) = id {
            let pending = self.state.lock().pending.remove(&id);
            let Some(pending) = pending else {
                return;
            };
            if let Some(signal) = pending.signal {
                signal.remove_event_listener("abort", pending.on_abort);
            }
            if let Some(raw) = message.get("error") {
                let message = raw
                    .as_str()
                    .map(ToString::to_string)
                    .or_else(|| {
                        raw.get("message")
                            .and_then(Value::as_str)
                            .map(ToString::to_string)
                    })
                    .unwrap_or_else(|| "codex error".to_string());
                let _ = pending.sender.send(Err(message.into()));
            } else {
                let _ = pending.sender.send(Ok(message.get("result").cloned().unwrap_or(Value::Null)));
            }
            return;
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return;
        };
        let handlers = self
            .state
            .lock()
            .handlers
            .get(method)
            .cloned()
            .unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        for (_, handler) in handlers {
            handler(params.clone());
        }
    }

    fn on_transport_closed(&self) {
        let pending = {
            let mut state = self.state.lock();
            if state.closed && state.pending.is_empty() {
                return;
            }
            state.closed = true;
            std::mem::take(&mut state.pending)
        };
        for (_, pending) in pending {
            if let Some(signal) = pending.signal {
                signal.remove_event_listener("abort", pending.on_abort);
            }
            let _ = pending.sender.send(Err(Box::new(CodexWireClosedError)));
        }
    }

    async fn write(&self, message: Value) -> Result<(), WireAsyncError> {
        if self.state.lock().closed {
            return Ok(());
        }
        self.process
            .stdin_write(format!("{message}\n").as_bytes())
            .await
    }
}

async fn read_transport(process: Arc<dyn IProcess>, weak: Weak<CodexWire>) {
    let mut reader = LineReader::new(process.stdout());
    loop {
        match reader.read_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(message) = serde_json::from_str(&line) else {
                    break;
                };
                if let Some(this) = weak.upgrade() {
                    this.handle_message(message);
                } else {
                    return;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    if let Some(this) = weak.upgrade() {
        this.on_transport_closed();
    }
}

struct LineReader {
    stream: Pin<Box<dyn Stream<Item = Result<String, WireAsyncError>> + Send>>,
    buffer: String,
}

impl LineReader {
    fn new(stream: Pin<Box<dyn Stream<Item = Result<String, WireAsyncError>> + Send>>) -> Self {
        Self {
            stream,
            buffer: String::new(),
        }
    }

    async fn read_line(&mut self) -> Result<Option<String>, WireAsyncError> {
        loop {
            if let Some(newline) = self.buffer.find('\n') {
                let line = self.buffer[..newline].trim_end_matches('\r').to_string();
                self.buffer.drain(..=newline);
                return Ok(Some(line));
            }
            let Some(chunk) = self.stream.next().await else {
                return Ok(None);
            };
            self.buffer.push_str(&chunk?);
        }
    }
}
