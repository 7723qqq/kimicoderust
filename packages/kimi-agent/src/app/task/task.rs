use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, LazyLock};

use futures_util::future::{BoxFuture, Shared};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::_base::utils::abort::AbortSignal;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

pub static TERMINAL_TASK_STATES: LazyLock<HashSet<TaskState>> = LazyLock::new(|| {
    HashSet::from([TaskState::Completed, TaskState::Failed, TaskState::Cancelled])
});

#[derive(Debug, Clone)]
pub struct TaskCancelledError {
    pub task_id: String,
}

impl TaskCancelledError {
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
        }
    }
}

impl fmt::Display for TaskCancelledError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Task {} was cancelled", self.task_id)
    }
}

impl Error for TaskCancelledError {}

pub type TaskError = Arc<dyn Error + Send + Sync>;
pub type TaskResult<T> = Shared<BoxFuture<'static, Result<T, TaskError>>>;
pub type TaskOutput = Arc<dyn Fn(String) + Send + Sync>;
pub type TaskFn<T> = Box<dyn FnOnce(AbortSignal, TaskOutput) -> BoxFuture<'static, Result<T, TaskError>> + Send>;

pub trait ITaskHandle<T>: Send + Sync {
    fn id(&self) -> &str;
    fn state(&self) -> TaskState;
    fn result(&self) -> TaskResult<T>;
    fn on_did_change_state(&self) -> broadcast::Receiver<TaskState>;
    fn on_did_output(&self) -> broadcast::Receiver<String>;
    fn cancel(&self);
    fn dispose(&self);
}

pub trait IDeferredHandle<T>: ITaskHandle<T> {
    fn resolve(&self, value: T);
    fn reject(&self, reason: Option<TaskError>);
}

pub trait ITaskService: Send + Sync {
    fn run<T>(&self, function: TaskFn<T>) -> Arc<dyn ITaskHandle<T>>
    where
        T: Clone + Send + Sync + 'static;

    fn defer<T>(&self) -> Arc<dyn IDeferredHandle<T>>
    where
        T: Clone + Send + Sync + 'static;
}
