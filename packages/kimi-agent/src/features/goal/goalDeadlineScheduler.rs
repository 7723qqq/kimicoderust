use std::sync::Arc;
use std::time::Instant;

pub trait GoalDeadlineScheduler: Send + Sync {
    fn now(&self) -> i64;

    fn schedule(&self, delay_ms: i64, callback: Arc<dyn Fn() + Send + Sync>) -> DeadlineHandle;
}

pub struct DeadlineHandle {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for DeadlineHandle {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub struct GoalDeadlineSchedulerService {
    started: Instant,
}

impl Default for GoalDeadlineSchedulerService {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl GoalDeadlineSchedulerService {
    pub fn new() -> Self {
        Self::default()
    }
}

impl GoalDeadlineScheduler for GoalDeadlineSchedulerService {
    fn now(&self) -> i64 {
        self.started.elapsed().as_millis() as i64
    }

    fn schedule(&self, delay_ms: i64, callback: Arc<dyn Fn() + Send + Sync>) -> DeadlineHandle {
        let task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms.max(0) as u64)).await;
            callback();
        });
        DeadlineHandle { task: Some(task) }
    }
}
