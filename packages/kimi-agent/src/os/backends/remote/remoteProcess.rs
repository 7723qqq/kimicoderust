use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::json;

use crate::os::interface::host_process::{
    HostOutputStream, HostProcessError, HostProcessOptions, HostProcessShell, IHostProcess,
    IHostProcessService, OS_PROCESS_KILL_FAILED, OS_PROCESS_SPAWN_FAILED,
    SHELL_GIT_BASH_NOT_FOUND,
};
use crate::os::backends::remote::rpc_domain_error::rpc_domain_error;
use crate::remote::protocol::channel::ExecChannel;
use crate::remote::protocol::codec::{decode_base64, encode_base64};
use crate::remote::protocol::errors::{ConnectionClosedError, RequestTimeoutError, RpcError};
use crate::remote::protocol::methods::*;
use crate::wire::record::WireAsyncError;

static HOST_PROCESS_CODES: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        OS_PROCESS_SPAWN_FAILED,
        OS_PROCESS_KILL_FAILED,
        SHELL_GIT_BASH_NOT_FOUND,
    ])
});

const OUTPUT_BUFFER_BYTES: usize = 256 * 1024;

pub fn to_remote_process_error(error: WireAsyncError) -> WireAsyncError {
    let Some(rpc) = error.downcast_ref::<RpcError>() else {
        return error;
    };
    if let Some(domain) = rpc_domain_error(rpc, &HOST_PROCESS_CODES) {
        return Box::new(HostProcessError::new(
            domain.domain_code,
            rpc.message.clone(),
            Some(domain.details),
        ));
    }
    error
}

struct PushStreamInner {
    queue: Mutex<VecDeque<Vec<u8>>>,
    queued_bytes: Mutex<usize>,
    closed: AtomicBool,
    taken: AtomicBool,
    notify: tokio::sync::Notify,
    on_stall_change: Arc<dyn Fn(bool) + Send + Sync>,
    stalled: AtomicBool,
}

#[derive(Clone)]
struct PushStream {
    inner: Arc<PushStreamInner>,
}

impl PushStream {
    fn new(on_stall_change: Arc<dyn Fn(bool) + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(PushStreamInner {
                queue: Mutex::new(VecDeque::new()),
                queued_bytes: Mutex::new(0),
                closed: AtomicBool::new(false),
                taken: AtomicBool::new(false),
                notify: tokio::sync::Notify::new(),
                on_stall_change,
                stalled: AtomicBool::new(false),
            }),
        }
    }

    fn push_chunk(&self, chunk: Vec<u8>) {
        let bytes = {
            let mut queue = self.inner.queue.lock();
            let mut queued_bytes = self.inner.queued_bytes.lock();
            queue.push_back(chunk);
            *queued_bytes = queued_bytes.saturating_add(queue.back().map_or(0, Vec::len));
            *queued_bytes
        };
        self.set_stalled(bytes >= OUTPUT_BUFFER_BYTES);
        self.inner.notify.notify_waiters();
    }

    fn end_stream(&self) {
        self.inner.closed.store(true, Ordering::Release);
        self.set_stalled(false);
        self.inner.notify.notify_waiters();
    }

    fn set_stalled(&self, stalled: bool) {
        if self.inner.stalled.swap(stalled, Ordering::AcqRel) != stalled {
            (self.inner.on_stall_change)(stalled);
        }
    }

    fn stream(&self) -> HostOutputStream {
        if self.inner.taken.swap(true, Ordering::AcqRel) {
            return Box::pin(futures_util::stream::empty());
        }
        let inner = self.inner.clone();
        Box::pin(futures_util::stream::unfold(inner, |inner| async move {
            loop {
                let chunk = {
                    let mut queue = inner.queue.lock();
                    let mut queued_bytes = inner.queued_bytes.lock();
                    queue.pop_front().map(|chunk| {
                        *queued_bytes = queued_bytes.saturating_sub(chunk.len());
                        (chunk, *queued_bytes)
                    })
                };
                if let Some((chunk, remaining)) = chunk {
                    if remaining < OUTPUT_BUFFER_BYTES {
                        let was_stalled = inner.stalled.swap(false, Ordering::AcqRel);
                        if was_stalled {
                            (inner.on_stall_change)(false);
                        }
                    }
                    return Some((
                        Ok::<String, WireAsyncError>(String::from_utf8_lossy(&chunk).into_owned()),
                        inner,
                    ));
                }
                if inner.closed.load(Ordering::Acquire) {
                    return None;
                }
                inner.notify.notified().await;
            }
        }))
    }
}

pub struct RemoteProcess {
    connection: Arc<dyn ExecChannel>,
    process_id: String,
    pid: AtomicU32,
    current_exit_code: Mutex<Option<i32>>,
    exit_notify: tokio::sync::Notify,
    write_chain: tokio::sync::Mutex<()>,
    stdout: PushStream,
    stderr: PushStream,
    stdout_stalled: AtomicBool,
    stderr_stalled: AtomicBool,
    output_flow_paused: Mutex<bool>,
}

impl RemoteProcess {
    fn new(
        connection: Arc<dyn ExecChannel>,
        process_id: String,
        merge_stderr: bool,
    ) -> Arc<Self> {
        let process = Arc::new_cyclic(|weak| {
            let stdout_stall = {
                let weak = weak.clone();
                Arc::new(move |stalled: bool| {
                    if let Some(process) = weak.upgrade() {
                        process.stdout_stalled.store(stalled, Ordering::Relaxed);
                        process.update_output_flow();
                    }
                })
            };
            let stderr_stall = {
                let weak = weak.clone();
                Arc::new(move |stalled: bool| {
                    if let Some(process) = weak.upgrade() {
                        process.stderr_stalled.store(stalled, Ordering::Relaxed);
                        process.update_output_flow();
                    }
                })
            };
            let stdout = PushStream::new(stdout_stall);
            let stderr = if merge_stderr {
                stdout.clone()
            } else {
                PushStream::new(stderr_stall)
            };
            Self {
                connection,
                process_id,
                pid: AtomicU32::new(0),
                current_exit_code: Mutex::new(None),
                exit_notify: tokio::sync::Notify::new(),
                write_chain: tokio::sync::Mutex::new(()),
                stdout,
                stderr,
                stdout_stalled: AtomicBool::new(false),
                stderr_stalled: AtomicBool::new(false),
                output_flow_paused: Mutex::new(false),
            }
        });
        process
    }

    pub fn attach(&self, pid: u32) {
        self.pid.store(pid, Ordering::Relaxed);
    }

    pub fn on_output(&self, stream: ProcessOutputStream, chunk: Vec<u8>) {
        if stream == ProcessOutputStream::Stderr {
            self.stderr.push_chunk(chunk);
        } else {
            self.stdout.push_chunk(chunk);
        }
    }

    fn update_output_flow(&self) {
        let stalled = self.stdout_stalled.load(Ordering::Relaxed)
            || self.stderr_stalled.load(Ordering::Relaxed);
        let mut paused = self.output_flow_paused.lock();
        if *paused == stalled {
            return;
        }
        *paused = stalled;
        self.connection.notify(
            PROCESS_FLOW_METHOD,
            Some(json!({ "processId": self.process_id, "paused": stalled })),
        );
    }

    pub fn on_exited(&self, exit_code: i32) {
        let mut current = self.current_exit_code.lock();
        if current.is_some() {
            return;
        }
        *current = Some(exit_code);
        drop(current);
        self.exit_notify.notify_waiters();
    }

    pub fn on_closed(&self) {
        self.stdout.end_stream();
        self.stderr.end_stream();
    }

    pub fn on_connection_close(&self) {
        self.on_exited(-1);
        self.on_closed();
    }
}

#[async_trait]
impl IHostProcess for RemoteProcess {
    fn pid(&self) -> u32 {
        self.pid.load(Ordering::Relaxed)
    }

    fn exit_code(&self) -> Option<i32> {
        *self.current_exit_code.lock()
    }

    fn stdout(&self) -> HostOutputStream {
        self.stdout.stream()
    }

    fn stderr(&self) -> HostOutputStream {
        self.stderr.stream()
    }

    async fn write_stdin(&self, data: &[u8]) -> Result<(), WireAsyncError> {
        let _guard = self.write_chain.lock().await;
        self.connection
            .call(
                PROCESS_WRITE_METHOD,
                Some(json!({
                    "processId": self.process_id,
                    "chunkBase64": encode_base64(data),
                })),
            )
            .await?;
        Ok(())
    }

    async fn close_stdin(&self) -> Result<(), WireAsyncError> {
        let _guard = self.write_chain.lock().await;
        self.connection
            .call(
                PROCESS_WRITE_METHOD,
                Some(json!({
                    "processId": self.process_id,
                    "chunkBase64": "",
                    "eof": true,
                })),
            )
            .await?;
        Ok(())
    }

    async fn wait(&self) -> Result<i32, WireAsyncError> {
        loop {
            if let Some(code) = *self.current_exit_code.lock() {
                return Ok(code);
            }
            self.exit_notify.notified().await;
        }
    }

    async fn kill(&self, signal: &str) -> Result<(), WireAsyncError> {
        let kind = match signal {
            "SIGINT" => ProcessSignalKind::Interrupt,
            "SIGTERM" => ProcessSignalKind::Terminate,
            "SIGKILL" => ProcessSignalKind::Kill,
            _ => {
                return Err(Box::new(HostProcessError::new(
                    OS_PROCESS_KILL_FAILED,
                    format!("unsupported signal for a remote process: {signal}"),
                    None,
                )));
            }
        };
        self.connection
            .call(
                PROCESS_SIGNAL_METHOD,
                Some(json!({ "processId": self.process_id, "signal": kind })),
            )
            .await?;
        Ok(())
    }

    async fn dispose(&self) {
        self.on_closed();
    }
}

pub struct RemoteProcessService {
    connection: Arc<dyn ExecChannel>,
    default_cwd: String,
    shell_path: String,
    processes: Arc<Mutex<HashMap<String, Arc<RemoteProcess>>>>,
}

impl RemoteProcessService {
    pub fn new(
        connection: Arc<dyn ExecChannel>,
        default_cwd: String,
        shell_path: String,
    ) -> Arc<Self> {
        let service = Arc::new(Self {
            connection: connection.clone(),
            default_cwd,
            shell_path,
            processes: Arc::new(Mutex::new(HashMap::new())),
        });
        let processes = service.processes.clone();
        connection.on_notification(PROCESS_OUTPUT_METHOD, Arc::new(move |params| {
            if let Ok(notification) = serde_json::from_value::<ProcessOutputNotification>(params)
                && let Some(process) = processes.lock().get(&notification.process_id).cloned()
            {
                process.on_output(notification.stream, decode_base64(&notification.chunk_base64));
            }
        }));
        let processes = service.processes.clone();
        connection.on_notification(PROCESS_EXITED_METHOD, Arc::new(move |params| {
            if let Ok(notification) = serde_json::from_value::<ProcessExitedNotification>(params)
                && let Some(process) = processes.lock().get(&notification.process_id).cloned()
            {
                process.on_exited(notification.exit_code);
            }
        }));
        let processes = service.processes.clone();
        connection.on_notification(PROCESS_CLOSED_METHOD, Arc::new(move |params| {
            if let Ok(notification) = serde_json::from_value::<ProcessClosedNotification>(params) {
                let process = processes.lock().get(&notification.process_id).cloned();
                if let Some(process) = process {
                    process.on_closed();
                    processes.lock().remove(&notification.process_id);
                }
            }
        }));
        let processes = service.processes.clone();
        connection.on_did_close(Arc::new(move |_| {
            for process in processes.lock().values() {
                process.on_connection_close();
            }
        }));
        service
    }

    pub async fn spawn_with_options(
        &self,
        command: &str,
        args: &[String],
        options: HostProcessOptions,
    ) -> Result<Arc<dyn IHostProcess>, WireAsyncError> {
        let argv = match &options.shell {
            None => std::iter::once(command.to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>(),
            Some(shell) => {
                let shell = match shell {
                    HostProcessShell::Bool(true) => self.shell_path.clone(),
                    HostProcessShell::Path(path) => path.clone(),
                    HostProcessShell::Bool(false) => command.to_string(),
                };
                if matches!(options.shell, Some(HostProcessShell::Bool(false))) {
                    std::iter::once(command.to_string())
                        .chain(args.iter().cloned())
                        .collect()
                } else {
                    vec![
                        shell,
                        "-c".to_string(),
                        std::iter::once(command)
                            .chain(args.iter().map(String::as_str))
                            .collect::<Vec<_>>()
                            .join(" "),
                    ]
                }
            }
        };
        let process_id = uuid::Uuid::new_v4().to_string();
        let process = RemoteProcess::new(
            self.connection.clone(),
            process_id.clone(),
            options.merge_stderr.unwrap_or(false),
        );
        self.processes
            .lock()
            .insert(process_id.clone(), process.clone());
        let result = self
            .connection
            .call(
                PROCESS_START_METHOD,
                Some(json!({
                    "processId": process_id,
                    "argv": argv,
                    "cwd": options.cwd.unwrap_or_else(|| self.default_cwd.clone()),
                    "env": options.env,
                })),
            )
            .await;
        match result {
            Ok(result) => {
                let result = serde_json::from_value::<ProcessStartResult>(result)?;
                process.attach(result.pid);
                Ok(process)
            }
            Err(error) => {
                self.processes.lock().remove(&process_id);
                if error.downcast_ref::<RequestTimeoutError>().is_some() {
                    self.connection.notify(
                        PROCESS_SIGNAL_METHOD,
                        Some(json!({ "processId": process_id, "signal": ProcessSignalKind::Terminate })),
                    );
                }
                if error.downcast_ref::<ConnectionClosedError>().is_some() {
                    process.on_connection_close();
                }
                Err(to_remote_process_error(error))
            }
        }
    }
}

#[async_trait]
impl IHostProcessService for RemoteProcessService {
    async fn spawn(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: Option<&HashMap<String, String>>,
    ) -> Result<Arc<dyn IHostProcess>, WireAsyncError> {
        self.spawn_with_options(
            command,
            args,
            HostProcessOptions {
                cwd: cwd.map(str::to_string),
                env: env.cloned(),
                ..Default::default()
            },
        )
        .await
    }
}
