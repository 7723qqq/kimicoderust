use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::environment::environment::{Environment, EnvironmentBinding, EnvironmentCapability, EnvironmentLease};
use crate::environment::environment_provider::EnvironmentProviderFactory;
use crate::environment::environment_registry::{
    EnvironmentRegistrationHandle, EnvironmentRegistryChange, EnvironmentRegistrySnapshot,
};
use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait EnvironmentResolver: Send + Sync {
    fn inspect(&self, binding: &EnvironmentBinding) -> Result<Arc<dyn Environment>, WireAsyncError>;

    fn acquire(
        &self,
        binding: &EnvironmentBinding,
        required: &[EnvironmentCapability],
    ) -> Result<EnvironmentLease, WireAsyncError>;

    async fn acquire_when_ready(
        &self,
        binding: &EnvironmentBinding,
        required: &[EnvironmentCapability],
    ) -> Result<EnvironmentLease, WireAsyncError>;
}

#[async_trait]
pub trait IEnvironmentProviderHandle: Send + Sync {
    async fn dispose(&self) -> Result<(), WireAsyncError>;
}

#[async_trait]
pub trait IEnvironmentService: EnvironmentResolver {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;

    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(EnvironmentRegistryChange) + Send + Sync>,
    ) -> DisposableRef;

    fn snapshot(&self) -> EnvironmentRegistrySnapshot;

    fn current(&self, environment_id: &str) -> Option<Arc<dyn Environment>>;

    fn register(
        &self,
        environment: Arc<dyn Environment>,
    ) -> Result<EnvironmentRegistrationHandle, WireAsyncError>;

    async fn drain_session(&self, session_id: &str);

    async fn add_provider(
        &self,
        factory: Arc<dyn EnvironmentProviderFactory>,
    ) -> Result<Arc<dyn IEnvironmentProviderHandle>, WireAsyncError>;

    async fn dispose(&self) -> Result<(), WireAsyncError>;
}

pub static I_ENVIRONMENT_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("environmentService"));
