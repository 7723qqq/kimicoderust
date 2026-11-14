use std::sync::Arc;
use std::time::Instant;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};

pub trait IGoalDeadlineScheduler: Send + Sync {
    fn now(&self) -> i64;

    fn schedule(&self, delay_ms: i64, callback: Arc<dyn Fn() + Send + Sync>) -> DisposableRef;
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

impl IGoalDeadlineScheduler for GoalDeadlineSchedulerService {
    fn now(&self) -> i64 {
        self.started.elapsed().as_millis() as i64
    }

    fn schedule(&self, delay_ms: i64, callback: Arc<dyn Fn() + Send + Sync>) -> DisposableRef {
        let task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms.max(0) as u64)).await;
            callback();
        });
        Arc::new(ScheduledDeadline {
            task: std::sync::Mutex::new(Some(task)),
        })
    }
}

struct ScheduledDeadline {
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl IDisposable for ScheduledDeadline {
    fn dispose(&self) {
        let task = self
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(task) = task {
            task.abort();
        }
    }
}
