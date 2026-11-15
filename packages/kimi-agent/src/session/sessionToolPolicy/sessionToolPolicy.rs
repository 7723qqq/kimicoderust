use std::sync::LazyLock;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::wire::record::WireAsyncError;

pub type SessionToolPolicyWaitUntil =
    Arc<dyn Fn(BoxFuture<'static, Result<(), WireAsyncError>>) + Send + Sync>;

#[derive(Clone)]
pub struct SessionToolPolicyChangedEvent {
    pub wait_until: SessionToolPolicyWaitUntil,
}

#[async_trait]
pub trait ISessionToolPolicy: Send + Sync {
    async fn ready(&self) -> Result<(), WireAsyncError>;
    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(SessionToolPolicyChangedEvent) + Send + Sync>,
    ) -> DisposableRef;
    fn disabled_tools(&self) -> Vec<String>;
    async fn set_disabled_tools(&self, names: Vec<String>) -> Result<(), WireAsyncError>;
}


pub static I_SESSION_TOOL_POLICY: LazyLock<ServiceIdentifier<Arc<dyn ISessionToolPolicy>>> =
    LazyLock::new(|| create_decorator("sessionToolPolicy"));
