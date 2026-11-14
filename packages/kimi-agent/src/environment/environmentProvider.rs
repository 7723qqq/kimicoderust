use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::environment::environment::Environment;
use crate::environment::environment_registry::EnvironmentRegistrationHandle;
use crate::wire::record::WireAsyncError;

pub struct EnvironmentProviderAttachment {
    pub dispose: Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>,
}

impl EnvironmentProviderAttachment {
    pub fn new(dispose: impl FnOnce() -> BoxFuture<'static, ()> + Send + 'static) -> Self {
        Self {
            dispose: Box::new(dispose),
        }
    }

    pub async fn dispose(self) {
        (self.dispose)().await;
    }
}

pub trait EnvironmentProviderHost {
    fn get<T: ?Sized + 'static>(&self) -> Arc<T>;
    fn register_environment(
        &self,
        environment: Arc<dyn Environment>,
    ) -> Result<EnvironmentRegistrationHandle, WireAsyncError>;
}

#[async_trait]
pub trait EnvironmentProviderFactory: Send + Sync {
    fn id(&self) -> &str;
    async fn attach(
        &self,
        host: &dyn EnvironmentProviderHost,
    ) -> Result<EnvironmentProviderAttachment, WireAsyncError>;
}
