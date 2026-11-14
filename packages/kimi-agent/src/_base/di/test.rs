use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};
use once_cell::sync::Lazy;

pub use super::test_instantiation_service::{
    CreateServicesOptions, ServiceGroup, ServiceRegistration, TestInstantiationService,
    create_services,
};
use super::instantiation::{ServiceIdentifier, ServicesAccessor};
use super::scope::{
    IScopeHandle, Scope, ScopeKind, ScopeOptions, ScopeSeed, create_app_scope,
    create_scoped_child_handle,
};
use crate::_base::log::log::{ILogService, ILogger, LogContext, LogLevel, LogPayload};

pub struct SilentTestLogger;

impl ILogger for SilentTestLogger {
    fn error(&self, _message: &str, _payload: Option<LogPayload>) {}
    fn warn(&self, _message: &str, _payload: Option<LogPayload>) {}
    fn info(&self, _message: &str, _payload: Option<LogPayload>) {}
    fn debug(&self, _message: &str, _payload: Option<LogPayload>) {}

    fn child(&self, _ctx: LogContext) -> Arc<dyn ILogger> {
        SILENT_TEST_LOGGER.clone()
    }
}

impl ILogService for SilentTestLogger {
    fn level(&self) -> LogLevel {
        LogLevel::Off
    }

    fn set_level(&self, _level: LogLevel) {}

    fn flush(&self) -> BoxFuture<'static, ()> {
        async {}.boxed()
    }
}

pub static SILENT_TEST_LOGGER: Lazy<Arc<SilentTestLogger>> =
    Lazy::new(|| Arc::new(SilentTestLogger));

pub struct ScopedTestHost {
    pub app: Arc<Scope>,
}

pub enum TestScope {
    Scope(Arc<Scope>),
    Handle(Arc<dyn IScopeHandle>),
}

impl TestScope {
    pub fn accessor(&self) -> Arc<dyn ServicesAccessor> {
        match self {
            Self::Scope(scope) => scope.accessor(),
            Self::Handle(handle) => handle.accessor(),
        }
    }

    pub fn dispose(&self) {
        match self {
            Self::Scope(scope) => scope.dispose(),
            Self::Handle(handle) => {
                let future = handle.dispose();
                tokio::spawn(future);
            }
        }
    }
}

impl ScopedTestHost {
    pub fn child(
        &self,
        kind: ScopeKind,
        id: String,
        stubs: ScopeSeed,
    ) -> Result<TestScope, String> {
        if kind == "program" {
            let handle = create_scoped_child_handle(
                self.app.instantiation.clone(),
                kind,
                id,
                ScopeOptions {
                    seeds: Some(stubs),
                    ..ScopeOptions::default()
                },
            )?;
            return Ok(TestScope::Handle(handle));
        }
        Ok(TestScope::Scope(self.app.create_child(
            kind,
            id,
            ScopeOptions {
                seeds: Some(stubs),
                ..ScopeOptions::default()
            },
        )?))
    }

    pub fn child_of(
        &self,
        parent: &Arc<Scope>,
        kind: ScopeKind,
        id: String,
        stubs: ScopeSeed,
    ) -> Result<Arc<Scope>, String> {
        parent.create_child(
            kind,
            id,
            ScopeOptions {
                seeds: Some(stubs),
                ..ScopeOptions::default()
            },
        )
    }

    pub fn dispose(&self) {
        self.app.dispose();
    }
}

pub fn create_scoped_test_host(app_stubs: ScopeSeed) -> Result<ScopedTestHost, String> {
    Ok(ScopedTestHost {
        app: create_app_scope(ScopeOptions {
            seeds: Some(app_stubs),
            ..ScopeOptions::default()
        })?,
    })
}

pub fn stub_pair<T>(id: ServiceIdentifier<T>, instance: T) -> (ServiceIdentifier<T>, T) {
    (id, instance)
}
