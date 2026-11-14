use std::sync::Arc;

use async_trait::async_trait;
use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::IDisposable;
use crate::_base::event::{IWaitUntil, WaitUntil, WaitUntilFuture};
use crate::_base::utils::abort::AbortSignal;
use crate::remote::declaration::remote_environment_declaration::{
    EnvironmentDeclarationSet, RemoteEnvironmentDeclaration,
};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct EnvironmentDeclarationChange {
    pub entries: Vec<RemoteEnvironmentDeclaration>,
    wait_until: WaitUntil,
}

impl EnvironmentDeclarationChange {
    pub fn wait_until(&self, future: WaitUntilFuture) -> Result<(), String> {
        (self.wait_until)(future)
    }
}

impl IWaitUntil for EnvironmentDeclarationChange {
    type Data = Vec<RemoteEnvironmentDeclaration>;

    fn with_wait_until(data: Self::Data, _signal: AbortSignal, wait_until: WaitUntil) -> Self {
        Self {
            entries: data,
            wait_until,
        }
    }
}

pub type EnvironmentDeclarationChangeListener =
    Arc<dyn Fn(EnvironmentDeclarationChange) + Send + Sync>;

#[async_trait]
pub trait IEnvironmentDeclarationService: Send + Sync {
    fn on_did_change(
        &self,
        listener: EnvironmentDeclarationChangeListener,
    ) -> Box<dyn IDisposable>;
    async fn declarations(&self) -> Result<EnvironmentDeclarationSet, WireAsyncError>;
    fn registration_error(&self, environment_id: &str) -> Option<WireAsyncError>;
    fn report_registration(&self, environment_id: &str, error: Option<String>);
}

pub static I_ENVIRONMENT_DECLARATION_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("environmentDeclarationService"));
