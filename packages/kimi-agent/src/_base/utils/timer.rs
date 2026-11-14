use std::sync::Arc;

use parking_lot::Mutex;

struct TimerInner {
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct IntervalTimer {
    inner: Arc<TimerInner>,
}

impl IntervalTimer {
    pub fn new(_unref: bool) -> Self {
        Self {
            inner: Arc::new(TimerInner {
                task: Mutex::new(None),
            }),
        }
    }

    pub fn cancel_and_set<F>(&self, callback: F, interval_ms: i64)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.dispose();
        let delay = std::time::Duration::from_millis(interval_ms.max(0) as u64);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(delay).await;
                callback();
            }
        });
        *self.inner.task.lock() = Some(task);
    }

    pub fn dispose(&self) {
        if let Some(task) = self.inner.task.lock().take() {
            task.abort();
        }
    }
}
