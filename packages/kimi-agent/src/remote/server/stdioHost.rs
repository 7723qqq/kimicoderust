use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::os::backends::host::host_fs_service::HostFileSystem;
use crate::os::backends::host::host_process_service::HostProcessService;
use crate::remote::protocol::codec::{LineFrameDecoder, encode_frame};
use crate::remote::protocol::errors::{RPC_INTERNAL_ERROR, RPC_INVALID_REQUEST, RPC_METHOD_NOT_FOUND, RpcError, to_rpc_error};
use crate::remote::protocol::messages::{
    JsonRpcMessage, JsonRpcRequest, ProtocolViolationError, RequestId, classify_message,
};
use crate::remote::protocol::methods::*;
use crate::wire::record::WireAsyncError;

use super::fs_handler::FsHandler;
use super::process_manager::ProcessManager;

#[async_trait::async_trait]
pub trait StdioReadable: Send + Sync {
    fn on_data(&self, listener: Arc<dyn Fn(Vec<u8>) + Send + Sync>);
    fn on_end(&self, listener: Arc<dyn Fn() + Send + Sync>);
    fn on_error(&self, listener: Arc<dyn Fn(std::io::Error) + Send + Sync>);
}

#[async_trait::async_trait]
pub trait StdioWritable: Send + Sync {
    async fn write(&self, frame: Vec<u8>) -> bool;
    async fn drain(&self);
    fn end(&self);
    fn on_error(&self, listener: Arc<dyn Fn(std::io::Error) + Send + Sync>);
}

pub struct StdioInput {
    inner: Arc<dyn StdioReadable>,
}

impl StdioInput {
    pub fn new(inner: Arc<dyn StdioReadable>) -> Self {
        Self { inner }
    }

    pub fn stdin() -> Self {
        Self::new(Arc::new(StdinReadable::new()))
    }
}

#[async_trait::async_trait]
impl StdioReadable for StdioInput {
    fn on_data(&self, listener: Arc<dyn Fn(Vec<u8>) + Send + Sync>) {
        self.inner.on_data(listener);
    }

    fn on_end(&self, listener: Arc<dyn Fn() + Send + Sync>) {
        self.inner.on_end(listener);
    }

    fn on_error(&self, listener: Arc<dyn Fn(std::io::Error) + Send + Sync>) {
        self.inner.on_error(listener);
    }
}

#[derive(Clone)]
pub struct StdioOutput {
    inner: Arc<dyn StdioWritable>,
}

impl StdioOutput {
    pub fn new(inner: Arc<dyn StdioWritable>) -> Self {
        Self { inner }
    }

    pub fn stdout() -> Self {
        Self::new(Arc::new(StdoutWritable::new()))
    }
}

#[async_trait::async_trait]
impl StdioWritable for StdioOutput {
    async fn write(&self, frame: Vec<u8>) -> bool {
        self.inner.write(frame).await
    }

    async fn drain(&self) {
        self.inner.drain().await;
    }

    fn end(&self) {
        self.inner.end();
    }

    fn on_error(&self, listener: Arc<dyn Fn(std::io::Error) + Send + Sync>) {
        self.inner.on_error(listener);
    }
}

struct StdinReadable {
    listeners: Arc<Mutex<Vec<Arc<dyn Fn(Vec<u8>) + Send + Sync>>>>,
    end_listeners: Arc<Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>>,
    error_listeners: Arc<Mutex<Vec<Arc<dyn Fn(std::io::Error) + Send + Sync>>>>,
    started: std::sync::atomic::AtomicBool,
}

impl StdinReadable {
    fn new() -> Self {
        Self {
            listeners: Arc::new(Mutex::new(Vec::new())),
            end_listeners: Arc::new(Mutex::new(Vec::new())),
            error_listeners: Arc::new(Mutex::new(Vec::new())),
            started: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn start(&self) {
        if self.started.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let listeners = self.listeners.clone();
        let end_listeners = self.end_listeners.clone();
        let error_listeners = self.error_listeners.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut stdin = tokio::io::stdin();
            let mut buffer = [0u8; 8192];
            loop {
                match stdin.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(read) => {
                        for listener in listeners.lock().iter() {
                            listener(buffer[..read].to_vec());
                        }
                    }
                    Err(error) => {
                        for listener in error_listeners.lock().iter() {
                            listener(std::io::Error::other(error.to_string()));
                        }
                        break;
                    }
                }
            }
            for listener in end_listeners.lock().iter() {
                listener();
            }
        });
    }
}

#[async_trait::async_trait]
impl StdioReadable for StdinReadable {
    fn on_data(&self, listener: Arc<dyn Fn(Vec<u8>) + Send + Sync>) {
        self.listeners.lock().push(listener);
        self.start();
    }

    fn on_end(&self, listener: Arc<dyn Fn() + Send + Sync>) {
        self.end_listeners.lock().push(listener);
        self.start();
    }

    fn on_error(&self, listener: Arc<dyn Fn(std::io::Error) + Send + Sync>) {
        self.error_listeners.lock().push(listener);
        self.start();
    }
}

struct StdoutWritable {
    output: Arc<tokio::sync::Mutex<tokio::io::Stdout>>,
    listeners: Arc<Mutex<Vec<Arc<dyn Fn(std::io::Error) + Send + Sync>>>>,
}

impl StdoutWritable {
    fn new() -> Self {
        Self {
            output: Arc::new(tokio::sync::Mutex::new(tokio::io::stdout())),
            listeners: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl StdioWritable for StdoutWritable {
    async fn write(&self, frame: Vec<u8>) -> bool {
        use tokio::io::AsyncWriteExt;
        match self.output.lock().await.write_all(&frame).await {
            Ok(()) => true,
            Err(error) => {
                for listener in self.listeners.lock().iter() {
                    listener(std::io::Error::other(error.to_string()));
                }
                false
            }
        }
    }

    async fn drain(&self) {
        use tokio::io::AsyncWriteExt;
        let _ = self.output.lock().await.flush().await;
    }

    fn end(&self) {
        let output = self.output.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = output.lock().await.shutdown().await;
        });
    }

    fn on_error(&self, listener: Arc<dyn Fn(std::io::Error) + Send + Sync>) {
        self.listeners.lock().push(listener);
    }
}

pub struct StdioHostOptions {
    pub version: String,
    pub environment: RemoteEnvironmentInfo,
    pub input: StdioInput,
    pub output: StdioOutput,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Control,
    Data,
}

type Handler = Arc<dyn Fn(Value) -> BoxFuture<'static, Result<Value, WireAsyncError>> + Send + Sync>;

struct OutboundState {
    control: VecDeque<Vec<u8>>,
    data: VecDeque<Vec<u8>>,
    control_bytes: usize,
    data_bytes: usize,
    pumping: bool,
    destroyed: bool,
    paused: bool,
}

struct OutboundWriter {
    output: StdioOutput,
    watermark_bytes: usize,
    on_fuse_breach: Arc<dyn Fn() + Send + Sync>,
    on_watermark_change: Arc<dyn Fn(bool) + Send + Sync>,
    state: Arc<Mutex<OutboundState>>,
}

impl OutboundWriter {
    fn new(
        output: StdioOutput,
        watermark_bytes: usize,
        on_fuse_breach: Arc<dyn Fn() + Send + Sync>,
        on_watermark_change: Arc<dyn Fn(bool) + Send + Sync>,
    ) -> Self {
        Self {
            output,
            watermark_bytes,
            on_fuse_breach,
            on_watermark_change,
            state: Arc::new(Mutex::new(OutboundState {
                control: VecDeque::new(),
                data: VecDeque::new(),
                control_bytes: 0,
                data_bytes: 0,
                pumping: false,
                destroyed: false,
                paused: false,
            })),
        }
    }

    fn enqueue(&self, frame: Vec<u8>, lane: Lane) {
        {
            let mut state = self.state.lock();
            if state.destroyed {
                return;
            }
            if state.control_bytes + state.data_bytes + frame.len() > MAX_PENDING_SEND_BYTES {
                drop(state);
                (self.on_fuse_breach)();
                return;
            }
            match lane {
                Lane::Control => {
                    state.control_bytes += frame.len();
                    state.control.push_back(frame);
                }
                Lane::Data => {
                    state.data_bytes += frame.len();
                    state.data.push_back(frame);
                    if !state.paused && state.data_bytes > self.watermark_bytes {
                        state.paused = true;
                        (self.on_watermark_change)(true);
                    }
                }
            }
            if state.pumping {
                return;
            }
            state.pumping = true;
        }
        self.spawn_pump();
    }

    fn spawn_pump(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            this.pump().await;
        });
    }

    async fn pump(&self) {
        loop {
            let (lane, frame) = {
                let mut state = self.state.lock();
                if state.destroyed {
                    state.pumping = false;
                    return;
                }
                let lane = if !state.control.is_empty() { Lane::Control } else { Lane::Data };
                let frame = match lane {
                    Lane::Control => state.control.pop_front().map(|frame| {
                        state.control_bytes -= frame.len();
                        frame
                    }),
                    Lane::Data => state.data.pop_front().map(|frame| {
                        state.data_bytes -= frame.len();
                        frame
                    }),
                };
                let Some(frame) = frame else {
                    state.pumping = false;
                    return;
                };
                if state.paused && state.data_bytes <= self.watermark_bytes / 2 {
                    state.paused = false;
                    (self.on_watermark_change)(false);
                }
                (lane, frame)
            };
            if !self.output.write(frame).await {
                self.output.drain().await;
            }
        }
    }

    fn destroy(&self) {
        let mut state = self.state.lock();
        state.destroyed = true;
        state.control.clear();
        state.data.clear();
        state.control_bytes = 0;
        state.data_bytes = 0;
    }

    fn clone(&self) -> Self {
        Self {
            output: self.output.clone(),
            watermark_bytes: self.watermark_bytes,
            on_fuse_breach: self.on_fuse_breach.clone(),
            on_watermark_change: self.on_watermark_change.clone(),
            state: self.state.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostLifecycle {
    PreInit,
    Ready,
}

struct HostState {
    lifecycle: HostLifecycle,
    in_flight: usize,
    waiting: VecDeque<Box<dyn FnOnce() + Send>>,
    shutting_down: bool,
    done: bool,
}

struct StdioHostInner {
    options: StdioHostOptions,
    decoder: Mutex<LineFrameDecoder>,
    writer: OutboundWriter,
    process_manager: Arc<ProcessManager>,
    fs_handler: Arc<FsHandler>,
    handlers: HashMap<String, Handler>,
    state: Mutex<HostState>,
    done_notify: tokio::sync::Notify,
}

#[derive(Clone)]
pub struct StdioHost {
    inner: Arc<StdioHostInner>,
}

impl StdioHost {
    pub fn new(options: StdioHostOptions) -> Self {
        let host_weak = Arc::new(Mutex::new(None::<std::sync::Weak<StdioHostInner>>));
        let process_manager = Arc::new(ProcessManager::new(
            Arc::new({
                let host_weak = host_weak.clone();
                move |method, params| {
                    if let Some(inner) = host_weak.lock().as_ref().and_then(|weak| weak.upgrade()) {
                        Self { inner }.send_notification(method, params);
                    }
                }
            }),
            HostProcessService::new(),
        ));
        let writer = OutboundWriter::new(
            options.output.clone(),
            8 * 1024 * 1024,
            Arc::new({
                let host_weak = host_weak.clone();
                move || {
                    if let Some(inner) = host_weak.lock().as_ref().and_then(|weak| weak.upgrade()) {
                        inner.options.log("outbound pending frame fuse breached");
                        Self { inner }.shutdown_soon("send buffer fuse");
                    }
                }
            }),
            Arc::new({
                let process_manager = process_manager.clone();
                move |paused| process_manager.set_output_paused(paused)
            }),
        );
        let fs_handler = Arc::new(FsHandler::new(HostFileSystem::new()));
        let mut handlers = HashMap::<String, Handler>::new();
        handlers.insert(PING_METHOD.to_string(), Arc::new(|_| async { Ok(json!({})) }.boxed()));
        handlers.insert(FS_READ_FILE_METHOD.to_string(), {
            let fs = fs_handler.clone();
            Arc::new(move |params| {
                let fs = fs.clone();
                async move { fs.read_file(params).await.map(|result| serde_json::to_value(result).unwrap()) }.boxed()
            })
        });
        handlers.insert(FS_WRITE_FILE_METHOD.to_string(), {
            let fs = fs_handler.clone();
            Arc::new(move |params| {
                let fs = fs.clone();
                async move { fs.write_file(params).await }.boxed()
            })
        });
        handlers.insert(FS_CREATE_DIRECTORY_METHOD.to_string(), {
            let fs = fs_handler.clone();
            Arc::new(move |params| {
                let fs = fs.clone();
                async move { fs.create_directory(params).await }.boxed()
            })
        });
        handlers.insert(FS_GET_METADATA_METHOD.to_string(), {
            let fs = fs_handler.clone();
            Arc::new(move |params| {
                let fs = fs.clone();
                async move { fs.get_metadata(params).await.map(|result| serde_json::to_value(result).unwrap()) }.boxed()
            })
        });
        handlers.insert(FS_CANONICALIZE_METHOD.to_string(), {
            let fs = fs_handler.clone();
            Arc::new(move |params| {
                let fs = fs.clone();
                async move { fs.canonicalize(params).await.map(|result| serde_json::to_value(result).unwrap()) }.boxed()
            })
        });
        handlers.insert(FS_READ_DIRECTORY_METHOD.to_string(), {
            let fs = fs_handler.clone();
            Arc::new(move |params| {
                let fs = fs.clone();
                async move { fs.read_directory(params).await.map(|result| serde_json::to_value(result).unwrap()) }.boxed()
            })
        });
        handlers.insert(FS_WALK_METHOD.to_string(), {
            let fs = fs_handler.clone();
            Arc::new(move |params| {
                let fs = fs.clone();
                async move { fs.walk(params).await.map(|result| serde_json::to_value(result).unwrap()) }.boxed()
            })
        });
        handlers.insert(FS_REMOVE_METHOD.to_string(), {
            let fs = fs_handler.clone();
            Arc::new(move |params| {
                let fs = fs.clone();
                async move { fs.remove(params).await }.boxed()
            })
        });
        handlers.insert(FS_RENAME_METHOD.to_string(), {
            let fs = fs_handler.clone();
            Arc::new(move |params| {
                let fs = fs.clone();
                async move { fs.rename(params).await }.boxed()
            })
        });
        handlers.insert(PROCESS_START_METHOD.to_string(), {
            let pm = process_manager.clone();
            Arc::new(move |params| {
                let pm = pm.clone();
                async move { pm.start(params).await.map(|result| serde_json::to_value(result).unwrap()) }.boxed()
            })
        });
        handlers.insert(PROCESS_WRITE_METHOD.to_string(), {
            let pm = process_manager.clone();
            Arc::new(move |params| {
                let pm = pm.clone();
                async move { pm.write(params).await }.boxed()
            })
        });
        handlers.insert(PROCESS_SIGNAL_METHOD.to_string(), {
            let pm = process_manager.clone();
            Arc::new(move |params| {
                let pm = pm.clone();
                async move { pm.signal(params).await }.boxed()
            })
        });
        let inner = Arc::new(StdioHostInner {
            writer,
            process_manager,
            fs_handler,
            handlers,
            options,
            decoder: Mutex::new(LineFrameDecoder::new()),
            state: Mutex::new(HostState {
                lifecycle: HostLifecycle::PreInit,
                in_flight: 0,
                waiting: VecDeque::new(),
                shutting_down: false,
                done: false,
            }),
            done_notify: tokio::sync::Notify::new(),
        });
        *host_weak.lock() = Some(Arc::downgrade(&inner));
        Self { inner }
    }

    pub fn done(&self) -> Shared<BoxFuture<'static, ()>> {
        let inner = self.inner.clone();
        async move {
            loop {
                let notified = inner.done_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if inner.state.lock().done {
                    return;
                }
                notified.await;
            }
        }
        .boxed()
        .shared()
    }

    pub fn start(&self) {
        let weak = Arc::downgrade(&self.inner);
        self.inner.options.input.on_data(Arc::new(move |chunk| {
            if let Some(inner) = weak.upgrade() {
                Self { inner }.on_data(&chunk);
            }
        }));
        let weak = Arc::downgrade(&self.inner);
        self.inner.options.input.on_end(Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                Self { inner }.shutdown_soon("stdin eof");
            }
        }));
        let weak = Arc::downgrade(&self.inner);
        self.inner.options.input.on_error(Arc::new(move |error| {
            if let Some(inner) = weak.upgrade() {
                inner.options.log(&format!("stdin error: {error}"));
                Self { inner }.shutdown_soon("stdin error");
            }
        }));
        let weak = Arc::downgrade(&self.inner);
        self.inner.options.output.on_error(Arc::new(move |error| {
            if let Some(inner) = weak.upgrade() {
                inner.options.log(&format!("stdout error: {error}"));
                Self { inner }.shutdown_soon("stdout error");
            }
        }));
    }

    fn on_data(&self, chunk: &[u8]) {
        if self.inner.state.lock().shutting_down {
            return;
        }
        let frames = match self.inner.decoder.lock().push(chunk) {
            Ok(frames) => frames,
            Err(error) => {
                self.violation(error.to_string());
                return;
            }
        };
        for frame in frames {
            if self.inner.state.lock().shutting_down {
                return;
            }
            self.on_frame(frame);
        }
    }

    fn on_frame(&self, frame: Value) {
        let message = match classify_message(frame) {
            Ok(message) => message,
            Err(error) => {
                self.violation(error.to_string());
                return;
            }
        };
        if self.inner.state.lock().lifecycle == HostLifecycle::PreInit {
            match message {
                JsonRpcMessage::Request(request) if request.method == INITIALIZE_METHOD => {
                    self.on_initialize(&request);
                }
                _ => self.violation("expected initialize as the first message".to_string()),
            }
            return;
        }
        match message {
            JsonRpcMessage::Notification(notification) => {
                if notification.method == PROCESS_FLOW_METHOD {
                    self.on_process_flow(notification.params);
                } else {
                    self.violation(format!("unexpected notification {}", notification.method));
                }
            }
            JsonRpcMessage::Request(request) => self.on_request(request),
            _ => self.violation("server received a response message".to_string()),
        }
    }

    fn on_initialize(&self, message: &JsonRpcRequest) {
        let params = message.params.as_ref().and_then(Value::as_object).cloned().unwrap_or_default();
        if !params.get("clientName").is_some_and(Value::is_string)
            || !params.get("clientVersion").is_some_and(Value::is_string)
        {
            self.violation("initialize params must carry clientName and clientVersion".to_string());
            return;
        }
        let result = InitializeResult {
            executor_version: self.inner.options.version.clone(),
            environment: self.inner.options.environment.clone(),
        };
        self.respond(&message.id, serde_json::to_value(result).unwrap(), Lane::Control);
        self.inner.state.lock().lifecycle = HostLifecycle::Ready;
    }

    fn on_process_flow(&self, params: Option<Value>) {
        let Some(params) = params.and_then(|params| params.as_object().cloned()) else {
            return;
        };
        let Some(process_id) = params.get("processId").and_then(Value::as_str) else {
            return;
        };
        let Some(paused) = params.get("paused").and_then(Value::as_bool) else {
            return;
        };
        self.inner.process_manager.set_client_paused(process_id, paused);
    }

    fn on_request(&self, message: JsonRpcRequest) {
        if message.method == INITIALIZE_METHOD {
            self.respond_error(&message.id, RpcError::new(RPC_INVALID_REQUEST, "already initialized", None));
            return;
        }
        let Some(handler) = self.inner.handlers.get(&message.method).cloned() else {
            self.respond_error(
                &message.id,
                RpcError::new(RPC_METHOD_NOT_FOUND, format!("unknown method {}", message.method), None),
            );
            return;
        };
        let lane = if matches!(message.method.as_str(), FS_READ_FILE_METHOD | FS_WALK_METHOD) {
            Lane::Data
        } else {
            Lane::Control
        };
        if CONTROL_METHODS.contains(message.method.as_str()) {
            self.run_handler(message.id, handler, message.params, lane, false);
            return;
        }
        if self.inner.state.lock().in_flight >= MAX_IN_FLIGHT_CALLS {
            let this = self.clone();
            self.inner.state.lock().waiting.push_back(Box::new(move || {
                this.run_handler(message.id, handler, message.params, lane, true);
            }));
            return;
        }
        self.inner.state.lock().in_flight += 1;
        self.run_handler(message.id, handler, message.params, lane, true);
    }

    fn run_handler(&self, id: RequestId, handler: Handler, params: Option<Value>, lane: Lane, held_slot: bool) {
        let this = self.clone();
        tokio::spawn(async move {
            let result = handler(params.unwrap_or(Value::Null)).await;
            match result {
                Ok(result) => this.respond(&id, if result.is_null() { json!({}) } else { result }, lane),
                Err(error) => this.respond_error(&id, to_rpc_error(error)),
            }
            if held_slot {
                this.release_slot();
            }
        });
    }

    fn release_slot(&self) {
        let next = {
            let mut state = self.inner.state.lock();
            if state.shutting_down {
                None
            } else {
                state.waiting.pop_front()
            }
        };
        if let Some(next) = next {
            next();
        } else {
            let mut state = self.inner.state.lock();
            state.in_flight = state.in_flight.saturating_sub(1);
        }
    }

    fn respond(&self, id: &RequestId, result: Value, lane: Lane) {
        match encode_frame(&json!({ "id": id, "result": result })) {
            Ok(frame) => self.inner.writer.enqueue(frame, lane),
            Err(_) => self.respond_error(
                id,
                RpcError::new(RPC_INTERNAL_ERROR, "response exceeds the frame cap", None),
            ),
        }
    }

    fn respond_error(&self, id: &RequestId, error: RpcError) {
        match encode_frame(&json!({ "id": id, "error": error.to_error_body() })) {
            Ok(frame) => self.inner.writer.enqueue(frame, Lane::Control),
            Err(_) => {
                self.shutdown_soon("failed to encode error response");
            }
        }
    }

    fn send_notification(&self, method: &str, params: Value) {
        match encode_frame(&json!({ "method": method, "params": params })) {
            Ok(frame) => self.inner.writer.enqueue(frame, Lane::Data),
            Err(_) => {
                self.shutdown_soon("failed to encode notification");
            }
        }
    }

    fn violation(&self, message: String) {
        self.inner.options.log(&format!("protocol violation: {message}"));
        self.shutdown_soon("protocol violation");
    }

    fn begin_shutdown(&self, reason: &str) -> bool {
        {
            let mut state = self.inner.state.lock();
            if state.shutting_down {
                return false;
            }
            state.shutting_down = true;
            state.waiting.clear();
        }
        self.inner.options.log(&format!("shutdown: {reason}"));
        true
    }

    fn shutdown_soon(&self, reason: &str) {
        if !self.begin_shutdown(reason) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.finish_shutdown().await;
        });
    }

    async fn finish_shutdown(&self) {
        self.inner.process_manager.terminate_all().await;
        self.inner.writer.destroy();
        self.inner.options.output.end();
        self.inner.state.lock().done = true;
        self.inner.done_notify.notify_waiters();
    }

    pub async fn shutdown(&self, reason: &str) -> Shared<BoxFuture<'static, ()>> {
        if !self.begin_shutdown(reason) {
            return self.done();
        }
        self.finish_shutdown().await;
        self.done()
    }
}

static CONTROL_METHODS: std::sync::LazyLock<HashSet<&'static str>> = std::sync::LazyLock::new(|| {
    HashSet::from([PING_METHOD, PROCESS_WRITE_METHOD, PROCESS_SIGNAL_METHOD])
});
