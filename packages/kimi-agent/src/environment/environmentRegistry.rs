use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures_util::{FutureExt, future::BoxFuture};
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::environment::environment::{
    Environment, EnvironmentBinding, EnvironmentCapability, EnvironmentLease,
    EnvironmentResource, EnvironmentStatus, StatusListener,
};
use crate::os::interface::host_timers::IHostTimers;
use crate::wire::record::WireAsyncError;

pub type EnvironmentErrorCode = &'static str;

#[derive(Debug)]
pub struct EnvironmentError {
    pub code: EnvironmentErrorCode,
    message: String,
    cause: Option<WireAsyncError>,
}

impl EnvironmentError {
    pub fn new(
        code: EnvironmentErrorCode,
        message: impl Into<String>,
        cause: Option<WireAsyncError>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            cause,
        }
    }
}

impl fmt::Display for EnvironmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EnvironmentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.as_deref().map(|cause| cause as _)
    }
}

struct TrackedResource {
    session_id: Option<String>,
    disposed: AtomicBool,
    dispose: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
}

struct Entry {
    environment: Arc<dyn Environment>,
    resources: Mutex<IndexMap<u64, Arc<TrackedResource>>>,
    active_leases: Mutex<usize>,
    drain_notify: tokio::sync::Notify,
    closed: AtomicBool,
    status_subscription: Mutex<Option<DisposableRef>>,
}

#[derive(Clone)]
pub struct EnvironmentRegistryChange {
    pub environment_id: String,
    pub current: Option<Arc<dyn Environment>>,
    pub status: Option<EnvironmentStatus>,
}

#[derive(Clone)]
pub struct EnvironmentGenerationSnapshot {
    pub environment_id: String,
    pub generation: String,
    pub status: EnvironmentStatus,
    pub capabilities: Vec<EnvironmentCapability>,
    pub connect_error: Option<String>,
}

#[derive(Clone, Default)]
pub struct EnvironmentRegistrySnapshot {
    pub environments: Vec<EnvironmentGenerationSnapshot>,
}

struct EnvironmentRegistryInner {
    entries: Mutex<IndexMap<String, Arc<Entry>>>,
    listeners: Mutex<HashMap<u64, Arc<dyn Fn(EnvironmentRegistryChange) + Send + Sync>>>,
    next_listener_id: AtomicU64,
    next_resource_id: AtomicU64,
    disposing: AtomicBool,
    drain_timeout_ms: u64,
    timers: Option<Arc<dyn IHostTimers>>,
}

#[derive(Clone)]
pub struct EnvironmentRegistry {
    inner: Arc<EnvironmentRegistryInner>,
}

pub struct EnvironmentRegistryOptions {
    pub drain_timeout_ms: Option<u64>,
    pub timers: Option<Arc<dyn IHostTimers>>,
}

impl Default for EnvironmentRegistryOptions {
    fn default() -> Self {
        Self {
            drain_timeout_ms: None,
            timers: None,
        }
    }
}

const DEFAULT_ENVIRONMENT_DRAIN_TIMEOUT_MS: u64 = 5000;

impl EnvironmentRegistry {
    pub fn new(options: EnvironmentRegistryOptions) -> Self {
        Self {
            inner: Arc::new(EnvironmentRegistryInner {
                entries: Mutex::new(IndexMap::new()),
                listeners: Mutex::new(HashMap::new()),
                next_listener_id: AtomicU64::new(0),
                next_resource_id: AtomicU64::new(0),
                disposing: AtomicBool::new(false),
                drain_timeout_ms: options
                    .drain_timeout_ms
                    .unwrap_or(DEFAULT_ENVIRONMENT_DRAIN_TIMEOUT_MS),
                timers: options.timers,
            }),
        }
    }

    pub fn on_did_change(
        &self,
        listener: Arc<dyn Fn(EnvironmentRegistryChange) + Send + Sync>,
    ) -> DisposableRef {
        let id = self.inner.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.inner.listeners.lock().insert(id, listener);
        let inner = Arc::downgrade(&self.inner);
        to_disposable(move || {
            if let Some(inner) = inner.upgrade() {
                inner.listeners.lock().remove(&id);
            }
        })
    }

    fn list(&self) -> Vec<Arc<dyn Environment>> {
        self.inner
            .entries
            .lock()
            .values()
            .map(|entry| entry.environment.clone())
            .collect()
    }

    pub fn snapshot(&self) -> EnvironmentRegistrySnapshot {
        EnvironmentRegistrySnapshot {
            environments: self
                .list()
                .into_iter()
                .map(|environment| {
                    let identity = environment.identity();
                    EnvironmentGenerationSnapshot {
                        environment_id: identity.environment_id,
                        generation: identity.generation,
                        status: environment.status(),
                        capabilities: environment.capabilities().into_iter().collect(),
                        connect_error: environment.connect_error(),
                    }
                })
                .collect(),
        }
    }

    pub fn current(&self, environment_id: &str) -> Option<Arc<dyn Environment>> {
        self.inner
            .entries
            .lock()
            .get(environment_id)
            .map(|entry| entry.environment.clone())
    }

    pub fn inspect(&self, binding: &EnvironmentBinding) -> Result<Arc<dyn Environment>, WireAsyncError> {
        self.current(&binding.environment_id).ok_or_else(|| {
            Box::new(EnvironmentError::new(
                "environment.not_found",
                format!("environment {} does not exist", binding.environment_id),
                None,
            )) as WireAsyncError
        })
    }

    pub fn register(
        &self,
        environment: Arc<dyn Environment>,
    ) -> Result<EnvironmentRegistrationHandle, WireAsyncError> {
        if self.inner.disposing.load(Ordering::Acquire) {
            return Err(Box::new(EnvironmentError::new(
                "environment.unavailable",
                "environment registry is disposing",
                None,
            )));
        }
        self.assert_ready(environment.as_ref())?;
        let identity = environment.identity();
        if self.inner.entries.lock().contains_key(&identity.environment_id) {
            return Err(Box::new(EnvironmentError::new(
                "environment.conflict",
                format!("environment {} already exists", identity.environment_id),
                None,
            )));
        }
        let entry = Arc::new(Entry {
            environment: environment.clone(),
            resources: Mutex::new(IndexMap::new()),
            active_leases: Mutex::new(0),
            drain_notify: tokio::sync::Notify::new(),
            closed: AtomicBool::new(false),
            status_subscription: Mutex::new(None),
        });
        let weak_entry = Arc::downgrade(&entry);
        let weak_inner = Arc::downgrade(&self.inner);
        let environment_id = identity.environment_id.clone();
        let subscription = environment.on_did_change_status(Arc::new(move |status| {
            let Some(entry) = weak_entry.upgrade() else {
                return;
            };
            let Some(inner) = weak_inner.upgrade() else {
                return;
            };
            if entry.closed.load(Ordering::Acquire) {
                return;
            }
            let current = inner.entries.lock().get(&environment_id).cloned();
            if current.is_none_or(|current| !Arc::ptr_eq(&current, &entry)) {
                return;
            }
            fire_change(
                &inner,
                EnvironmentRegistryChange {
                    environment_id: environment_id.clone(),
                    current: Some(entry.environment.clone()),
                    status: Some(status),
                },
            );
        }));
        *entry.status_subscription.lock() = Some(subscription);
        self.inner
            .entries
            .lock()
            .insert(identity.environment_id.clone(), entry);
        self.publish(environment.clone());
        Ok(EnvironmentRegistrationHandle {
            environment_id: identity.environment_id,
            registry: self.inner.clone(),
        })
    }

    pub async fn remove(&self, environment_id: &str) -> Result<(), WireAsyncError> {
        let previous = self.inner.entries.lock().shift_remove(environment_id);
        let Some(previous) = previous else {
            return Ok(());
        };
        fire_change(
            &self.inner,
            EnvironmentRegistryChange {
                environment_id: environment_id.to_string(),
                current: None,
                status: None,
            },
        );
        self.retire(previous, true).await;
        Ok(())
    }

    pub async fn acquire_when_ready(
        &self,
        binding: &EnvironmentBinding,
        required: &[EnvironmentCapability],
    ) -> Result<EnvironmentLease, WireAsyncError> {
        let entry = self.inner.entries.lock().get(&binding.environment_id).cloned();
        if let Some(entry) = entry
            && !entry.closed.load(Ordering::Acquire)
            && !environment_is_ready(entry.environment.as_ref())
        {
            entry.environment.connect().await?;
            if let Some(when_ready) = entry.environment.when_ready() {
                when_ready.await?;
            }
        }
        self.acquire(binding, required)
    }

    pub fn acquire(
        &self,
        binding: &EnvironmentBinding,
        required: &[EnvironmentCapability],
    ) -> Result<EnvironmentLease, WireAsyncError> {
        let entry = self.inner.entries.lock().get(&binding.environment_id).cloned();
        let Some(entry) = entry else {
            return Err(Box::new(EnvironmentError::new(
                "environment.not_found",
                format!("environment {} does not exist", binding.environment_id),
                None,
            )));
        };
        if entry.closed.load(Ordering::Acquire)
            || !environment_is_ready(entry.environment.as_ref())
        {
            let reason = entry
                .environment
                .connect_error()
                .and_then(|error| error.split('\n').next().map(str::to_string))
                .filter(|reason| !reason.is_empty())
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default();
            let status = if entry.closed.load(Ordering::Acquire) {
                "unavailable".to_string()
            } else {
                entry.environment.status().to_string()
            };
            return Err(Box::new(EnvironmentError::new(
                "environment.unavailable",
                format!("environment {} is {status}{reason}", binding.environment_id),
                None,
            )));
        }
        let capabilities = entry.environment.capabilities();
        for capability in required {
            if !capabilities.contains(capability) {
                return Err(Box::new(EnvironmentError::new(
                    "environment.capability_unavailable",
                    format!(
                        "environment {} does not provide {capability}",
                        binding.environment_id
                    ),
                    None,
                )));
            }
        }
        *entry.active_leases.lock() += 1;
        let active = Arc::new(AtomicBool::new(true));
        let track_active = active.clone();
        let track_entry = Arc::downgrade(&entry);
        let inner = Arc::downgrade(&self.inner);
        let environment_id = binding.environment_id.clone();
        let track_resource = Arc::new(
            move |resource: Arc<dyn EnvironmentResource>, session_id: Option<String>| {
                let Some(entry) = track_entry.upgrade() else {
                    return Err(Box::new(EnvironmentError::new(
                        "environment.unavailable",
                        format!("environment {environment_id} is unavailable"),
                        None,
                    )) as WireAsyncError);
                };
                if !track_active.load(Ordering::Acquire)
                    || entry.closed.load(Ordering::Acquire)
                {
                    return Err(Box::new(EnvironmentError::new(
                        "environment.unavailable",
                        format!("environment {environment_id} is unavailable"),
                        None,
                    )) as WireAsyncError);
                }
                let Some(inner) = inner.upgrade() else {
                    return Err(Box::new(EnvironmentError::new(
                        "environment.unavailable",
                        format!("environment {environment_id} is unavailable"),
                        None,
                    )) as WireAsyncError);
                };
                let id = inner.next_resource_id.fetch_add(1, Ordering::Relaxed);
                entry.resources.lock().insert(
                    id,
                    Arc::new(TrackedResource {
                        session_id,
                        disposed: AtomicBool::new(false),
                        dispose: Arc::new(move || resource.dispose()),
                    }),
                );
                Ok(())
            },
        );
        let dispose_entry = Arc::downgrade(&entry);
        let lease = EnvironmentLease::new(
            entry.environment.clone(),
            track_resource,
            move || {
                if !active.swap(false, Ordering::AcqRel) {
                    return;
                }
                if let Some(entry) = dispose_entry.upgrade() {
                    let mut active = entry.active_leases.lock();
                    *active = active.saturating_sub(1);
                    if *active == 0 {
                        entry.drain_notify.notify_waiters();
                    }
                }
            },
        );
        Ok(lease)
    }

    pub async fn dispose(&self) {
        if self.inner.disposing.swap(true, Ordering::AcqRel) {
            return;
        }
        let entries = std::mem::take(&mut *self.inner.entries.lock())
            .into_iter()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        for entry in entries.into_iter().rev() {
            self.retire(entry, false).await;
        }
        self.inner.listeners.lock().clear();
    }

    pub async fn drain_session(&self, session_id: &str) {
        let mut records = Vec::new();
        for entry in self.inner.entries.lock().values() {
            for (id, record) in entry.resources.lock().iter() {
                if record.session_id.as_deref() == Some(session_id) {
                    records.push((*id, entry.clone(), record.clone()));
                }
            }
        }
        records.reverse();
        for (id, entry, record) in records {
            dispose_record(entry, id, record).await;
        }
    }

    fn publish(&self, environment: Arc<dyn Environment>) {
        let identity = environment.identity();
        fire_change(
            &self.inner,
            EnvironmentRegistryChange {
                environment_id: identity.environment_id,
                status: Some(environment.status()),
                current: Some(environment),
            },
        );
    }

    fn assert_ready(&self, environment: &dyn Environment) -> Result<(), WireAsyncError> {
        if environment.status() == EnvironmentStatus::Disposed {
            return Err(Box::new(EnvironmentError::new(
                "environment.unavailable",
                format!(
                    "environment {} is {}",
                    environment.identity().environment_id,
                    environment.status()
                ),
                None,
            )));
        }
        Ok(())
    }

    async fn retire(&self, entry: Arc<Entry>, drain: bool) {
        if entry.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(subscription) = entry.status_subscription.lock().take() {
            subscription.dispose();
        }
        if drain {
            self.wait_for_drain(&entry).await;
        }
        let records = std::mem::take(&mut *entry.resources.lock())
            .into_iter()
            .map(|(id, record)| (id, record))
            .collect::<Vec<_>>();
        for (id, record) in records.into_iter().rev() {
            dispose_record(entry.clone(), id, record).await;
        }
        entry.environment.dispose().await;
    }

    async fn wait_for_drain(&self, entry: &Arc<Entry>) {
        if *entry.active_leases.lock() == 0 {
            return;
        }
        if let Some(timers) = &self.inner.timers {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let sender = Arc::new(Mutex::new(Some(sender)));
            let handle = timers.set_timeout(
                Arc::new(move || {
                    if let Some(sender) = sender.lock().take() {
                        let _ = sender.send(());
                    }
                }),
                self.inner.drain_timeout_ms,
            );
            tokio::select! {
                _ = entry.drain_notify.notified() => {}
                _ = receiver => {}
            }
            timers.clear_timeout(handle);
        } else {
            tokio::select! {
                _ = entry.drain_notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(self.inner.drain_timeout_ms)) => {}
            }
        }
    }
}

pub struct EnvironmentRegistrationHandle {
    pub environment_id: String,
    registry: Arc<EnvironmentRegistryInner>,
}

impl EnvironmentRegistrationHandle {
    pub async fn remove(&self) -> Result<(), WireAsyncError> {
        EnvironmentRegistry {
            inner: self.registry.clone(),
        }
        .remove(&self.environment_id)
        .await
    }
}

async fn dispose_record(entry: Arc<Entry>, id: u64, record: Arc<TrackedResource>) {
    if record.disposed.swap(true, Ordering::AcqRel) {
        return;
    }
    entry.resources.lock().shift_remove(&id);
    (record.dispose)().await;
}

fn fire_change(inner: &EnvironmentRegistryInner, change: EnvironmentRegistryChange) {
    for listener in inner.listeners.lock().values() {
        listener(change.clone());
    }
}

pub fn environment_is_ready(environment: &dyn Environment) -> bool {
    environment.status() == EnvironmentStatus::Ready
}
