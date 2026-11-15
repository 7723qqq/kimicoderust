use std::sync::Arc;

use async_trait::async_trait;
use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::{DISPOSABLE_NONE, DisposableRef, IDisposable};
use crate::wire::record::WireAsyncError;

use super::context::{TelemetryContextPatch, TelemetryProperties};

#[derive(Debug, Clone)]
pub struct TelemetryAppenderRecord {
    pub event: String,
    pub context: TelemetryProperties,
    pub properties: TelemetryProperties,
}

#[async_trait]
pub trait ITelemetryAppender: Send + Sync {
    fn track(&self, record: TelemetryAppenderRecord) -> Result<(), WireAsyncError>;

    async fn flush(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct TelemetryScopeBinding {
    pub telemetry: Arc<dyn ITelemetryService>,
    disposable: DisposableRef,
}

impl TelemetryScopeBinding {
    pub fn new(telemetry: Arc<dyn ITelemetryService>, disposable: DisposableRef) -> Self {
        Self {
            telemetry,
            disposable,
        }
    }

    pub fn detached(telemetry: Arc<dyn ITelemetryService>) -> Self {
        Self::new(telemetry, DISPOSABLE_NONE.clone())
    }

    pub fn dispose(&self) {
        self.disposable.dispose();
    }
}

impl IDisposable for TelemetryScopeBinding {
    fn dispose(&self) {
        TelemetryScopeBinding::dispose(self);
    }
}

pub trait ITelemetryScopeBindingHost: Send + Sync {
    fn create_scope_binding(&self, seed: TelemetryContextPatch) -> TelemetryScopeBinding;
}

#[async_trait]
pub trait ITelemetryService: Send + Sync {
    fn track(&self, event: &str, properties: Option<TelemetryProperties>);
    fn track2(&self, event: &str, properties: Option<TelemetryProperties>);
    fn with_context(&self, patch: TelemetryContextPatch) -> Arc<dyn ITelemetryService>;
    fn set_context(&self, patch: TelemetryContextPatch);
    fn get_context(&self) -> TelemetryContextPatch;
    fn add_appender(&self, appender: Arc<dyn ITelemetryAppender>) -> DisposableRef;
    fn remove_appender(&self, appender: &Arc<dyn ITelemetryAppender>);
    fn set_enabled(&self, enabled: bool);
    fn scope_binding_host(&self) -> Option<&dyn ITelemetryScopeBindingHost> {
        None
    }
    async fn flush(&self);
    async fn shutdown(&self);
}

pub struct NullTelemetryAppender;

impl ITelemetryAppender for NullTelemetryAppender {
    fn track(&self, _record: TelemetryAppenderRecord) -> Result<(), WireAsyncError> {
        Ok(())
    }
}

pub static NULL_TELEMETRY_APPENDER: Lazy<Arc<dyn ITelemetryAppender>> =
    Lazy::new(|| Arc::new(NullTelemetryAppender));

pub fn null_telemetry_appender() -> Arc<dyn ITelemetryAppender> {
    NULL_TELEMETRY_APPENDER.clone()
}

pub struct NoopTelemetryService;

#[async_trait]
impl ITelemetryService for NoopTelemetryService {
    fn track(&self, _event: &str, _properties: Option<TelemetryProperties>) {}

    fn track2(&self, _event: &str, _properties: Option<TelemetryProperties>) {}

    fn with_context(&self, _patch: TelemetryContextPatch) -> Arc<dyn ITelemetryService> {
        noop_telemetry_service()
    }

    fn set_context(&self, _patch: TelemetryContextPatch) {}

    fn get_context(&self) -> TelemetryContextPatch {
        TelemetryContextPatch::new()
    }

    fn add_appender(&self, _appender: Arc<dyn ITelemetryAppender>) -> DisposableRef {
        DISPOSABLE_NONE.clone()
    }

    fn remove_appender(&self, _appender: &Arc<dyn ITelemetryAppender>) {}

    fn set_enabled(&self, _enabled: bool) {}

    async fn flush(&self) {}

    async fn shutdown(&self) {}
}

pub static NOOP_TELEMETRY_SERVICE: Lazy<Arc<dyn ITelemetryService>> =
    Lazy::new(|| Arc::new(NoopTelemetryService));

pub fn noop_telemetry_service() -> Arc<dyn ITelemetryService> {
    NOOP_TELEMETRY_SERVICE.clone()
}

pub static I_TELEMETRY_SERVICE: Lazy<ServiceIdentifier<Arc<dyn ITelemetryService>>> =
    Lazy::new(|| create_decorator("agentTelemetryService"));
