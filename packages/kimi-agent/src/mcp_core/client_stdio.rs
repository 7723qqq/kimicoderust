use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::{FutureExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::utils::abort::{AbortError, AbortSignal, abort_error};
use crate::_base::utils::proxy::{proxy_env_for_child, reconcile_child_no_proxy};
use crate::app::environment::environment::EnvironmentResolver;
use crate::environment::environment::{EnvironmentLease, LOCAL_ENVIRONMENT_ID};
use crate::environment::environment_registry::EnvironmentError;
use crate::mcp_core::errors::MCP_STARTUP_FAILED;
use crate::os::interface::host_process::{HostProcessError, IHostProcess, OS_PROCESS_SPAWN_FAILED};
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::{Error2, NOT_IMPLEMENTED_CODE};

use super::client_shared::*;
use super::config_schema::McpServerStdioConfig;
use super::types::{MCPClient, MCPToolDefinition, MCPToolResult};

const STDERR_BUFFER_CAPACITY: usize = 4 * 1024;
const STDIO_DRAIN_GRACE_MS: u64 = 100;
const STDIO_EOF_GRACE_MS: u64 = 200;
const STDIO_TERMINATION_GRACE_MS: u64 = 5_000;

pub struct StdioMcpClientOptions {
    pub client_name: Option<String>,
    pub client_version: Option<String>,
    pub startup_timeout_ms: Option<u64>,
    pub tool_call_timeout_ms: Option<u64>,
    pub default_cwd: Option<String>,
    pub environment_resolver: Arc<dyn EnvironmentResolver>,
    pub environment_id: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
struct SharedProcessError {
    message: Arc<String>,
    errno: Option<String>,
}

impl SharedProcessError {
    fn new(error: WireAsyncError) -> Self {
        Self {
            message: Arc::new(error.to_string()),
            errno: error_errno(error.as_ref()),
        }
    }
}

impl fmt::Display for SharedProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message.as_str())
    }
}

impl Error for SharedProcessError {}

struct StdioTransportState {
    process: Option<Arc<dyn IHostProcess>>,
    lease: Option<EnvironmentLease>,
    tracked: Option<Box<dyn IDisposable>>,
    started: bool,
    closed: bool,
    finished: bool,
    exit_error: Option<Shared<BoxFuture<'static, Option<SharedProcessError>>>>,
    stdout_ended: bool,
    stderr_ended: bool,
    on_close_fired: bool,
    drain_timer: Option<tokio::task::JoinHandle<()>>,
    on_error: Option<Arc<dyn Fn(WireAsyncError) + Send + Sync>>,
    on_close: Option<Arc<dyn Fn() + Send + Sync>>,
}

struct EnvironmentStdioTransport {
    config: McpServerStdioConfig,
    options: StdioMcpClientOptions,
    stderr: Arc<Mutex<BoundedTail>>,
    read_buffer: Arc<Mutex<StdioFrameBuffer>>,
    pending: RpcPending,
    next_id: Mutex<i64>,
    state: Arc<Mutex<StdioTransportState>>,
}

impl EnvironmentStdioTransport {
    fn on_error(&self, error: WireAsyncError) {
        if let Some(listener) = self.state.lock().on_error.clone() {
            listener(error);
        }
    }

    fn on_data(&self, chunk: &[u8]) {
        self.read_buffer.lock().append(chunk);
        loop {
            let line = self.read_buffer.lock().read_line();
            let Some(line) = line else {
                return;
            };
            match serde_json::from_str::<Value>(&line) {
                Ok(message) => {
                    self.pending.handle(&message);
                }
                Err(error) => self.on_error(error.into()),
            }
        }
    }

    fn finish(&self) {
        let (process, tracked, lease) = {
            let mut state = self.state.lock();
            if state.closed || state.finished {
                return;
            }
            state.finished = true;
            (state.process.take(), state.tracked.take(), state.lease.take())
        };
        if let Some(process) = process {
            tokio::spawn(process.dispose());
        }
        self.read_buffer.lock().clear();
        if let Some(tracked) = tracked {
            tracked.dispose();
        }
        if let Some(lease) = lease {
            lease.dispose();
        }
        self.fire_on_close();
    }

    fn fire_on_close(&self) {
        let should_fire = {
            let mut state = self.state.lock();
            if state.on_close_fired || state.closed || !state.finished {
                return;
            }
            if let Some(timer) = state.drain_timer.take() {
                timer.abort();
            }
            if state.stdout_ended && state.stderr_ended {
                state.on_close_fired = true;
                true
            } else {
                false
            }
        };
        if should_fire {
            if let Some(listener) = self.state.lock().on_close.clone() {
                listener();
            }
            return;
        }
        let state = self.state.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(STDIO_DRAIN_GRACE_MS)).await;
            let listener = {
                let mut state = state.lock();
                if state.on_close_fired || state.closed {
                    return;
                }
                state.on_close_fired = true;
                state.on_close.clone()
            };
            if let Some(listener) = listener {
                listener();
            }
        });
        self.state.lock().drain_timer = Some(timer);
    }

    async fn start(&self) -> Result<(), WireAsyncError> {
        {
            let state = self.state.lock();
            if state.started {
                return Err("Environment stdio transport is already started".into());
            }
            if state.closed {
                return Err("Environment stdio transport is closed".into());
            }
        }
        if self.config.executor.is_some_and(|executor| executor != super::config_schema::McpExecutor::Local) {
            return Err(Box::new(Error2::new(
                NOT_IMPLEMENTED_CODE,
                format!(
                    "MCP stdio executor '{}' is not supported; use environment_id to select the execution environment",
                    self.config.executor.unwrap().as_str()
                ),
            )));
        }
        self.state.lock().started = true;
        let lease = self
            .options
            .environment_resolver
            .acquire_when_ready(&self.options.environment_id, &["process"])
            .await?;
        let result = async {
            let host = lease.environment.host.as_ref().ok_or_else(|| {
                Box::new(EnvironmentError::new(
                    "environment.unavailable",
                    format!("environment {} is not ready", self.options.environment_id),
                    None,
                )) as WireAsyncError
            })?;
            let path = lease.environment.path.as_ref().ok_or_else(|| {
                Box::new(EnvironmentError::new(
                    "environment.unavailable",
                    format!("environment {} is not ready", self.options.environment_id),
                    None,
                )) as WireAsyncError
            })?;
            let base = if self.options.environment_id == LOCAL_ENVIRONMENT_ID {
                self.options.default_cwd.clone().unwrap_or_else(|| host.home_dir.clone())
            } else {
                host.cwd.clone().unwrap_or_else(|| host.home_dir.clone())
            };
            let cwd = match &self.config.cwd {
                Some(cwd) => path.resolve(&base, cwd),
                None => path.resolve_single(&base),
            };
            let env = if self.options.environment_id == LOCAL_ENVIRONMENT_ID {
                merge_stdio_env(self.config.env.as_ref(), &std::env::vars().map(|(key, value)| (key, Some(value))).collect())
            } else {
                self.config.env.clone().unwrap_or_default()
            };
            let process = lease.track(
                lease.environment.process.clone().unwrap()
                    .spawn(
                        self.config.command.as_ref().unwrap(),
                        self.config.args.as_deref().unwrap_or(&[]),
                        Some(&cwd),
                        env.as_ref(),
                    )
                    .await?,
                self.options.session_id.as_deref(),
            );
            self.state.lock().process = Some(process.clone());
            let tracked = lease.track(
                Arc::new(TransportDisposable {
                    transport: self.clone_handle(),
                }),
                self.options.session_id.as_deref(),
            );
            self.state.lock().tracked = Some(tracked);
            self.pump_stdout(process.clone());
            self.pump_stderr(process.clone());
            let this = self.clone_handle();
            let wait_process = process.clone();
            let exit_error = async move {
                wait_process.wait().await.err().map(SharedProcessError::new)
            }
            .boxed()
            .shared();
            self.state.lock().exit_error = Some(exit_error.clone());
            tokio::spawn(async move {
                if let Some(error) = exit_error.await {
                    this.on_error(Box::new(error));
                }
                this.finish();
            });
            Ok(())
        }
        .await;
        if let Err(error) = result {
            self.state.lock().lease = None;
            lease.dispose();
            return Err(error);
        }
        self.state.lock().lease = Some(lease);
        Ok(())
    }

    fn pump_stdout(&self, process: Arc<dyn IHostProcess>) {
        let this = self.clone_handle();
        tokio::spawn(async move {
            let mut stdout = process.stdout();
            while let Some(chunk) = stdout.next().await {
                match chunk {
                    Ok(chunk) => this.on_data(&chunk),
                    Err(error) => this.on_error(error),
                }
            }
            this.state.lock().stdout_ended = true;
            this.finish();
            this.fire_on_close();
        });
    }

    fn pump_stderr(&self, process: Arc<dyn IHostProcess>) {
        let this = self.clone_handle();
        tokio::spawn(async move {
            let mut stderr = process.stderr();
            while let Some(chunk) = stderr.next().await {
                match chunk {
                    Ok(chunk) => this.stderr.lock().push(&String::from_utf8_lossy(&chunk)),
                    Err(error) => this.on_error(error),
                }
            }
            this.state.lock().stderr_ended = true;
            this.fire_on_close();
        });
    }

    async fn close(&self) -> Result<(), WireAsyncError> {
        let process = {
            let mut state = self.state.lock();
            if state.closed {
                return Ok(());
            }
            state.closed = true;
            if let Some(timer) = state.drain_timer.take() {
                timer.abort();
            }
            state.process.take()
        };
        if let Some(process) = process {
            if let Err(error) = process.stdin_end().await {
                self.on_error(error);
            }
            let exited_on_eof = wait_for_exit(&process, STDIO_EOF_GRACE_MS).await;
            if !exited_on_eof {
                let _ = process.kill("SIGTERM").await;
                let exited = wait_for_exit(&process, STDIO_TERMINATION_GRACE_MS).await;
                if !exited && process.exit_code().is_none() {
                    let _ = process.kill("SIGKILL").await;
                    let _ = process.wait().await;
                }
            }
            tokio::spawn(process.dispose());
        }
        self.read_buffer.lock().clear();
        let (tracked, lease) = {
            let mut state = self.state.lock();
            (state.tracked.take(), state.lease.take())
        };
        if let Some(tracked) = tracked {
            tracked.dispose();
        }
        if let Some(lease) = lease {
            lease.dispose();
        }
        let listener = {
            let mut state = self.state.lock();
            if state.on_close_fired {
                None
            } else {
                state.on_close_fired = true;
                state.on_close.clone()
            }
        };
        if let Some(listener) = listener {
            listener();
        }
        Ok(())
    }

    fn clone_handle(&self) -> Self {
        Self {
            config: self.config.clone(),
            options: StdioMcpClientOptions {
                client_name: self.options.client_name.clone(),
                client_version: self.options.client_version.clone(),
                startup_timeout_ms: self.options.startup_timeout_ms,
                tool_call_timeout_ms: self.options.tool_call_timeout_ms,
                default_cwd: self.options.default_cwd.clone(),
                environment_resolver: self.options.environment_resolver.clone(),
                environment_id: self.options.environment_id.clone(),
                session_id: self.options.session_id.clone(),
            },
            stderr: self.stderr.clone(),
            read_buffer: self.read_buffer.clone(),
            pending: self.pending.clone(),
            next_id: Mutex::new(0),
            state: self.state.clone(),
        }
    }
}

struct TransportDisposable {
    transport: EnvironmentStdioTransport,
}

impl IDisposable for TransportDisposable {
    fn dispose(&self) {
        tokio::spawn(self.transport.close());
    }
}

#[async_trait]
impl McpTransport for EnvironmentStdioTransport {
    async fn start(&self) -> Result<(), WireAsyncError> {
        EnvironmentStdioTransport::start(self).await
    }

    async fn request(&self, method: &str, params: Option<Value>, options: Option<McpRequestOptions>) -> Result<Value, WireAsyncError> {
        let process = self.state.lock().process.clone();
        let Some(process) = process.filter(|_| !self.state.lock().closed) else {
            return Err("Environment stdio transport is not running".into());
        };
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
        let data = serde_json::to_string(&body)? + "\n";
        let this = self.clone_handle();
        let mut write = tokio::spawn(async move {
            let result = process.stdin_write(data.as_bytes()).await;
            if let Err(error) = &result {
                this.on_error(error.to_string().into());
            }
            result
        });
        let signal = options.and_then(|options| options.signal);
        if let Some(signal) = &signal {
            tokio::select! {
                result = &mut write => match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        self.pending.cancel(id);
                        return Err(error);
                    }
                    Err(error) => std::panic::resume_unwind(error.into_panic()),
                },
                result = &mut receiver => {
                    return result.unwrap_or_else(|_| Err("stdio request closed".into()));
                }
                _ = signal.cancelled() => {
                    self.pending.cancel(id);
                    return Err(stdio_request_abort_error(signal));
                }
            }
        } else {
            tokio::select! {
                result = &mut write => match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        self.pending.cancel(id);
                        return Err(error);
                    }
                    Err(error) => std::panic::resume_unwind(error.into_panic()),
                },
                result = &mut receiver => {
                    return result.unwrap_or_else(|_| Err("stdio request closed".into()));
                }
            }
        }
        match signal {
            Some(signal) => {
                tokio::select! {
                    result = receiver => result.unwrap_or_else(|_| Err("stdio request closed".into())),
                    _ = signal.cancelled() => {
                        self.pending.cancel(id);
                        Err(stdio_request_abort_error(&signal))
                    }
                }
            }
            None => receiver.await.unwrap_or_else(|_| Err("stdio request closed".into())),
        }
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), WireAsyncError> {
        let process = self.state.lock().process.clone();
        let Some(process) = process.filter(|_| !self.state.lock().closed) else {
            return Err("Environment stdio transport is not running".into());
        };
        let mut body = Map::new();
        body.insert("jsonrpc".to_string(), "2.0".into());
        body.insert("method".to_string(), method.into());
        if let Some(params) = params {
            body.insert("params".to_string(), params);
        }
        let data = serde_json::to_string(&body)? + "\n";
        if let Err(error) = process.stdin_write(data.as_bytes()).await {
            self.on_error(error.to_string().into());
            return Err(error);
        }
        Ok(())
    }

    async fn close(&self) -> Result<(), WireAsyncError> {
        EnvironmentStdioTransport::close(self).await
    }

    async fn exit_error(&self) -> Result<Option<WireAsyncError>, WireAsyncError> {
        let future = self.state.lock().exit_error.clone();
        let Some(future) = future else {
            return Ok(None);
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), future).await;
        Ok(result.ok().flatten().map(|error| Box::new(error) as WireAsyncError))
    }
}

pub struct StdioMcpClient {
    client: Arc<McpJsonRpcClient>,
    transport: Arc<EnvironmentStdioTransport>,
    config: McpServerStdioConfig,
    client_name: String,
    client_version: String,
    startup_timeout_ms: Option<u64>,
    tool_call_timeout_ms: Option<u64>,
    state: Arc<Mutex<StdioClientState>>,
}

struct StdioClientState {
    session_evidence_listener: Option<SessionEvidenceListener>,
    last_transport_error: Option<String>,
    pending_session_evidence: Option<SessionEvidence>,
    hooks_installed: bool,
}

impl StdioMcpClient {
    pub const STDERR_BUFFER_CAPACITY: usize = STDERR_BUFFER_CAPACITY;

    pub fn new(config: McpServerStdioConfig, options: StdioMcpClientOptions) -> Self {
        let transport = Arc::new(EnvironmentStdioTransport {
            config: config.clone(),
            options: StdioMcpClientOptions {
                client_name: options.client_name.clone(),
                client_version: options.client_version.clone(),
                startup_timeout_ms: options.startup_timeout_ms,
                tool_call_timeout_ms: options.tool_call_timeout_ms,
                default_cwd: options.default_cwd.clone(),
                environment_resolver: options.environment_resolver.clone(),
                environment_id: options.environment_id.clone(),
                session_id: options.session_id.clone(),
            },
            stderr: Arc::new(Mutex::new(BoundedTail::new(STDERR_BUFFER_CAPACITY))),
            read_buffer: Arc::new(Mutex::new(StdioFrameBuffer::default())),
            pending: RpcPending::new(),
            next_id: Mutex::new(0),
            state: Arc::new(Mutex::new(StdioTransportState {
                process: None,
                lease: None,
                tracked: None,
                started: false,
                closed: false,
                finished: false,
                exit_error: None,
                stdout_ended: false,
                stderr_ended: false,
                on_close_fired: false,
                drain_timer: None,
                on_error: None,
                on_close: None,
            })),
        });
        Self {
            client: Arc::new(McpJsonRpcClient::new(transport.clone())),
            transport,
            config,
            client_name: options.client_name.unwrap_or_else(|| KIMI_MCP_CLIENT_NAME.to_string()),
            client_version: options.client_version.unwrap_or_else(|| KIMI_MCP_CLIENT_VERSION.to_string()),
            startup_timeout_ms: options.startup_timeout_ms,
            tool_call_timeout_ms: options.tool_call_timeout_ms,
            state: Arc::new(Mutex::new(StdioClientState {
                session_evidence_listener: None,
                last_transport_error: None,
                pending_session_evidence: None,
                hooks_installed: false,
            })),
        }
    }

    pub async fn connect(&self) -> Result<(), WireAsyncError> {
        if self.client.closed() {
            return Err(Box::new(Error2::new(MCP_STARTUP_FAILED, "MCP stdio client is closed")));
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
            let transport_error = match self.transport.exit_error().await? {
                Some(error) => Some(error),
                None => self.state.lock().last_transport_error.clone().map(|error| error.into()),
            };
            return Err(classify_stdio_startup_error(error, transport_error));
        }
        if self.client.closed() {
            let _ = self.client.close().await;
            return Err(Box::new(Error2::new(
                MCP_STARTUP_FAILED,
                "MCP stdio client was closed during startup",
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

    pub fn stderr_snapshot(&self) -> String {
        self.transport.stderr.lock().snapshot()
    }

    fn install_transport_hooks(&self) {
        if self.state.lock().hooks_installed {
            return;
        }
        self.state.lock().hooks_installed = true;
        let weak = Arc::downgrade(&self.state);
        let transport = self.transport.clone();
        let client = self.client.clone();
        self.transport.state.lock().on_close = Some(Arc::new(move || {
            if client.closed() || !client.ready() {
                return;
            }
            let Some(state) = weak.upgrade() else {
                return;
            };
            let stderr = transport.stderr.lock().snapshot();
            let stderr = (!stderr.is_empty()).then_some(stderr);
            let mut state = state.lock();
            let evidence = SessionEvidence {
                kind: SessionEvidenceKind::Hard,
                reason: UnexpectedCloseReason {
                    error: state.last_transport_error.clone().map(|error| error.into()),
                    stderr,
                },
            };
            if let Some(listener) = state.session_evidence_listener.clone() {
                drop(state);
                listener(evidence);
            } else {
                state.pending_session_evidence = Some(evidence);
            }
        }));
        let weak = Arc::downgrade(&self.state);
        self.transport.state.lock().on_error = Some(Arc::new(move |error| {
            if let Some(state) = weak.upgrade() {
                state.lock().last_transport_error = Some(error.to_string().into());
            }
        }));
    }
}

#[async_trait]
impl MCPClient for StdioMcpClient {
    async fn list_tools(&self) -> Result<Vec<MCPToolDefinition>, WireAsyncError> {
        self.client.list_tools(self.startup_timeout_ms).await
    }

    async fn call_tool(
        &self,
        name: &str,
        args: HashMap<String, Value>,
        signal: Option<AbortSignal>,
    ) -> Result<MCPToolResult, WireAsyncError> {
        self.client
            .call_tool(
                name,
                args.into_iter().collect(),
                build_request_options(self.tool_call_timeout_ms, signal),
            )
            .await
    }

    async fn ping(&self, signal: Option<AbortSignal>) -> Result<(), WireAsyncError> {
        self.client
            .ping(build_request_options(Some(MCP_LIVENESS_PROBE_TIMEOUT_MS), signal))
            .await
    }
}

#[derive(Default)]
pub struct StdioFrameBuffer {
    parts: Vec<Vec<u8>>,
    scanned_parts: usize,
}

impl StdioFrameBuffer {
    pub fn append(&mut self, chunk: &[u8]) {
        if !chunk.is_empty() {
            self.parts.push(chunk.to_vec());
        }
    }

    pub fn read_line(&mut self) -> Option<String> {
        for index in self.scanned_parts..self.parts.len() {
            let part = &self.parts[index];
            let Some(end) = part.iter().position(|byte| *byte == b'\n') else {
                continue;
            };
            let line = self.take(index, end);
            return Some(line.strip_suffix('\r').unwrap_or(&line).to_string());
        }
        self.scanned_parts = self.parts.len();
        None
    }

    pub fn clear(&mut self) {
        self.parts.clear();
        self.scanned_parts = 0;
    }

    fn take(&mut self, index: usize, end: usize) -> String {
        let mut line = self.parts[..index].concat();
        line.extend_from_slice(&self.parts[index][..end]);
        let rest = self.parts[index][end + 1..].to_vec();
        let tail = self.parts[index + 1..].to_vec();
        self.parts = if rest.is_empty() {
            tail
        } else {
            std::iter::once(rest).chain(tail).collect()
        };
        self.scanned_parts = 0;
        String::from_utf8_lossy(&line).into_owned()
    }
}

pub struct BoundedTail {
    buffer: String,
    capacity: usize,
}

impl BoundedTail {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: String::new(),
            capacity,
        }
    }

    pub fn push(&mut self, chunk: &str) {
        self.buffer.push_str(chunk);
        while self.buffer.len() > self.capacity {
            let Some(ch) = self.buffer.chars().next() else {
                break;
            };
            self.buffer.drain(..ch.len_utf8());
        }
    }

    pub fn snapshot(&self) -> String {
        self.buffer.clone()
    }
}

pub fn merge_stdio_env(
    config_env: Option<&HashMap<String, String>>,
    parent_env: &HashMap<String, Option<String>>,
) -> HashMap<String, String> {
    let mut merged = parent_env
        .iter()
        .filter_map(|(key, value)| value.as_ref().map(|value| (key.clone(), value.clone())))
        .collect::<HashMap<_, _>>();
    if let Some(config_env) = config_env {
        merged.extend(config_env.clone());
    }
    merged.extend(proxy_env_for_child(&merged));
    reconcile_child_no_proxy(&mut merged, config_env);
    merged
}

fn stdio_request_abort_error(signal: &AbortSignal) -> WireAsyncError {
    match signal.reason() {
        Some(reason) => reason
            .downcast_ref::<AbortError>()
            .map(|error| Box::new(error.clone()) as WireAsyncError)
            .unwrap_or_else(|| reason.to_string().into()),
        None => Box::new(abort_error()),
    }
}

fn error_errno(error: &(dyn Error + 'static)) -> Option<String> {
    if let Some(error) = error.downcast_ref::<SharedProcessError>() {
        return error.errno.clone();
    }
    if let Some(error) = error.downcast_ref::<HostProcessError>() {
        if let Some(errno) = error2_errno(error) {
            return Some(errno);
        }
        return error.source().and_then(|source| error_errno(source));
    }
    if let Some(error) = error.downcast_ref::<Error2>() {
        if let Some(errno) = error2_errno(error) {
            return Some(errno);
        }
        return error.source().and_then(|source| error_errno(source));
    }
    if let Some(error) = error.downcast_ref::<std::io::Error>() {
        return io_error_errno(error);
    }
    None
}

fn error2_errno(error: &Error2) -> Option<String> {
    let value = error.details()?.get("errno")?;
    if let Some(errno) = value.as_str() {
        return Some(errno.to_string());
    }
    value.as_i64().and_then(io_errno_code)
}

fn io_error_errno(error: &std::io::Error) -> Option<String> {
    if let Some(errno) = error.raw_os_error().and_then(io_errno_code) {
        return Some(errno);
    }
    match error.kind() {
        std::io::ErrorKind::NotFound => Some("ENOENT".to_string()),
        std::io::ErrorKind::PermissionDenied => Some("EACCES".to_string()),
        _ => None,
    }
}

#[cfg(unix)]
fn io_errno_code(code: i64) -> Option<String> {
    let code = code as i32;
    if code == libc::ENOENT {
        return Some("ENOENT".to_string());
    }
    if code == libc::EACCES {
        return Some("EACCES".to_string());
    }
    if code == libc::ENOEXEC {
        return Some("ENOEXEC".to_string());
    }
    if code == libc::ENOTDIR {
        return Some("ENOTDIR".to_string());
    }
    if code == libc::EISDIR {
        return Some("EISDIR".to_string());
    }
    None
}

#[cfg(windows)]
fn io_errno_code(code: i64) -> Option<String> {
    match code {
        2 => Some("ENOENT".to_string()),
        5 => Some("EACCES".to_string()),
        193 => Some("ENOEXEC".to_string()),
        267 => Some("ENOTDIR".to_string()),
        _ => None,
    }
}

fn classify_stdio_startup_error(error: WireAsyncError, transport_error: Option<WireAsyncError>) -> WireAsyncError {
    const PERMANENT: &[&str] = &["ENOENT", "EACCES", "ENOEXEC", "ENOTDIR", "EISDIR"];
    let host_spawn = error.downcast_ref::<HostProcessError>().is_some_and(|error| {
        error.code().as_str() == OS_PROCESS_SPAWN_FAILED
    });
    if !host_spawn {
        if let Some(transport_error) = transport_error {
            let permanent = error_errno(transport_error.as_ref())
                .is_some_and(|errno| PERMANENT.contains(&errno.as_str()));
            if permanent {
                return Box::new(McpClassifiedStartupError::new(
                    McpFailureKind::Config,
                    transport_error.to_string(),
                    Some(transport_error),
                ));
            }
        }
        return error;
    }
    let errno = error_errno(error.as_ref());
    if errno.is_some_and(|errno| PERMANENT.contains(&errno.as_str())) {
        return Box::new(McpClassifiedStartupError::new(
            McpFailureKind::Config,
            error.to_string(),
            Some(error),
        ));
    }
    error
}

async fn wait_for_exit(process: &Arc<dyn IHostProcess>, timeout_ms: u64) -> bool {
    tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        process.wait(),
    )
    .await
    .is_ok()
}
