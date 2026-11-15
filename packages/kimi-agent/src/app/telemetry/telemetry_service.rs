use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::join_all;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::_base::di::instantiation::refine_service_decorator;
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::app::scopes::LifecycleScope;

use super::context::{TelemetryContextPatch, TelemetryProperties};
use super::telemetry::{
    ITelemetryAppender, ITelemetryScopeBindingHost, ITelemetryService, TelemetryAppenderRecord,
    TelemetryScopeBinding, null_telemetry_appender,
};

const WIRE_SESSION_ID_PROPERTY: &str = "sessionId";

fn apply_patch(target: &mut TelemetryProperties, patch: TelemetryContextPatch) {
    for (key, value) in patch {
        if value.is_null() {
            target.remove(&key);
        } else {
            target.insert(key, value);
        }
    }
}

pub fn compose_telemetry_properties(
    ambient: &TelemetryProperties,
    explicit: Option<&TelemetryProperties>,
) -> TelemetryProperties {
    let mut properties = TelemetryProperties::new();
    for (key, value) in ambient {
        if key == "session_id" {
            continue;
        }
        properties.insert(key.clone(), value.clone());
    }
    if let Some(session_id) = ambient.get("session_id") {
        properties.insert(WIRE_SESSION_ID_PROPERTY.to_string(), session_id.clone());
    }
    if let Some(explicit) = explicit {
        for (key, value) in explicit {
            properties.insert(key.clone(), value.clone());
        }
    }
    properties
}

pub trait IntoTelemetryContextPatch {
    fn into_telemetry_context_patch(self) -> TelemetryContextPatch;
}

impl IntoTelemetryContextPatch for TelemetryContextPatch {
    fn into_telemetry_context_patch(self) -> TelemetryContextPatch {
        self
    }
}

impl IntoTelemetryContextPatch for Value {
    fn into_telemetry_context_patch(self) -> TelemetryContextPatch {
        match self {
            Value::Object(properties) => properties,
            _ => panic!("telemetry context patch must be an object"),
        }
    }
}

pub fn bind_telemetry_scope(
    parent: Arc<dyn ITelemetryService>,
    seed: impl IntoTelemetryContextPatch,
) -> TelemetryScopeBinding {
    let seed = seed.into_telemetry_context_patch();
    if let Some(host) = parent.scope_binding_host() {
        return host.create_scope_binding(seed);
    }
    TelemetryScopeBinding::detached(parent.with_context(seed))
}

trait TelemetryAmbientSource: Send + Sync {
    fn ambient(&self) -> TelemetryProperties;
}

struct TelemetryRoot {
    appenders: Mutex<Vec<Arc<dyn ITelemetryAppender>>>,
    context: Mutex<TelemetryProperties>,
    enabled: AtomicBool,
}

#[derive(Clone)]
pub struct TelemetryService {
    root: Arc<TelemetryRoot>,
}

impl TelemetryService {
    pub fn new() -> Self {
        Self {
            root: Arc::new(TelemetryRoot {
                appenders: Mutex::new(vec![null_telemetry_appender()]),
                context: Mutex::new(TelemetryProperties::new()),
                enabled: AtomicBool::new(true),
            }),
        }
    }

    fn dispatch(
        &self,
        event: &str,
        ambient: TelemetryProperties,
        properties: Option<TelemetryProperties>,
    ) {
        if !self.root.enabled.load(Ordering::Acquire) {
            return;
        }
        let record = TelemetryAppenderRecord {
            event: event.to_string(),
            properties: compose_telemetry_properties(&ambient, properties.as_ref()),
            context: ambient,
        };
        let appenders = self.root.appenders.lock().clone();
        for appender in appenders {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                appender.track(record.clone())
            }));
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => on_unexpected_error(error),
                Err(error) => on_unexpected_error(panic_message(error)),
            }
        }
    }
}

impl Default for TelemetryService {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryAmbientSource for TelemetryService {
    fn ambient(&self) -> TelemetryProperties {
        self.root.context.lock().clone()
    }
}

#[async_trait]
impl ITelemetryService for TelemetryService {
    fn track(&self, event: &str, properties: Option<TelemetryProperties>) {
        self.dispatch(event, self.ambient(), properties);
    }

    fn track2(&self, event: &str, properties: Option<TelemetryProperties>) {
        self.dispatch(event, self.ambient(), properties);
    }

    fn with_context(&self, patch: TelemetryContextPatch) -> Arc<dyn ITelemetryService> {
        let mut context = self.ambient();
        apply_patch(&mut context, patch);
        Arc::new(TelemetrySnapshotView::new(self.clone(), context))
    }

    fn set_context(&self, patch: TelemetryContextPatch) {
        apply_patch(&mut self.root.context.lock(), patch);
    }

    fn get_context(&self) -> TelemetryContextPatch {
        self.ambient()
    }

    fn add_appender(&self, appender: Arc<dyn ITelemetryAppender>) -> DisposableRef {
        self.root.appenders.lock().push(appender.clone());
        let this = self.clone();
        to_disposable(move || this.remove_appender(&appender))
    }

    fn remove_appender(&self, appender: &Arc<dyn ITelemetryAppender>) {
        self.root
            .appenders
            .lock()
            .retain(|current| !Arc::ptr_eq(current, appender));
    }

    fn set_enabled(&self, enabled: bool) {
        self.root.enabled.store(enabled, Ordering::Release);
    }

    fn scope_binding_host(&self) -> Option<&dyn ITelemetryScopeBindingHost> {
        Some(self)
    }

    async fn flush(&self) {
        let appenders = self.root.appenders.lock().clone();
        join_all(appenders.into_iter().map(|appender| async move {
            if let Err(error) = appender.flush().await {
                on_unexpected_error(error);
            }
        }))
        .await;
    }

    async fn shutdown(&self) {
        let appenders = self.root.appenders.lock().clone();
        join_all(appenders.into_iter().map(|appender| async move {
            if let Err(error) = appender.shutdown().await {
                on_unexpected_error(error);
            }
        }))
        .await;
    }
}

impl ITelemetryScopeBindingHost for TelemetryService {
    fn create_scope_binding(&self, seed: TelemetryContextPatch) -> TelemetryScopeBinding {
        let bound = BoundTelemetryService::new(self.clone(), Arc::new(self.clone()), seed);
        let telemetry: Arc<dyn ITelemetryService> = Arc::new(bound.clone());
        let disposable = to_disposable(move || bound.dispose());
        TelemetryScopeBinding::new(telemetry, disposable)
    }
}

struct BoundTelemetryInner {
    root: TelemetryService,
    parent: Arc<dyn TelemetryAmbientSource>,
    fragment: Mutex<TelemetryProperties>,
    disposed: AtomicBool,
}

#[derive(Clone)]
struct BoundTelemetryService {
    inner: Arc<BoundTelemetryInner>,
}

impl BoundTelemetryService {
    fn new(
        root: TelemetryService,
        parent: Arc<dyn TelemetryAmbientSource>,
        seed: TelemetryContextPatch,
    ) -> Self {
        let mut fragment = TelemetryProperties::new();
        apply_patch(&mut fragment, seed);
        Self {
            inner: Arc::new(BoundTelemetryInner {
                root,
                parent,
                fragment: Mutex::new(fragment),
                disposed: AtomicBool::new(false),
            }),
        }
    }

    fn dispose(&self) {
        self.inner.disposed.store(true, Ordering::Release);
    }
}

impl IDisposable for BoundTelemetryService {
    fn dispose(&self) {
        BoundTelemetryService::dispose(self);
    }
}

impl TelemetryAmbientSource for BoundTelemetryService {
    fn ambient(&self) -> TelemetryProperties {
        let mut inherited = self.inner.parent.ambient();
        if self.inner.disposed.load(Ordering::Acquire) {
            return inherited;
        }
        for (key, value) in self.inner.fragment.lock().iter() {
            inherited.insert(key.clone(), value.clone());
        }
        inherited
    }
}

#[async_trait]
impl ITelemetryService for BoundTelemetryService {
    fn track(&self, event: &str, properties: Option<TelemetryProperties>) {
        self.inner.root.dispatch(event, self.ambient(), properties);
    }

    fn track2(&self, event: &str, properties: Option<TelemetryProperties>) {
        self.inner.root.dispatch(event, self.ambient(), properties);
    }

    fn with_context(&self, patch: TelemetryContextPatch) -> Arc<dyn ITelemetryService> {
        let mut context = self.ambient();
        apply_patch(&mut context, patch);
        Arc::new(TelemetrySnapshotView::new(
            self.inner.root.clone(),
            context,
        ))
    }

    fn set_context(&self, patch: TelemetryContextPatch) {
        if !self.inner.disposed.load(Ordering::Acquire) {
            apply_patch(&mut self.inner.fragment.lock(), patch);
        }
    }

    fn get_context(&self) -> TelemetryContextPatch {
        self.ambient()
    }

    fn add_appender(&self, appender: Arc<dyn ITelemetryAppender>) -> DisposableRef {
        self.inner.root.add_appender(appender)
    }

    fn remove_appender(&self, appender: &Arc<dyn ITelemetryAppender>) {
        self.inner.root.remove_appender(appender);
    }

    fn set_enabled(&self, enabled: bool) {
        self.inner.root.set_enabled(enabled);
    }

    fn scope_binding_host(&self) -> Option<&dyn ITelemetryScopeBindingHost> {
        Some(self)
    }

    async fn flush(&self) {
        self.inner.root.flush().await;
    }

    async fn shutdown(&self) {
        self.inner.root.shutdown().await;
    }
}

impl ITelemetryScopeBindingHost for BoundTelemetryService {
    fn create_scope_binding(&self, seed: TelemetryContextPatch) -> TelemetryScopeBinding {
        let bound = BoundTelemetryService::new(
            self.inner.root.clone(),
            Arc::new(self.clone()),
            seed,
        );
        let telemetry: Arc<dyn ITelemetryService> = Arc::new(bound.clone());
        let disposable = to_disposable(move || bound.dispose());
        TelemetryScopeBinding::new(telemetry, disposable)
    }
}

struct TelemetrySnapshotView {
    root: TelemetryService,
    context: Mutex<TelemetryProperties>,
}

impl TelemetrySnapshotView {
    fn new(root: TelemetryService, context: TelemetryProperties) -> Self {
        Self {
            root,
            context: Mutex::new(context),
        }
    }
}

#[async_trait]
impl ITelemetryService for TelemetrySnapshotView {
    fn track(&self, event: &str, properties: Option<TelemetryProperties>) {
        self.root
            .dispatch(event, self.context.lock().clone(), properties);
    }

    fn track2(&self, event: &str, properties: Option<TelemetryProperties>) {
        self.root
            .dispatch(event, self.context.lock().clone(), properties);
    }

    fn with_context(&self, patch: TelemetryContextPatch) -> Arc<dyn ITelemetryService> {
        let mut context = self.context.lock().clone();
        apply_patch(&mut context, patch);
        Arc::new(TelemetrySnapshotView::new(self.root.clone(), context))
    }

    fn set_context(&self, patch: TelemetryContextPatch) {
        apply_patch(&mut self.context.lock(), patch);
    }

    fn get_context(&self) -> TelemetryContextPatch {
        self.context.lock().clone()
    }

    fn add_appender(&self, appender: Arc<dyn ITelemetryAppender>) -> DisposableRef {
        self.root.add_appender(appender)
    }

    fn remove_appender(&self, appender: &Arc<dyn ITelemetryAppender>) {
        self.root.remove_appender(appender);
    }

    fn set_enabled(&self, enabled: bool) {
        self.root.set_enabled(enabled);
    }

    async fn flush(&self) {
        self.root.flush().await;
    }

    async fn shutdown(&self) {
        self.root.shutdown().await;
    }
}

fn panic_message(error: Box<dyn std::any::Any + Send>) -> String {
    error
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| error.downcast_ref::<&str>().map(|value| (*value).to_string()))
        .unwrap_or_else(|| "unknown appender panic".to_string())
}

static TELEMETRY_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|_| {
        let service: Arc<dyn ITelemetryService> = Arc::new(TelemetryService::new());
        Arc::new(service) as DynamicValue
    });
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        refine_service_decorator(&super::telemetry::I_TELEMETRY_SERVICE),
        SyncDescriptor::new(ctor, Vec::new()),
        ScopeActivation::OnScopeCreated,
        "telemetry",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_telemetry_service_registered() -> Result<(), String> {
    Lazy::force(&TELEMETRY_SERVICE_REGISTRATION).clone()
}
