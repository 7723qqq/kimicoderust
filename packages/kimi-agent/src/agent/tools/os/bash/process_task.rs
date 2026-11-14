use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;

use crate::os::interface::host_process::{HostOutputStream, IHostProcess};
use crate::rpc::types::BoxFuture;
use crate::storage::task_runner::TaskStatus;
use crate::subagent::types::ParentCancel;
use crate::wire::record::WireAsyncError;

pub const STREAM_DRAIN_GRACE_MS: u64 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessTaskOutputKind {
    Stdout,
    Stderr,
}

impl ProcessTaskOutputKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

pub type ProcessTaskOutputCallback =
    Arc<dyn Fn(ProcessTaskOutputKind, &str) + Send + Sync + 'static>;
pub type ProcessTaskRelease = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTaskInfo {
    pub kind: &'static str,
    pub command: String,
    pub pid: u32,
    pub exit_code: Option<i32>,
    pub parent_tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskSettlement {
    pub status: TaskStatus,
    pub stop_reason: Option<String>,
}

pub trait AgentTaskSink: Send + Sync {
    fn signal(&self) -> ParentCancel;
    fn append_output(&self, text: &str);
    fn settle(&self, settlement: AgentTaskSettlement) -> BoxFuture<'static, ()>;
}

pub struct ProcessTask {
    pub proc: Arc<dyn IHostProcess>,
    pub command: String,
    pub description: String,
    pub parent_tool_call_id: Option<String>,
    on_output: Option<ProcessTaskOutputCallback>,
    release: Mutex<Option<ProcessTaskRelease>>,
    exit_code: Arc<Mutex<Option<i32>>>,
}

impl ProcessTask {
    pub fn new(
        proc: Arc<dyn IHostProcess>,
        command: impl Into<String>,
        description: impl Into<String>,
        on_output: Option<ProcessTaskOutputCallback>,
        release: Option<ProcessTaskRelease>,
        parent_tool_call_id: Option<String>,
    ) -> Self {
        Self {
            proc,
            command: command.into(),
            description: description.into(),
            parent_tool_call_id,
            on_output,
            release: Mutex::new(release),
            exit_code: Arc::new(Mutex::new(None)),
        }
    }

    pub fn kind(&self) -> &'static str {
        "process"
    }

    pub fn id_prefix(&self) -> &'static str {
        "bash"
    }

    pub async fn start(&self, sink: Arc<dyn AgentTaskSink>) {
        let signal = sink.signal();
        let stdout = observe_process_stream(
            self.proc.stdout(),
            ProcessTaskOutputKind::Stdout,
            sink.clone(),
            signal.clone(),
            self.on_output.clone(),
        );
        let stderr = observe_process_stream(
            self.proc.stderr(),
            ProcessTaskOutputKind::Stderr,
            sink.clone(),
            signal.clone(),
            self.on_output.clone(),
        );
        let mut stream_drained = tokio::spawn(async move {
            futures_util::future::try_join(stdout, stderr)
                .await
                .map(|_| ())
        });
        let wait = self.proc.wait();
        tokio::pin!(wait);
        let waited = if signal.triggered() {
            let _ = self.proc.kill("SIGTERM").await;
            wait.await
        } else {
            tokio::select! {
                result = &mut wait => result,
                () = signal.wait() => {
                    let _ = self.proc.kill("SIGTERM").await;
                    wait.await
                }
            }
        };
        let settlement = match waited {
            Ok(exit_code) => match wait_for_stream_drain(&mut stream_drained).await {
                Ok(()) => {
                    *self.exit_code.lock().unwrap_or_else(|error| error.into_inner()) =
                        Some(exit_code);
                    AgentTaskSettlement {
                        status: if signal.triggered() {
                            TaskStatus::Killed
                        } else if exit_code == 0 {
                            TaskStatus::Completed
                        } else {
                            TaskStatus::Failed
                        },
                        stop_reason: None,
                    }
                }
                Err(error) => {
                    *self.exit_code.lock().unwrap_or_else(|error| error.into_inner()) =
                        self.proc.exit_code();
                    AgentTaskSettlement {
                        status: if signal.triggered() {
                            TaskStatus::Killed
                        } else {
                            TaskStatus::Failed
                        },
                        stop_reason: (!signal.triggered()).then(|| error.to_string()),
                    }
                }
            }
            Err(error) => {
                wait_for_stream_drain_settled(&mut stream_drained).await;
                *self.exit_code.lock().unwrap_or_else(|error| error.into_inner()) =
                    self.proc.exit_code();
                AgentTaskSettlement {
                    status: if signal.triggered() {
                        TaskStatus::Killed
                    } else {
                        TaskStatus::Failed
                    },
                    stop_reason: (!signal.triggered()).then(|| error.to_string()),
                }
            }
        };
        self.dispose_process().await;
        sink.settle(settlement).await;
    }

    pub async fn force_stop(&self) -> Result<(), WireAsyncError> {
        let result = if self.proc.exit_code().is_none() {
            self.proc.kill("SIGKILL").await
        } else {
            Ok(())
        };
        self.dispose_process().await;
        result
    }

    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn to_info(&self) -> ProcessTaskInfo {
        ProcessTaskInfo {
            kind: "process",
            command: self.command.clone(),
            pid: self.proc.pid(),
            exit_code: self.exit_code(),
            parent_tool_call_id: self.parent_tool_call_id.clone(),
        }
    }

    async fn dispose_process(&self) {
        self.proc.dispose().await;
        if let Some(release) = self
            .release
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            release();
        }
    }
}

async fn wait_for_stream_drain(
    drained: &mut tokio::task::JoinHandle<Result<(), WireAsyncError>>,
) -> Result<(), WireAsyncError> {
    match tokio::time::timeout(Duration::from_millis(STREAM_DRAIN_GRACE_MS), drained).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(std::io::Error::other(error.to_string()).into()),
        Err(_) => Ok(()),
    }
}

async fn wait_for_stream_drain_settled(
    drained: &mut tokio::task::JoinHandle<Result<(), WireAsyncError>>,
) {
    let _ = wait_for_stream_drain(drained).await;
}

async fn observe_process_stream(
    mut stream: HostOutputStream,
    kind: ProcessTaskOutputKind,
    sink: Arc<dyn AgentTaskSink>,
    signal: ParentCancel,
    on_output: Option<ProcessTaskOutputCallback>,
) -> Result<(), WireAsyncError> {
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                if chunk.is_empty() {
                    continue;
                }
                sink.append_output(&chunk);
                if !signal.triggered()
                    && let Some(on_output) = on_output.as_ref()
                {
                    on_output(kind, &chunk);
                }
            }
            Err(error) => {
                if signal.triggered() {
                    return Ok(());
                }
                return Err(error);
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessTaskResult {
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExitError {
    pub exit_code: Option<i32>,
}

impl std::fmt::Display for ProcessExitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.exit_code {
            Some(code) => write!(formatter, "Process exited with code {code}"),
            None => formatter.write_str("Process exited with code null"),
        }
    }
}

impl std::error::Error for ProcessExitError {}

#[derive(Debug)]
pub enum ProcessExecutorError {
    Aborted,
    Exit(ProcessExitError),
    Process(String),
}

impl std::fmt::Display for ProcessExecutorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Aborted => formatter.write_str("Process execution aborted"),
            Self::Exit(error) => error.fmt(formatter),
            Self::Process(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for ProcessExecutorError {}

struct ExecutorSink {
    signal: ParentCancel,
    output: Arc<dyn Fn(&str) + Send + Sync>,
    settlement: Mutex<Option<AgentTaskSettlement>>,
}

impl AgentTaskSink for ExecutorSink {
    fn signal(&self) -> ParentCancel {
        self.signal.clone()
    }

    fn append_output(&self, text: &str) {
        (self.output)(text);
    }

    fn settle(&self, settlement: AgentTaskSettlement) -> BoxFuture<'static, ()> {
        *self
            .settlement
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(settlement);
        Box::pin(async {})
    }
}

pub fn create_process_executor(
    proc: Arc<dyn IHostProcess>,
    on_output: Option<ProcessTaskOutputCallback>,
) -> impl FnOnce(
    ParentCancel,
    Arc<dyn Fn(&str) + Send + Sync>,
) -> BoxFuture<'static, Result<ProcessTaskResult, ProcessExecutorError>> {
    move |signal, output| {
        Box::pin(async move {
            let sink = Arc::new(ExecutorSink {
                signal,
                output,
                settlement: Mutex::new(None),
            });
            let task = ProcessTask::new(proc, "", "", on_output, None, None);
            task.start(sink.clone()).await;
            let settlement = sink
                .settlement
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
                .expect("process task settles before its executor returns");
            match settlement.status {
                TaskStatus::Completed => Ok(ProcessTaskResult {
                    exit_code: task.exit_code(),
                }),
                TaskStatus::Killed => Err(ProcessExecutorError::Aborted),
                TaskStatus::Failed if settlement.stop_reason.is_none() => {
                    Err(ProcessExecutorError::Exit(ProcessExitError {
                        exit_code: task.exit_code(),
                    }))
                }
                _ => Err(ProcessExecutorError::Process(
                    settlement
                        .stop_reason
                        .unwrap_or_else(|| "Process execution failed".to_string()),
                )),
            }
        })
    }
}
