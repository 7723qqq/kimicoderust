use crate::rpc::types::BoxFuture;

pub use super::mode_mutex_service::ExclusiveReviewMode;

pub trait AgentModeMutexService: Send + Sync {
    fn active_mode(&self) -> Option<ExclusiveReviewMode>;

    fn switch_to(&self, target: ExclusiveReviewMode) -> BoxFuture<'static, Result<(), String>>;

    fn leave(&self, target: ExclusiveReviewMode) -> BoxFuture<'static, Result<(), String>>;
}
