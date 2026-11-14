use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt, select_all};
use indexmap::IndexMap;
use tokio::sync::Notify;

use crate::human::tool::executor::{TaskWaitInput, TaskWaitOutcome, WaitForTasks};
use crate::human::tool::machine::ToolActor;
use crate::wire::record::WireAsyncError;

pub type ToolActorRef = Arc<ToolActor>;

pub trait AgentSnapshotSource: Send + Sync {
    fn snapshot_background(&self) -> IndexMap<String, ToolActorRef>;
}

fn is_done(actor: &ToolActor) -> bool {
    matches!(actor.snapshot().1.as_str(), "succeeded" | "failed" | "aborted")
}

async fn wait_for_done(actor: ToolActorRef) {
    let notify = Arc::new(Notify::new());
    let listener_notify = notify.clone();
    let listener_actor = actor.clone();
    actor.subscribe(Arc::new(move |_| {
        if is_done(&listener_actor) {
            listener_notify.notify_one();
        }
    }));
    if is_done(&actor) {
        return;
    }
    notify.notified().await;
}

pub fn create_wait_for_tasks(self_: impl AgentSnapshotSource + 'static) -> WaitForTasks {
    let self_: Arc<dyn AgentSnapshotSource> = Arc::new(self_);
    Arc::new(move |input: TaskWaitInput| {
        let self_ = self_.clone();
        async move { wait_for_tasks(self_, input).await }.boxed()
    })
}

fn wait_for_tasks(
    self_: Arc<dyn AgentSnapshotSource>,
    input: TaskWaitInput,
) -> BoxFuture<'static, Result<TaskWaitOutcome, WireAsyncError>> {
    async move {
        let background = self_.snapshot_background();
        let mut targets = Vec::new();
        let mut unknown = Vec::new();
        let ids = input
            .task_id
            .map(|id| vec![id])
            .unwrap_or_else(|| background.keys().cloned().collect::<Vec<_>>());
        for id in ids {
            if let Some(actor) = background.get(&id) {
                targets.push((id, actor.clone()));
            } else {
                unknown.push(id);
            }
        }
        if targets.is_empty() {
            return Ok(TaskWaitOutcome {
                completed: Vec::new(),
                running: Vec::new(),
                unknown,
                timed_out: false,
            });
        }
        let waiters = targets
            .iter()
            .map(|(_, actor)| wait_for_done(actor.clone()).boxed())
            .collect::<Vec<_>>();
        let any = select_all(waiters);
        tokio::pin!(any);
        let timed_out = tokio::select! {
            _ = &mut any => false,
            _ = tokio::time::sleep(Duration::from_millis(input.timeout_ms)) => true,
        };
        let mut completed = Vec::new();
        let mut running = Vec::new();
        for (id, actor) in targets {
            if is_done(&actor) {
                completed.push(id);
            } else {
                running.push(id);
            }
        }
        Ok(TaskWaitOutcome {
            completed,
            running,
            unknown,
            timed_out,
        })
    }
    .boxed()
}
