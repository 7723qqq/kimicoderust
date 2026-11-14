use std::sync::Arc;

pub type IHostTimerHandle = u64;

pub trait IHostTimers: Send + Sync {
    fn set_clamped_timeout(
        &self,
        callback: Arc<dyn Fn() + Send + Sync>,
        timeout_ms: u64,
    ) -> IHostTimerHandle;
    fn set_timeout(
        &self,
        callback: Arc<dyn Fn() + Send + Sync>,
        timeout_ms: u64,
    ) -> IHostTimerHandle;
    fn clear_timeout(&self, handle: IHostTimerHandle);
}
