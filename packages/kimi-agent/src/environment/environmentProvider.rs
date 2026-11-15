use std::any::type_name;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
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

pub trait EnvironmentProviderHost: Send + Sync {
    fn get_dynamic(&self, id: &ServiceIdentifier<DynamicValue>) -> DynamicValue;
    fn register_environment(
        &self,
        environment: Arc<dyn Environment>,
    ) -> Result<EnvironmentRegistrationHandle, WireAsyncError>;
}

pub trait EnvironmentProviderHostExt: EnvironmentProviderHost {
    fn get<T: ?Sized + 'static>(&self) -> Arc<T> {
        let value = self.get_dynamic(&provider_service_identifier::<T>());
        Arc::downcast::<Arc<T>>(value)
            .unwrap_or_else(|_| panic!("service type does not match its identifier"))
            .as_ref()
            .clone()
    }
}

impl<H> EnvironmentProviderHostExt for H where H: EnvironmentProviderHost + ?Sized {}

fn provider_service_identifier<T: ?Sized + 'static>() -> ServiceIdentifier<DynamicValue> {
    let short_name = type_name::<T>()
        .rsplit("::")
        .next()
        .expect("service type has a name");
    let name = if short_name == "ITelemetryService" {
        "agentTelemetryService".to_string()
    } else {
        let name = short_name.strip_prefix('I').unwrap_or(short_name);
        let mut chars = name.chars();
        match chars.next() {
            Some(first) => format!("{}{}", first.to_lowercase(), chars.as_str()),
            None => name.to_string(),
        }
    };
    create_decorator(name)
}

#[async_trait]
pub trait EnvironmentProviderFactory: Send + Sync {
    fn id(&self) -> &str;
    async fn attach(
        &self,
        host: &dyn EnvironmentProviderHost,
    ) -> Result<EnvironmentProviderAttachment, WireAsyncError>;
}
