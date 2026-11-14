use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::turn_loop::types::{ToolAccesses, tool_accesses_conflict};
use crate::wire::record::WireAsyncError;

pub struct ToolCallTaskStart<Result> {
    pub result: BoxFuture<'static, std::result::Result<Result, WireAsyncError>>,
}

#[derive(Clone)]
pub struct ToolCallTask<Result> {
    pub accesses: ToolAccesses,
    pub start: Arc<
        dyn Fn() -> BoxFuture<'static, std::result::Result<ToolCallTaskStart<Result>, WireAsyncError>>
            + Send
            + Sync,
    >,
}

struct ScheduledToolCallTask<Result> {
    id: u64,
    accesses: ToolAccesses,
    start: Arc<
        dyn Fn() -> BoxFuture<'static, std::result::Result<ToolCallTaskStart<Result>, WireAsyncError>>
            + Send
            + Sync,
    >,
}

impl<Result> Clone for ScheduledToolCallTask<Result> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            accesses: self.accesses.clone(),
            start: self.start.clone(),
        }
    }
}

struct ToolSchedulerState<Result> {
    active_tasks: Vec<ScheduledToolCallTask<Result>>,
    queued_tasks: Vec<ScheduledToolCallTask<Result>>,
    senders: HashMap<u64, oneshot::Sender<std::result::Result<Result, WireAsyncError>>>,
}

struct ToolSchedulerInner<Result> {
    state: Mutex<ToolSchedulerState<Result>>,
    next_id: AtomicU64,
}

pub struct ToolScheduler<Result> {
    inner: Arc<ToolSchedulerInner<Result>>,
}

impl<Result> Clone for ToolScheduler<Result> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<Result> ToolScheduler<Result>
where
    Result: Send + 'static,
{
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ToolSchedulerInner {
                state: Mutex::new(ToolSchedulerState {
                    active_tasks: Vec::new(),
                    queued_tasks: Vec::new(),
                    senders: HashMap::new(),
                }),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    pub fn add(
        &self,
        task: ToolCallTask<Result>,
    ) -> BoxFuture<'static, std::result::Result<Result, WireAsyncError>> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        let scheduled = ScheduledToolCallTask {
            id,
            accesses: task.accesses,
            start: task.start,
        };
        let start_now = {
            let mut state = self.inner.state.lock();
            state.senders.insert(id, sender);
            if Self::is_blocked(&scheduled, &state.active_tasks, &state.queued_tasks) {
                state.queued_tasks.push(scheduled.clone());
                false
            } else {
                state.active_tasks.push(scheduled.clone());
                true
            }
        };
        if start_now {
            self.start(scheduled);
        }
        async move {
            match receiver.await {
                Ok(result) => result,
                Err(_) => Err(Box::new(std::io::Error::other(
                    "tool scheduler task ended without settling",
                )) as WireAsyncError),
            }
        }
        .boxed()
    }

    fn is_blocked(
        task: &ScheduledToolCallTask<Result>,
        active: &[ScheduledToolCallTask<Result>],
        queued_before: &[ScheduledToolCallTask<Result>],
    ) -> bool {
        Self::conflicts_with_any(task, active) || Self::conflicts_with_any(task, queued_before)
    }

    fn conflicts_with_any(
        task: &ScheduledToolCallTask<Result>,
        candidates: &[ScheduledToolCallTask<Result>],
    ) -> bool {
        candidates.iter().any(|candidate| {
            tool_accesses_conflict(&task.accesses, &candidate.accesses)
        })
    }

    fn start(&self, task: ScheduledToolCallTask<Result>) {
        let this = self.clone();
        tokio::spawn(async move {
            let execution = async move {
                let started = (task.start)().await?;
                started.result.await
            };
            let outcome = match AssertUnwindSafe(execution).catch_unwind().await {
                Ok(outcome) => outcome,
                Err(error) => {
                    let message = error
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| error.downcast_ref::<&str>().map(|value| (*value).to_string()))
                        .unwrap_or_else(|| "unknown tool scheduler error".to_string());
                    Err(Box::new(std::io::Error::other(message)) as WireAsyncError)
                }
            };
            this.finish(task.id, outcome);
        });
    }

    fn finish(&self, id: u64, outcome: std::result::Result<Result, WireAsyncError>) {
        let sender = self.inner.state.lock().senders.remove(&id);
        if let Some(sender) = sender {
            let _ = sender.send(outcome);
        }
        let to_start = {
            let mut state = self.inner.state.lock();
            state.active_tasks.retain(|task| task.id != id);
            let mut to_start = Vec::new();
            let mut still_queued = Vec::new();
            for task in std::mem::take(&mut state.queued_tasks) {
                if Self::is_blocked(&task, &state.active_tasks, &still_queued) {
                    still_queued.push(task);
                } else {
                    state.active_tasks.push(task.clone());
                    to_start.push(task);
                }
            }
            state.queued_tasks = still_queued;
            to_start
        };
        for task in to_start {
            self.start(task);
        }
    }
}

impl<Result> Default for ToolScheduler<Result>
where
    Result: Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}
