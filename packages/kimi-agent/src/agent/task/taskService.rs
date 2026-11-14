use std::collections::VecDeque;
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::_base::utils::abort::AbortController;
use crate::app::task::task::ITaskHandle;
use crate::os::interface::host_timers::IHostTimerHandle;
use crate::wire::record::WireAsyncError;

use super::task::RegisterAgentTaskOptions;
use super::types::{
    AgentTask, AgentTaskInfo, AgentTaskInfoBase, AgentTaskStatus,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundTaskReleaseReason {
    Detached,
    TimeoutDetached,
    Terminal,
}

impl ForegroundTaskReleaseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Detached => "detached",
            Self::TimeoutDetached => "timeout_detached",
            Self::Terminal => "terminal",
        }
    }
}

pub struct ForegroundRelease {
    sender: Option<oneshot::Sender<ForegroundTaskReleaseReason>>,
    future: Shared<BoxFuture<'static, ForegroundTaskReleaseReason>>,
}

impl ForegroundRelease {
    pub fn resolve(&mut self, reason: ForegroundTaskReleaseReason) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(reason);
        }
    }

    pub fn future(&self) -> Shared<BoxFuture<'static, ForegroundTaskReleaseReason>> {
        self.future.clone()
    }
}

pub fn create_foreground_release() -> ForegroundRelease {
    let (sender, receiver) = oneshot::channel();
    let future = Box::pin(async move {
        receiver
            .await
            .unwrap_or(ForegroundTaskReleaseReason::Terminal)
    })
    .boxed()
    .shared();
    ForegroundRelease {
        sender: Some(sender),
        future,
    }
}

pub fn ready_future<T>(value: T) -> Shared<BoxFuture<'static, T>>
where
    T: Clone + Send + Sync + 'static,
{
    async move { value }.boxed().shared()
}

pub struct TaskWriteQueue {
    tail: Shared<BoxFuture<'static, ()>>,
}

impl TaskWriteQueue {
    pub fn new() -> Self {
        Self {
            tail: ready_future(()),
        }
    }

    pub fn push(&mut self, operation: BoxFuture<'static, ()>) -> Shared<BoxFuture<'static, ()>> {
        let previous = self.tail.clone();
        self.tail = Box::pin(async move {
            previous.await;
            operation.await;
        })
        .boxed()
        .shared();
        self.tail.clone()
    }

    pub fn future(&self) -> Shared<BoxFuture<'static, ()>> {
        self.tail.clone()
    }
}

impl Default for TaskWriteQueue {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ManagedTask {
    pub task_id: String,
    pub task: Option<Arc<dyn AgentTask>>,
    pub handle: Option<Arc<dyn ITaskHandle<serde_json::Value>>>,
    pub to_info_fn: Option<Arc<dyn Fn(AgentTaskInfoBase) -> AgentTaskInfo + Send + Sync>>,
    pub force_stop_fn:
        Option<Arc<dyn Fn() -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync>>,
    pub on_detach_fn: Option<Arc<dyn Fn() + Send + Sync>>,
    pub description: Option<String>,
    pub output_chunks: VecDeque<String>,
    pub output_size_bytes: u64,
    pub retained_output_bytes: usize,
    pub output_limit_tripped: bool,
    pub status: AgentTaskStatus,
    pub options: RegisterAgentTaskOptions,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub foreground_release: Option<ForegroundRelease>,
    pub stop_reason: Option<String>,
    pub terminal_notification_suppressed: Option<bool>,
    pub terminal_fired: bool,
    pub abort_controller: Arc<AbortController>,
    pub foreground_signal_cleanup: Option<Box<dyn FnOnce() + Send>>,
    pub lifecycle: Shared<BoxFuture<'static, ()>>,
    pub persist_write_queue: TaskWriteQueue,
    pub output_write_queue: TaskWriteQueue,
    pub pending_output: Vec<String>,
    pub pending_output_bytes: usize,
    pub output_persist_started: bool,
    pub persistence_error: Arc<Mutex<Option<String>>>,
    pub timeout_handle: Option<IHostTimerHandle>,
    pub timed_out: bool,
    pub waiters: Vec<(u64, oneshot::Sender<()>)>,
    pub next_waiter_id: u64,
    pub handle_subscription: Option<CancellationToken>,
}

impl ManagedTask {
    pub fn new(
        task_id: String,
        mut options: RegisterAgentTaskOptions,
        started_at: f64,
    ) -> Self {
        let detached = options.detached.unwrap_or(true);
        let foreground_release = if detached {
            None
        } else {
            Some(create_foreground_release())
        };
        if detached {
            options.signal = None;
        }
        Self {
            task_id,
            task: None,
            handle: None,
            to_info_fn: None,
            force_stop_fn: None,
            on_detach_fn: None,
            description: None,
            output_chunks: VecDeque::new(),
            output_size_bytes: 0,
            retained_output_bytes: 0,
            output_limit_tripped: false,
            status: AgentTaskStatus::Running,
            options,
            started_at,
            ended_at: None,
            foreground_release,
            stop_reason: None,
            terminal_notification_suppressed: None,
            terminal_fired: false,
            abort_controller: Arc::new(AbortController::new()),
            foreground_signal_cleanup: None,
            lifecycle: ready_future(()),
            persist_write_queue: TaskWriteQueue::new(),
            output_write_queue: TaskWriteQueue::new(),
            pending_output: Vec::new(),
            pending_output_bytes: 0,
            output_persist_started: detached,
            persistence_error: Arc::new(Mutex::new(None)),
            timeout_handle: None,
            timed_out: false,
            waiters: Vec::new(),
            next_waiter_id: 1,
            handle_subscription: None,
        }
    }

    pub fn is_detached(&self) -> bool {
        self.foreground_release.is_none()
    }

    pub fn starts_detached(&self) -> bool {
        self.options.detached != Some(false)
    }
}

pub fn install_foreground_signal(entry: &mut ManagedTask, on_abort: Arc<dyn Fn() + Send + Sync>) {
    let Some(signal) = entry.options.signal.clone() else {
        return;
    };
    if signal.is_cancelled() {
        tokio::spawn(async move { on_abort() });
        return;
    }
    let handle = tokio::spawn(async move {
        signal.cancelled().await;
        on_abort();
    });
    entry.foreground_signal_cleanup = Some(Box::new(move || {
        handle.abort();
    }));
}

pub trait TaskLifecycle: Send + Sync {
    fn apply_detach_timeout(&self, entry: &SharedManagedTask);
    fn start_output_persist(&self, entry: &SharedManagedTask);
    fn persist_live(&self, entry: &SharedManagedTask);
    fn record_task_started(&self, entry: &SharedManagedTask);
    fn fire_change(&self, entry: &SharedManagedTask);
}

pub fn detach_entry(
    entry: &SharedManagedTask,
    via_timeout: bool,
    lifecycle: &dyn TaskLifecycle,
) -> bool {
    let (mut release, cleanup, on_detach) = {
        let mut entry = entry.lock();
        if entry.status.is_terminal()
            || entry.options.detachable == Some(false)
            || entry.foreground_release.is_none()
        {
            return false;
        }
        let release = entry.foreground_release.take().expect("release checked");
        let cleanup = entry.foreground_signal_cleanup.take();
        let on_detach = entry.on_detach_fn.clone();
        (release, cleanup, on_detach)
    };
    if let Some(cleanup) = cleanup {
        cleanup();
    }
    lifecycle.apply_detach_timeout(entry);
    if let Some(on_detach) = on_detach {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_detach()));
    }
    lifecycle.start_output_persist(entry);
    lifecycle.persist_live(entry);
    lifecycle.record_task_started(entry);
    lifecycle.fire_change(entry);
    release.resolve(if via_timeout {
        ForegroundTaskReleaseReason::TimeoutDetached
    } else {
        ForegroundTaskReleaseReason::Detached
    });
    true
}

pub fn terminate_release(entry: &mut ManagedTask) {
    if let Some(release) = entry.foreground_release.as_mut() {
        release.resolve(ForegroundTaskReleaseReason::Terminal);
    }
}

pub fn resolve_waiters(entry: &mut ManagedTask) {
    for (_, waiter) in entry.waiters.drain(..) {
        let _ = waiter.send(());
    }
}

pub type SharedManagedTask = Arc<Mutex<ManagedTask>>;
