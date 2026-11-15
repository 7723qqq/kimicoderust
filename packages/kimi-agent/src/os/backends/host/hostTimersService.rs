use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::app::scopes::LifecycleScope;
use crate::os::interface::host_timers::{IHostTimerHandle, IHostTimers};

const MAX_TIMER_DELAY_MS: u64 = 0x7fffffff;

#[derive(Clone)]
pub struct HostTimersService {
    tasks: Arc<Mutex<HashMap<u64, tokio::task::JoinHandle<()>>>>,
    next_id: Arc<AtomicU64>,
}

impl HostTimersService {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(0)),
        }
    }

    fn schedule(
        &self,
        callback: Arc<dyn Fn() + Send + Sync>,
        timeout_ms: u64,
    ) -> IHostTimerHandle {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let tasks = self.tasks.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
            callback();
            tasks.lock().remove(&id);
        });
        self.tasks.lock().insert(id, task);
        id
    }
}

impl Default for HostTimersService {
    fn default() -> Self {
        Self::new()
    }
}

impl IHostTimers for HostTimersService {
    fn set_clamped_timeout(
        &self,
        callback: Arc<dyn Fn() + Send + Sync>,
        timeout_ms: u64,
    ) -> IHostTimerHandle {
        self.schedule(callback, timeout_ms.min(MAX_TIMER_DELAY_MS))
    }

    fn set_timeout(
        &self,
        callback: Arc<dyn Fn() + Send + Sync>,
        timeout_ms: u64,
    ) -> IHostTimerHandle {
        self.schedule(callback, timeout_ms)
    }

    fn clear_timeout(&self, handle: IHostTimerHandle) {
        if let Some(task) = self.tasks.lock().remove(&handle) {
            task.abort();
        }
    }
}
