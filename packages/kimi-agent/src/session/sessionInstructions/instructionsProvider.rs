use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::ScopeSeed;
use crate::human::utils::watch::WatchChange;
use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait ISessionInstructionsProvider: Send + Sync {
    async fn ready(&self) -> Result<(), WireAsyncError>;
    fn agents_md(&self) -> Option<String>;
    fn agents_md_warning(&self) -> Option<String>;
    fn agents_md_paths(&self) -> Option<Vec<String>>;
    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(Vec<WatchChange>) + Send + Sync>,
    ) -> DisposableRef;
}

pub fn session_instructions_provider_seed(
    provider: Arc<dyn ISessionInstructionsProvider>,
) -> Vec<ScopeSeed> {
    vec![ScopeSeed::new::<ISessionInstructionsProvider>(provider)]
}
