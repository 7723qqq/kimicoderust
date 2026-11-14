use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt, Shared, join_all};
use parking_lot::Mutex;
use tokio::sync::Notify;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyedGeneration {
    String(String),
    Number(u64),
}

impl fmt::Display for KeyedGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(value) => formatter.write_str(value),
            Self::Number(value) => fmt::Display::fmt(value, formatter),
        }
    }
}

#[derive(Clone, Debug)]
pub struct KeyedResourceGeneration {
    pub owner: String,
    pub generation: KeyedGeneration,
}

pub trait KeyedResource: Send + Sync + 'static {
    fn dispose(&self) -> BoxFuture<'static, ()>;

    fn abort(&self, _reason: Option<String>) {}
}

pub struct KeyedResourceLease<Resource>
where
    Resource: KeyedResource,
{
    pub resource: Arc<Resource>,
    entry: Arc<ResourceEntry<Resource>>,
    active: AtomicBool,
}

impl<Resource> KeyedResourceLease<Resource>
where
    Resource: KeyedResource,
{
    pub fn release(&self) {
        if !self.active.swap(false, Ordering::SeqCst) {
            return;
        }
        if self.entry.leases.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.entry.zero_leases.notify_waiters();
        }
    }
}

type ResourceFuture<Resource> = Shared<BoxFuture<'static, Result<Arc<Resource>, String>>>;
type DrainFuture = Shared<BoxFuture<'static, ()>>;

fn spawn_shared<F, T>(future: F) -> Shared<BoxFuture<'static, T>>
where
    F: Future<Output = T> + Send + 'static,
    T: Clone + Send + Sync + 'static,
{
    let task = tokio::spawn(future);
    async move {
        match task.await {
            Ok(value) => value,
            Err(_) => std::future::pending::<T>().await,
        }
    }
    .boxed()
    .shared()
}

struct ResourceEntry<Resource>
where
    Resource: KeyedResource,
{
    id: u64,
    promise: Mutex<Option<ResourceFuture<Resource>>>,
    resource: Mutex<Option<Arc<Resource>>>,
    leases: AtomicUsize,
    draining: AtomicBool,
    abort_on_drain: AtomicBool,
    aborted: AtomicBool,
    disposed: AtomicBool,
    drain_promise: Mutex<Option<DrainFuture>>,
    zero_leases: Notify,
}

static NEXT_ENTRY_ID: AtomicU64 = AtomicU64::new(1);

pub type ResourceFactory<Key, Resource> = Arc<
    dyn Fn(Key) -> BoxFuture<'static, Result<Arc<Resource>, String>> + Send + Sync,
>;

pub struct KeyedResourceLeasePool<Key, Resource>
where
    Key: Clone + Eq + Hash + Send + Sync + 'static,
    Resource: KeyedResource,
{
    pub identity: KeyedResourceGeneration,
    create: ResourceFactory<Key, Resource>,
    entries: Arc<Mutex<HashMap<Key, Arc<ResourceEntry<Resource>>>>>,
    withdrawn: AtomicBool,
    withdrawal: Mutex<Option<DrainFuture>>,
}

impl<Key, Resource> KeyedResourceLeasePool<Key, Resource>
where
    Key: Clone + Eq + Hash + Send + Sync + 'static,
    Resource: KeyedResource,
{
    pub fn new(identity: KeyedResourceGeneration, create: ResourceFactory<Key, Resource>) -> Arc<Self> {
        Arc::new(Self {
            identity,
            create,
            entries: Arc::new(Mutex::new(HashMap::new())),
            withdrawn: AtomicBool::new(false),
            withdrawal: Mutex::new(None),
        })
    }

    pub fn acquire(
        self: &Arc<Self>,
        key: Key,
    ) -> BoxFuture<'static, Result<KeyedResourceLease<Resource>, String>> {
        if self.withdrawn.load(Ordering::SeqCst) {
            let error = self.unavailable();
            return async move { Err(error) }.boxed();
        }
        let entry = {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.get(&key).cloned() {
                entry
            } else {
                let entry = self.create_entry(key.clone());
                entries.insert(key, entry.clone());
                entry
            }
        };
        if entry.draining.load(Ordering::SeqCst) {
            let error = self.unavailable();
            return async move { Err(error) }.boxed();
        }
        entry.leases.fetch_add(1, Ordering::SeqCst);
        let future = entry.promise.lock().clone().expect("resource promise is not initialized");
        async move {
            match future.await {
                Ok(resource) => Ok(KeyedResourceLease {
                    resource,
                    entry: entry.clone(),
                    active: AtomicBool::new(true),
                }),
                Err(error) => {
                    if entry.leases.fetch_sub(1, Ordering::SeqCst) == 1 {
                        entry.zero_leases.notify_waiters();
                    }
                    Err(error)
                }
            }
        }
        .boxed()
    }

    pub fn has(&self, key: &Key) -> bool {
        self.entries.lock().contains_key(key)
    }

    pub fn dispose_key(
        self: &Arc<Self>,
        key: &Key,
        reason: Option<String>,
        abort: bool,
    ) -> BoxFuture<'static, ()> {
        let Some(entry) = self.entries.lock().remove(key) else {
            return async {}.boxed();
        };
        self.drain(entry, reason, abort)
    }

    pub fn withdraw(self: &Arc<Self>, reason: Option<String>) -> BoxFuture<'static, ()> {
        let mut withdrawal_slot = self.withdrawal.lock();
        if let Some(withdrawal) = withdrawal_slot.as_ref() {
            return Box::pin(withdrawal.clone());
        }
        self.withdrawn.store(true, Ordering::SeqCst);
        let entries = self
            .entries
            .lock()
            .drain()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        let drains = entries
            .into_iter()
            .map(|entry| self.drain(entry, reason.clone(), false))
            .collect::<Vec<_>>();
        let withdrawal = async move {
            join_all(drains).await;
        }
        .boxed()
        .shared();
        *withdrawal_slot = Some(withdrawal.clone());
        Box::pin(withdrawal)
    }

    fn create_entry(self: &Arc<Self>, key: Key) -> Arc<ResourceEntry<Resource>> {
        let entry = Arc::new(ResourceEntry {
            id: NEXT_ENTRY_ID.fetch_add(1, Ordering::Relaxed),
            promise: Mutex::new(None),
            resource: Mutex::new(None),
            leases: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            abort_on_drain: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            drain_promise: Mutex::new(None),
            zero_leases: Notify::new(),
        });
        let create = self.create.clone();
        let weak_entry = Arc::downgrade(&entry);
        let weak_pool = Arc::downgrade(self);
        let promise = spawn_shared(async move {
            let result = create(key.clone()).await;
            match &result {
                Ok(resource) => {
                    if let Some(entry) = weak_entry.upgrade() {
                        *entry.resource.lock() = Some(resource.clone());
                        if entry.abort_on_drain.load(Ordering::SeqCst)
                            && let Some(pool) = weak_pool.upgrade()
                        {
                            pool.abort(&entry, None);
                        }
                    }
                }
                Err(_) => {
                    if let (Some(pool), Some(entry)) = (weak_pool.upgrade(), weak_entry.upgrade()) {
                        let mut entries = pool.entries.lock();
                        if entries.get(&key).is_some_and(|current| current.id == entry.id) {
                            entries.remove(&key);
                        }
                    }
                }
            }
            result
        });
        *entry.promise.lock() = Some(promise);
        entry
    }

    fn drain(
        self: &Arc<Self>,
        entry: Arc<ResourceEntry<Resource>>,
        reason: Option<String>,
        abort: bool,
    ) -> BoxFuture<'static, ()> {
        if abort {
            entry.abort_on_drain.store(true, Ordering::SeqCst);
        }
        let mut drain_slot = entry.drain_promise.lock();
        if let Some(drain) = drain_slot.as_ref() {
            return Box::pin(drain.clone());
        }
        entry.draining.store(true, Ordering::SeqCst);
        let pool = self.clone();
        let drain_entry = entry.clone();
        let promise = spawn_shared(async move {
            let future = drain_entry.promise.lock().clone().unwrap();
            if future.await.is_err() {
                return;
            }
            if drain_entry.abort_on_drain.load(Ordering::SeqCst) {
                pool.abort(&drain_entry, reason);
            }
            loop {
                let notified = drain_entry.zero_leases.notified();
                if drain_entry.leases.load(Ordering::SeqCst) == 0 {
                    break;
                }
                notified.await;
            }
            if drain_entry.disposed.swap(true, Ordering::SeqCst) {
                return;
            }
            if let Some(resource) = drain_entry.resource.lock().clone() {
                resource.dispose().await;
            }
        });
        *drain_slot = Some(promise.clone());
        Box::pin(promise)
    }

    fn abort(&self, entry: &Arc<ResourceEntry<Resource>>, reason: Option<String>) {
        if entry.aborted.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(resource) = entry.resource.lock().clone() else {
            return;
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resource.abort(reason);
        }));
    }

    fn unavailable(&self) -> String {
        format!(
            "resource generation {}:{} is withdrawn",
            self.identity.owner, self.identity.generation
        )
    }
}
