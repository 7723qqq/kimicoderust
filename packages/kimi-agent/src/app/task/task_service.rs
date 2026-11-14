use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures_util::FutureExt;
use parking_lot::Mutex;
use tokio::sync::{broadcast, oneshot};

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::utils::abort::AbortController;
use crate::app::scopes::LifecycleScope;

use super::task::{IDeferredHandle, ITaskHandle, ITaskService, TERMINAL_TASK_STATES, TaskCancelledError, TaskError, TaskFn, TaskResult, TaskState};

struct TaskEvents {
    state: Mutex<Option<broadcast::Sender<TaskState>>>,
    output: Mutex<Option<broadcast::Sender<String>>>,
}

impl TaskEvents {
    fn new() -> Self {
        let (state, _) = broadcast::channel(64);
        let (output, _) = broadcast::channel(64);
        Self {
            state: Mutex::new(Some(state)),
            output: Mutex::new(Some(output)),
        }
    }

    fn state_receiver(&self) -> broadcast::Receiver<TaskState> {
        self.state.lock().as_ref().map(broadcast::Sender::subscribe).unwrap_or_else(|| broadcast::channel(1).1)
    }

    fn output_receiver(&self) -> broadcast::Receiver<String> {
        self.output.lock().as_ref().map(broadcast::Sender::subscribe).unwrap_or_else(|| broadcast::channel(1).1)
    }

    fn dispose(&self) {
        self.state.lock().take();
        self.output.lock().take();
    }
}

struct RunHandle<T> {
    id: String,
    state: Mutex<TaskState>,
    abort_controller: AbortController,
    events: TaskEvents,
    result: Mutex<Option<TaskResult<T>>>,
    disposed: AtomicBool,
}

impl<T> RunHandle<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn new(id: String, function: TaskFn<T>) -> Arc<Self> {
        let handle = Arc::new(Self {
            id,
            state: Mutex::new(TaskState::Pending),
            abort_controller: AbortController::new(),
            events: TaskEvents::new(),
            result: Mutex::new(None),
            disposed: AtomicBool::new(false),
        });
        handle.transition(TaskState::Running);
        let output_handle = handle.clone();
        let output = Arc::new(move |data: String| {
            if !TERMINAL_TASK_STATES.contains(&output_handle.state()) && !output_handle.disposed.load(Ordering::Acquire)
                && let Some(sender) = output_handle.events.output.lock().as_ref()
            {
                let _ = sender.send(data);
            }
        });
        let future = function(handle.abort_controller.signal(), output);
        let task_handle = handle.clone();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let result = match future.await {
                Ok(value) => {
                    if task_handle.abort_controller.signal().aborted() {
                        task_handle.transition(TaskState::Cancelled);
                        Err(Arc::new(TaskCancelledError::new(task_handle.id.clone())) as TaskError)
                    } else {
                        task_handle.transition(TaskState::Completed);
                        Ok(value)
                    }
                }
                Err(error) => {
                    if task_handle.abort_controller.signal().aborted() {
                        task_handle.transition(TaskState::Cancelled);
                    } else {
                        task_handle.transition(TaskState::Failed);
                    }
                    Err(error)
                }
            };
            let _ = sender.send(result);
        });
        let id = handle.id.clone();
        let result = async move {
            receiver.await.unwrap_or_else(|_| Err(Arc::new(TaskRunnerError(format!("Task {id} ended without a result"))) as TaskError))
        }.boxed().shared();
        *handle.result.lock() = Some(result);
        handle
    }

    fn transition(&self, to: TaskState) {
        let mut state = self.state.lock();
        if TERMINAL_TASK_STATES.contains(&state) {
            return;
        }
        *state = to;
        if !self.disposed.load(Ordering::Acquire)
            && let Some(sender) = self.events.state.lock().as_ref()
        {
            let _ = sender.send(to);
        }
    }
}

impl<T> ITaskHandle<T> for RunHandle<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn state(&self) -> TaskState {
        *self.state.lock()
    }

    fn result(&self) -> TaskResult<T> {
        self.result.lock().clone().expect("task result initialized")
    }

    fn on_did_change_state(&self) -> broadcast::Receiver<TaskState> {
        self.events.state_receiver()
    }

    fn on_did_output(&self) -> broadcast::Receiver<String> {
        self.events.output_receiver()
    }

    fn cancel(&self) {
        if TERMINAL_TASK_STATES.contains(&self.state()) {
            return;
        }
        self.abort_controller.abort();
        self.transition(TaskState::Cancelled);
    }

    fn dispose(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.cancel();
        self.events.dispose();
    }
}

struct DeferHandle<T> {
    id: String,
    state: Mutex<TaskState>,
    events: TaskEvents,
    sender: Mutex<Option<oneshot::Sender<Result<T, TaskError>>>>,
    result: TaskResult<T>,
    disposed: AtomicBool,
}

impl<T> DeferHandle<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn new(id: String) -> Arc<Self> {
        let (sender, receiver) = oneshot::channel();
        let result = async move {
            receiver.await.unwrap_or_else(|_| Err(Arc::new(TaskRunnerError("Deferred task ended without a result".to_string())) as TaskError))
        }.boxed().shared();
        Arc::new(Self {
            id,
            state: Mutex::new(TaskState::Pending),
            events: TaskEvents::new(),
            sender: Mutex::new(Some(sender)),
            result,
            disposed: AtomicBool::new(false),
        })
    }

    fn transition(&self, to: TaskState) {
        let mut state = self.state.lock();
        if TERMINAL_TASK_STATES.contains(&state) {
            return;
        }
        *state = to;
        if !self.disposed.load(Ordering::Acquire)
            && let Some(sender) = self.events.state.lock().as_ref()
        {
            let _ = sender.send(to);
        }
    }
}

impl<T> ITaskHandle<T> for DeferHandle<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn state(&self) -> TaskState {
        *self.state.lock()
    }

    fn result(&self) -> TaskResult<T> {
        self.result.clone()
    }

    fn on_did_change_state(&self) -> broadcast::Receiver<TaskState> {
        self.events.state_receiver()
    }

    fn on_did_output(&self) -> broadcast::Receiver<String> {
        self.events.output_receiver()
    }

    fn cancel(&self) {
        if TERMINAL_TASK_STATES.contains(&self.state()) {
            return;
        }
        self.transition(TaskState::Cancelled);
        if let Some(sender) = self.sender.lock().take() {
            let _ = sender.send(Err(Arc::new(TaskCancelledError::new(self.id.clone())) as TaskError));
        }
    }

    fn dispose(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.cancel();
        self.events.dispose();
    }
}

impl<T> IDeferredHandle<T> for DeferHandle<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn resolve(&self, value: T) {
        if TERMINAL_TASK_STATES.contains(&self.state()) {
            return;
        }
        self.transition(TaskState::Completed);
        if let Some(sender) = self.sender.lock().take() {
            let _ = sender.send(Ok(value));
        }
    }

    fn reject(&self, reason: Option<TaskError>) {
        if TERMINAL_TASK_STATES.contains(&self.state()) {
            return;
        }
        self.transition(TaskState::Failed);
        if let Some(sender) = self.sender.lock().take() {
            let _ = sender.send(Err(reason.unwrap_or_else(|| Arc::new(TaskRejectedError) as TaskError)));
        }
    }
}

pub struct TaskService {
    next_id: AtomicU64,
}

impl TaskService {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(0),
        }
    }

    fn generate_id(&self) -> String {
        format!("task-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for TaskService {
    fn default() -> Self {
        Self::new()
    }
}

impl ITaskService for TaskService {
    fn run<T>(&self, function: TaskFn<T>) -> Arc<dyn ITaskHandle<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        RunHandle::new(self.generate_id(), function)
    }

    fn defer<T>(&self) -> Arc<dyn IDeferredHandle<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        DeferHandle::new(self.generate_id())
    }
}

#[derive(Debug)]
struct TaskRejectedError;

impl fmt::Display for TaskRejectedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Task rejected")
    }
}

impl Error for TaskRejectedError {}

#[derive(Debug)]
struct TaskRunnerError(String);

impl fmt::Display for TaskRunnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for TaskRunnerError {}

pub fn register_task_service() {
    register_scoped_service(LifecycleScope::App, ScopeActivation::OnScopeCreated, "task");
}
