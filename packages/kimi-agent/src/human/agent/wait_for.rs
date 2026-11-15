use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt, select_all};
use indexmap::IndexMap;

use crate::human::tool::executor::{TaskWaitInput, TaskWaitOutcome, WaitForTasks};
use crate::human::xstate2::Actor;
use crate::wire::record::WireAsyncError;

pub type ToolActorRef = Arc<Actor>;

pub trait AgentSnapshotSource: Send + Sync {
    fn snapshot_background(&self) -> IndexMap<String, ToolActorRef>;
}

async fn wait_for_done(actor: ToolActorRef) {
    let _ = crate::human::xstate2::wait_for(&actor, |snapshot| snapshot.status == "done").await;
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
            if actor.get_snapshot().status == "done" {
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
