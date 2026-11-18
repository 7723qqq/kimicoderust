use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::os::interface::host_timers::IHostTimers;
use crate::remote::protocol::channel::{ConnectionCloseInfo, ConnectionCloseKind, ExecChannel};
use crate::remote::protocol::codec::{LineFrameDecoder, encode_frame};
use crate::remote::protocol::errors::{
    ConnectionClosedError, RPC_METHOD_NOT_FOUND, RequestTimeoutError, RpcError,
};
use crate::remote::protocol::messages::{
    JsonRpcMessage, ProtocolViolationError, RequestId, classify_message,
};
use crate::remote::protocol::methods::{
    INITIALIZE_METHOD, InitializeResult, MIN_EXECUTOR_VERSION, PING_METHOD,
    PROCESS_WRITE_METHOD, RemoteEnvironmentInfo, SERVER_NOTIFICATION_METHODS, compare_versions,
};
use crate::wire::record::WireAsyncError;

use super::exec_bridge::BytePipe;

pub const DEFAULT_INITIALIZE_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_REQUEST_CALL_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_LIVENESS_INTERVAL_MS: u64 = 30_000;
pub const DEFAULT_LIVENESS_TIMEOUT_MS: u64 = 10_000;

pub struct ConnectOptions {
    pub client_name: String,
    pub client_version: String,
    pub min_executor_version: Option<String>,
    pub initialize_timeout_ms: Option<u64>,
    pub request_call_timeout_ms: Option<u64>,
    pub liveness_interval_ms: Option<u64>,
    pub liveness_timeout_ms: Option<u64>,
    pub handshake_timers: Option<Arc<dyn IHostTimers>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeErrorKind {
    Timeout,
    ExecutorExit,
    Incompatible,
    Authentication,
}

#[derive(Default)]
pub struct HandshakeErrorDetails {
    pub kind: Option<HandshakeErrorKind>,
    pub exit_code: Option<i32>,
    pub executor_version: Option<String>,
    pub min_executor_version: Option<String>,
    pub cause: Option<WireAsyncError>,
}

#[derive(Debug)]
pub struct HandshakeError {
    message: String,
    pub kind: Option<HandshakeErrorKind>,
    pub exit_code: Option<i32>,
    pub executor_version: Option<String>,
    pub min_executor_version: Option<String>,
    cause: Option<WireAsyncError>,
}

impl HandshakeError {
    pub fn new(message: impl Into<String>, details: HandshakeErrorDetails) -> Self {
        Self {
            message: message.into(),
            kind: details.kind,
            exit_code: details.exit_code,
            executor_version: details.executor_version,
            min_executor_version: details.min_executor_version,
            cause: details.cause,
        }
    }
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for HandshakeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.cause.as_deref().map(|cause| cause as &(dyn Error + 'static))
    }
}

type PendingResult = Result<Value, WireAsyncError>;
type PendingSender = tokio::sync::oneshot::Sender<PendingResult>;

struct PendingCall {
    sender: PendingSender,
    timer: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Handshake,
    Ready,
    Closed,
}

struct ConnectionState {
    lifecycle: Lifecycle,
    handshake_complete: bool,
    next_id: i64,
    pending: IndexMap<RequestId, PendingCall>,
    notification_handlers: IndexMap<String, IndexMap<u64, Arc<dyn Fn(Value) + Send + Sync>>>,
    next_handler_id: u64,
    close_listeners: IndexMap<u64, Arc<dyn Fn(ConnectionCloseInfo) + Send + Sync>>,
    next_close_listener_id: u64,
    close_info: Option<ConnectionCloseInfo>,
    environment: Option<RemoteEnvironmentInfo>,
    executor_version: Option<String>,
    liveness_task: Option<tokio::task::JoinHandle<()>>,
    pong_timer: Option<tokio::task::JoinHandle<()>>,
    ping_in_flight: bool,
    liveness_stopped: bool,
}

pub struct RemoteExecConnection {
    pipe: Arc<dyn BytePipe>,
    weak: Mutex<std::sync::Weak<RemoteExecConnection>>,
    decoder: Mutex<LineFrameDecoder>,
    state: Arc<Mutex<ConnectionState>>,
    request_call_timeout_ms: u64,
    liveness_interval_ms: u64,
    liveness_timeout_ms: u64,
    last_inbound_at: std::sync::atomic::AtomicI64,
}

impl RemoteExecConnection {
    pub async fn connect(pipe: Arc<dyn BytePipe>, options: ConnectOptions) -> Result<Arc<Self>, WireAsyncError> {
        let request_call_timeout_ms = options.request_call_timeout_ms.unwrap_or(DEFAULT_REQUEST_CALL_TIMEOUT_MS);
        let liveness_interval_ms = options.liveness_interval_ms.unwrap_or(DEFAULT_LIVENESS_INTERVAL_MS);
        let liveness_timeout_ms = options.liveness_timeout_ms.unwrap_or(DEFAULT_LIVENESS_TIMEOUT_MS);
        let connection = Arc::new(Self {
            pipe: pipe.clone(),
            weak: Mutex::new(std::sync::Weak::new()),
            decoder: Mutex::new(LineFrameDecoder::new()),
            state: Arc::new(Mutex::new(ConnectionState {
                lifecycle: Lifecycle::Handshake,
                handshake_complete: false,
                next_id: 1,
                pending: IndexMap::new(),
                notification_handlers: IndexMap::new(),
                next_handler_id: 0,
                close_listeners: IndexMap::new(),
                next_close_listener_id: 0,
                close_info: None,
                environment: None,
                executor_version: None,
                liveness_task: None,
                pong_timer: None,
                ping_in_flight: false,
                liveness_stopped: false,
            })),
            request_call_timeout_ms,
            liveness_interval_ms,
            liveness_timeout_ms,
            last_inbound_at: std::sync::atomic::AtomicI64::new(now_ms()),
        });
        *connection.weak.lock() = Arc::downgrade(&connection);
        let weak = Arc::downgrade(&connection);
        pipe.on_data(Arc::new(move |chunk| {
            if let Some(connection) = weak.upgrade() {
                connection.on_data(&chunk);
            }
        }));
        let weak = Arc::downgrade(&connection);
        pipe.on_end(Arc::new(move || {
            if let Some(connection) = weak.upgrade() {
                connection.fail(
                    Box::new(ConnectionClosedError::new("pipe ended")),
                    Some(ConnectionCloseKind::Eof),
                );
            }
        }));
        let weak = Arc::downgrade(&connection);
        pipe.on_error(Arc::new(move |error| {
            if let Some(connection) = weak.upgrade() {
                connection.fail(
                    Box::new(ConnectionClosedError::new(format!("pipe error: {error}"))),
                    Some(ConnectionCloseKind::PipeError),
                );
            }
        }));
        let id = connection.allocate_id();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        connection.state.lock().pending.insert(
            id.clone(),
            PendingCall {
                sender,
                timer: None,
            },
        );
        let timeout_ms = options.initialize_timeout_ms.unwrap_or(DEFAULT_INITIALIZE_TIMEOUT_MS);
        let custom_timer = options.handshake_timers.map(|timers| {
            let weak = Arc::downgrade(&connection);
            let timer = timers.set_timeout(
                Arc::new(move || {
                    if let Some(connection) = weak.upgrade() {
                        connection.fail(
                            Box::new(HandshakeError::new(
                                format!("initialize timed out after {timeout_ms}ms"),
                                HandshakeErrorDetails {
                                    kind: Some(HandshakeErrorKind::Timeout),
                                    ..Default::default()
                                },
                            )),
                            None,
                        );
                    }
                }),
                timeout_ms,
            );
            (timers, timer)
        });
        let default_timer = if custom_timer.is_none() {
            let weak = Arc::downgrade(&connection);
            Some(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
                if let Some(connection) = weak.upgrade() {
                    connection.fail(
                        Box::new(HandshakeError::new(
                            format!("initialize timed out after {timeout_ms}ms"),
                            HandshakeErrorDetails {
                                kind: Some(HandshakeErrorKind::Timeout),
                                ..Default::default()
                            },
                        )),
                        None,
                    );
                }
            }))
        } else {
            None
        };
        connection.pipe.write(&encode_frame(&json!({
            "id": id,
            "method": INITIALIZE_METHOD,
            "params": {
                "clientName": options.client_name,
                "clientVersion": options.client_version,
            }
        }))?);
        let result = receiver.await;
        if let Some((timers, timer)) = custom_timer {
            timers.clear_timeout(timer);
        }
        if let Some(timer) = default_timer {
            timer.abort();
        }
        let result = match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                connection.teardown();
                return Err(error);
            }
            Err(error) => {
                connection.teardown();
                return Err(error.into());
            }
        };
        if let Some(gate) = connection.gate_environment(
            options.min_executor_version.as_deref().unwrap_or(MIN_EXECUTOR_VERSION),
        ) {
            connection.teardown();
            return Err(Box::new(gate));
        }
        connection.state.lock().lifecycle = Lifecycle::Ready;
        connection.start_liveness();
        Ok(connection)
    }

    fn allocate_id(&self) -> RequestId {
        let mut state = self.state.lock();
        let id = state.next_id;
        state.next_id += 1;
        RequestId::Number(id.into())
    }

    pub fn environment(&self) -> RemoteEnvironmentInfo {
        self.state.lock().environment.clone().unwrap()
    }

    pub fn executor_version(&self) -> String {
        self.state.lock().executor_version.clone().unwrap()
    }

    pub fn closed(&self) -> bool {
        self.state.lock().lifecycle == Lifecycle::Closed
    }

    pub fn close_reason(&self) -> Option<ConnectionCloseInfo> {
        self.state.lock().close_info.clone()
    }

    fn on_initialize_result(&self, result: Value) -> Result<InitializeResult, WireAsyncError> {
        let parsed = self.validate_initialize_result(result)?;
        self.state.lock().executor_version = Some(parsed.executor_version.clone());
        self.state.lock().environment = Some(parsed.environment.clone());
        self.state.lock().handshake_complete = true;
        Ok(parsed)
    }

    fn validate_initialize_result(&self, result: Value) -> Result<InitializeResult, HandshakeError> {
        let Some(object) = result.as_object() else {
            return Err(HandshakeError::new(
                "initialize response must be an object",
                Default::default(),
            ));
        };
        if object.get("executorVersion").and_then(Value::as_str).is_none_or(|value| value.is_empty()) {
            return Err(HandshakeError::new(
                "initialize response must carry executorVersion",
                Default::default(),
            ));
        }
        let Some(environment) = object.get("environment").and_then(Value::as_object) else {
            return Err(HandshakeError::new(
                "initialize response must carry an environment object",
                Default::default(),
            ));
        };
        for field in [
            "osKind", "osArch", "osVersion", "shellName", "shellPath", "pathClass", "homeDir",
            "cwd", "tempDir",
        ] {
            if environment.get(field).and_then(Value::as_str).is_none_or(|value| value.is_empty()) {
                return Err(HandshakeError::new(
                    format!("environment.{field} must be a non-empty string"),
                    Default::default(),
                ));
            }
        }
        serde_json::from_value(result).map_err(|error| {
            HandshakeError::new(
                error.to_string(),
                HandshakeErrorDetails {
                    cause: Some(error.into()),
                    ..Default::default()
                },
            )
        })
    }

    fn gate_environment(&self, min_executor_version: &str) -> Option<HandshakeError> {
        let executor_version = self.executor_version();
        if compare_versions(&executor_version, min_executor_version) < 0 {
            return Some(HandshakeError::new(
                format!(
                    "executor version {executor_version} is below the minimum {min_executor_version}; upgrade the remote executor (kimi exec-server) and retry"
                ),
                HandshakeErrorDetails {
                    kind: Some(HandshakeErrorKind::Incompatible),
                    executor_version: Some(executor_version),
                    min_executor_version: Some(min_executor_version.to_string()),
                    ..Default::default()
                },
            ));
        }
        let environment = self.environment();
        if environment.path_class != "posix" && environment.path_class != "win32" {
            return Some(HandshakeError::new(
                format!(
                    "executor environment {} has unsupported path class {}",
                    environment.os_kind, environment.path_class
                ),
                HandshakeErrorDetails {
                    kind: Some(HandshakeErrorKind::Incompatible),
                    ..Default::default()
                },
            ));
        }
        if environment.path_class == "win32" && environment.windows_process_lifecycle != Some(true) {
            return Some(HandshakeError::new(
                "Windows executor does not support the required process lifecycle; upgrade the remote executor (kimi exec-server) and retry",
                HandshakeErrorDetails {
                    kind: Some(HandshakeErrorKind::Incompatible),
                    ..Default::default()
                },
            ));
        }
        None
    }

    pub fn close(&self) {
        self.fail(
            Box::new(ConnectionClosedError::new("connection closed by client")),
            Some(ConnectionCloseKind::ClientClosed),
        );
    }

    fn start_liveness(self: &Arc<Self>) {
        let this = self.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(this.liveness_interval_ms)).await;
                if this.state.lock().liveness_stopped {
                    break;
                }
                this.on_liveness_tick();
            }
        });
        self.state.lock().liveness_task = Some(task);
    }

    fn stop_liveness(&self) {
        let mut state = self.state.lock();
        state.liveness_stopped = true;
        if let Some(task) = state.liveness_task.take() {
            task.abort();
        }
        if let Some(timer) = state.pong_timer.take() {
            timer.abort();
        }
        state.ping_in_flight = false;
    }

    fn on_liveness_tick(self: &Arc<Self>) {
        {
            let state = self.state.lock();
            if state.lifecycle != Lifecycle::Ready || state.ping_in_flight {
                return;
            }
        }
        if now_ms() - self.last_inbound_at.load(std::sync::atomic::Ordering::Relaxed)
            < self.liveness_interval_ms as i64
        {
            return;
        }
        self.state.lock().ping_in_flight = true;
        let this = self.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(this.liveness_timeout_ms)).await;
            this.fail(
                Box::new(ConnectionClosedError::new(format!(
                    "liveness ping timed out after {}ms",
                    this.liveness_timeout_ms
                ))),
                Some(ConnectionCloseKind::Liveness),
            );
        });
        self.state.lock().pong_timer = Some(timer);
        let this = self.clone();
        tokio::spawn(async move {
            let result = this.call(PING_METHOD, None).await;
            match result {
                Err(error) if error.downcast_ref::<RpcError>().is_some_and(|error| error.code == RPC_METHOD_NOT_FOUND) => {
                    this.stop_liveness();
                }
                _ => this.settle_ping(),
            }
        });
    }

    fn settle_ping(&self) {
        let mut state = self.state.lock();
        if let Some(timer) = state.pong_timer.take() {
            timer.abort();
        }
        state.ping_in_flight = false;
    }

    fn on_data(&self, chunk: &[u8]) {
        if self.closed() {
            return;
        }
        self.last_inbound_at.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
        let frames = match self.decoder.lock().push(chunk) {
            Ok(frames) => frames,
            Err(error) => {
                self.fail(Box::new(error), None);
                return;
            }
        };
        for frame in frames {
            if self.closed() {
                return;
            }
            self.on_frame(frame);
        }
    }

    fn on_frame(&self, frame: Value) {
        let message = match classify_message(frame) {
            Ok(message) => message,
            Err(error) => {
                self.fail(Box::new(error), None);
                return;
            }
        };
        match message {
            JsonRpcMessage::Response(response) => {
                let is_initialize = matches!(response.id, RequestId::Number(ref id) if id.as_i64() == Some(1))
                    && !self.state.lock().handshake_complete;
                if is_initialize {
                    if let Err(error) = self.on_initialize_result(response.result.clone()) {
                        self.fail(error, None);
                        return;
                    }
                }
                let pending = self.state.lock().pending.remove(&response.id);
                if let Some(pending) = pending {
                    if let Some(timer) = pending.timer {
                        timer.abort();
                    }
                    let _ = pending.sender.send(Ok(response.result));
                }
            }
            JsonRpcMessage::ErrorResponse(response) => {
                let pending = self.state.lock().pending.remove(&response.id);
                if let Some(pending) = pending {
                    if let Some(timer) = pending.timer {
                        timer.abort();
                    }
                    let _ = pending.sender.send(Err(Box::new(RpcError::from_error_body(response.error))));
                }
            }
            JsonRpcMessage::Request(request) => {
                if !self.state.lock().handshake_complete {
                    self.fail(
                        Box::new(ProtocolViolationError::new(
                            "received a message before the initialize response",
                        )),
                        Some(ConnectionCloseKind::ProtocolError),
                    );
                } else {
                    self.fail(
                        Box::new(ProtocolViolationError::new(format!(
                            "unexpected server request {}",
                            request.method
                        ))),
                        Some(ConnectionCloseKind::ProtocolError),
                    );
                }
            }
            JsonRpcMessage::Notification(notification) => {
                if !self.state.lock().handshake_complete {
                    self.fail(
                        Box::new(ProtocolViolationError::new(
                            "received a message before the initialize response",
                        )),
                        Some(ConnectionCloseKind::ProtocolError),
                    );
                    return;
                }
                if !SERVER_NOTIFICATION_METHODS.contains(notification.method.as_str()) {
                    self.fail(
                        Box::new(ProtocolViolationError::new(format!(
                            "unknown notification {}",
                            notification.method
                        ))),
                        Some(ConnectionCloseKind::ProtocolError),
                    );
                    return;
                }
                let handlers = self
                    .state
                    .lock()
                    .notification_handlers
                    .get(&notification.method)
                    .map(|handlers| handlers.values().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                for handler in handlers {
                    let params = notification.params.clone().unwrap_or(Value::Null);
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(params)));
                }
            }
        }
    }

    fn expire_request(&self, id: &RequestId, method: &str) {
        let pending = self.state.lock().pending.remove(id);
        if let Some(pending) = pending {
            let _ = pending.sender.send(Err(Box::new(RequestTimeoutError::new(
                method,
                self.request_call_timeout_ms,
            ))));
        }
    }

    fn fail(&self, error: WireAsyncError, kind: Option<ConnectionCloseKind>) {
        {
            let mut state = self.state.lock();
            if state.lifecycle == Lifecycle::Closed {
                return;
            }
            state.lifecycle = Lifecycle::Closed;
            let kind = kind.unwrap_or_else(|| {
                if error.downcast_ref::<ProtocolViolationError>().is_some() {
                    ConnectionCloseKind::ProtocolError
                } else {
                    ConnectionCloseKind::Other
                }
            });
            state.close_info = Some(ConnectionCloseInfo {
                reason: error.to_string(),
                kind: Some(kind),
            });
        }
        self.stop_liveness();
        let handshake_complete = self.state.lock().handshake_complete;
        let mut original = Some(error);
        let failure = original.as_ref().map(|error| error.to_string());
        let pending = {
            let mut state = self.state.lock();
            std::mem::take(&mut state.pending).into_iter().collect::<Vec<_>>()
        };
        for (id, pending) in pending {
            if let Some(timer) = pending.timer {
                timer.abort();
            }
            let is_initialize = matches!(id, RequestId::Number(ref id) if id.as_i64() == Some(1));
            let error = if !handshake_complete && is_initialize {
                original.take().unwrap_or_else(|| {
                    Box::new(ConnectionClosedError::new(failure.clone().unwrap())) as WireAsyncError
                })
            } else {
                Box::new(ConnectionClosedError::new(failure.clone().unwrap())) as WireAsyncError
            };
            let _ = pending.sender.send(Err(error));
        }
        let info = self.close_reason();
        let listeners = {
            let mut state = self.state.lock();
            let listeners = state.close_listeners.values().cloned().collect::<Vec<_>>();
            state.close_listeners.clear();
            listeners
        };
        if let Some(info) = info {
            for listener in listeners {
                listener(info.clone());
            }
        }
        self.teardown();
    }

    fn teardown(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.pipe.end()));
    }
}

#[async_trait]
impl ExecChannel for RemoteExecConnection {
    async fn call(&self, method: &str, params: Option<Value>) -> Result<Value, WireAsyncError> {
        {
            let state = self.state.lock();
            if state.lifecycle != Lifecycle::Ready {
                let reason = state.close_info.as_ref().map(|info| info.reason.clone());
                return Err(Box::new(ConnectionClosedError::new(match reason {
                    Some(reason) => format!("connection is closed: {reason}"),
                    None => "connection is not ready".to_string(),
                })));
            }
        }
        let id = self.allocate_id();
        let frame = encode_frame(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let timer = if method == PROCESS_WRITE_METHOD {
            None
        } else {
            let weak = self.weak.lock().clone();
            let id = id.clone();
            let method = method.to_string();
            Some(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(weak.upgrade().map_or(DEFAULT_REQUEST_CALL_TIMEOUT_MS, |this| this.request_call_timeout_ms))).await;
                if let Some(this) = weak.upgrade() {
                    this.expire_request(&id, &method);
                }
            }))
        };
        self.state.lock().pending.insert(id.clone(), PendingCall {
            sender,
            timer,
        });
        self.pipe.write(&frame);
        receiver.await.unwrap_or_else(|_| {
            Err(Box::new(ConnectionClosedError::new("connection is closed")) as WireAsyncError)
        })
    }

    fn notify(&self, method: &str, params: Option<Value>) {
        if self.state.lock().lifecycle != Lifecycle::Ready {
            return;
        }
        if let Ok(frame) = encode_frame(&json!({ "method": method, "params": params })) {
            self.pipe.write(&frame);
        }
    }

    fn on_notification(
        &self,
        method: &str,
        handler: Arc<dyn Fn(Value) + Send + Sync>,
    ) -> Box<dyn FnOnce() + Send> {
        let id = {
            let mut state = self.state.lock();
            let id = state.next_handler_id;
            state.next_handler_id += 1;
            state.notification_handlers.entry(method.to_string()).or_default().insert(id, handler);
            id
        };
        let state = self.state.clone();
        let method = method.to_string();
        Box::new(move || {
            let mut state = state.lock();
            if let Some(handlers) = state.notification_handlers.get_mut(&method) {
                handlers.remove(&id);
            }
        })
    }

    fn on_did_close(&self, listener: Arc<dyn Fn(ConnectionCloseInfo) + Send + Sync>) {
        if let Some(info) = self.close_reason() {
            listener(info);
            return;
        }
        let mut state = self.state.lock();
        let id = state.next_close_listener_id;
        state.next_close_listener_id += 1;
        state.close_listeners.insert(id, listener);
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
