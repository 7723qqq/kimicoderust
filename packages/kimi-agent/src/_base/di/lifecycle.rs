use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt, Shared};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use super::dependency_graph::ObjectId;
use super::descriptors::DynamicValue;
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::lifecycle::ledger::{Ledger, LedgerEntry};

pub trait IDisposable: Send + Sync {
    fn dispose(&self);

    fn debug_label(&self) -> Option<&str> {
        None
    }
}

pub type DisposableRef = Arc<dyn IDisposable>;

pub trait IDisposableTracker: Send + Sync {
    fn track_disposable(&self, disposable: DisposableRef);
    fn set_parent(&self, child: ObjectId, parent: Option<DisposableRef>);
    fn mark_as_disposed(&self, disposable: ObjectId);
    fn mark_as_singleton(&self, disposable: DisposableRef);
}

struct DisposableInfo {
    value: DisposableRef,
    source: Option<String>,
    parent: Option<DisposableRef>,
    is_singleton: bool,
    idx: usize,
}

static NEXT_DISPOSABLE_IDX: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
pub struct DisposableTracker {
    living_disposables: Mutex<IndexMap<ObjectId, DisposableInfo>>,
}

impl DisposableTracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure(&self, disposable: DisposableRef) -> ObjectId {
        let id = disposable_id(&disposable);
        self.living_disposables
            .lock()
            .entry(id)
            .or_insert_with(|| DisposableInfo {
                value: disposable,
                source: None,
                parent: None,
                is_singleton: false,
                idx: NEXT_DISPOSABLE_IDX.fetch_add(1, Ordering::Relaxed),
            });
        id
    }

    pub fn get_tracked_disposables(&self) -> Vec<DisposableRef> {
        let living = self.living_disposables.lock();
        let mut cache = HashMap::<ObjectId, bool>::new();
        living
            .iter()
            .filter(|(id, info)| {
                info.source.is_some() && !root_is_singleton(**id, &living, &mut cache)
            })
            .map(|(_, info)| info.value.clone())
            .collect()
    }
}

fn root_is_singleton(
    id: ObjectId,
    living: &IndexMap<ObjectId, DisposableInfo>,
    cache: &mut HashMap<ObjectId, bool>,
) -> bool {
    if let Some(value) = cache.get(&id) {
        return *value;
    }
    let Some(info) = living.get(&id) else {
        return false;
    };
    let result = if let Some(parent) = &info.parent {
        root_is_singleton(disposable_id(parent), living, cache)
    } else {
        info.is_singleton
    };
    cache.insert(id, result);
    result
}

impl IDisposableTracker for DisposableTracker {
    fn track_disposable(&self, disposable: DisposableRef) {
        let id = self.ensure(disposable);
        let mut living = self.living_disposables.lock();
        if let Some(info) = living.get_mut(&id)
            && info.source.is_none()
        {
            info.source = Some(std::backtrace::Backtrace::capture().to_string());
        }
    }

    fn set_parent(&self, child: ObjectId, parent: Option<DisposableRef>) {
        if let Some(info) = self.living_disposables.lock().get_mut(&child) {
            info.parent = parent;
        }
    }

    fn mark_as_disposed(&self, disposable: ObjectId) {
        self.living_disposables.lock().shift_remove(&disposable);
    }

    fn mark_as_singleton(&self, disposable: DisposableRef) {
        let id = self.ensure(disposable);
        if let Some(info) = self.living_disposables.lock().get_mut(&id) {
            info.is_singleton = true;
        }
    }
}

static DISPOSABLE_TRACKER: Lazy<Mutex<Option<Arc<dyn IDisposableTracker>>>> =
    Lazy::new(|| Mutex::new(None));

pub fn set_disposable_tracker(tracker: Option<Arc<dyn IDisposableTracker>>) {
    *DISPOSABLE_TRACKER.lock() = tracker;
}

pub fn track_disposable(disposable: DisposableRef) -> DisposableRef {
    if let Some(tracker) = DISPOSABLE_TRACKER.lock().clone() {
        tracker.track_disposable(disposable.clone());
    }
    disposable
}

pub fn mark_as_disposed(disposable: ObjectId) {
    if let Some(tracker) = DISPOSABLE_TRACKER.lock().clone() {
        tracker.mark_as_disposed(disposable);
    }
}

fn set_parent_of_disposable(child: DisposableRef, parent: Option<DisposableRef>) {
    if let Some(tracker) = DISPOSABLE_TRACKER.lock().clone() {
        tracker.set_parent(disposable_id(&child), parent);
    }
}

fn set_parent_of_disposables(children: &[DisposableRef], parent: Option<DisposableRef>) {
    let Some(tracker) = DISPOSABLE_TRACKER.lock().clone() else {
        return;
    };
    for child in children {
        tracker.set_parent(disposable_id(child), parent.clone());
    }
}

pub fn mark_as_singleton(singleton: DisposableRef) -> DisposableRef {
    if let Some(tracker) = DISPOSABLE_TRACKER.lock().clone() {
        tracker.mark_as_singleton(singleton.clone());
    }
    singleton
}

pub fn disposable_id(disposable: &DisposableRef) -> ObjectId {
    Arc::as_ptr(disposable) as *const () as ObjectId
}

fn disposable_label(disposable: &DisposableRef) -> String {
    disposable
        .debug_label()
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "disposable:anonymous".to_string())
}

#[derive(Debug)]
pub struct DisposeError {
    pub errors: Vec<String>,
}

impl std::fmt::Display for DisposeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.errors.len() == 1 {
            formatter.write_str(&self.errors[0])
        } else {
            formatter.write_str("Encountered errors while disposing of store")
        }
    }
}

impl std::error::Error for DisposeError {}

pub fn dispose(disposable: &DisposableRef) {
    disposable.dispose();
}

pub fn dispose_all(disposables: impl IntoIterator<Item = DisposableRef>) -> Result<(), DisposeError> {
    let mut errors = Vec::new();
    for disposable in disposables {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            disposable.dispose();
        }));
        if let Err(error) = result {
            errors.push(
                error
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| error.downcast_ref::<&str>().map(|value| (*value).to_string()))
                    .unwrap_or_else(|| "unknown disposal error".to_string()),
            );
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(DisposeError { errors })
    }
}

pub fn dispose_if_disposable(disposables: Vec<Option<DisposableRef>>) -> Result<(), DisposeError> {
    dispose_all(disposables.into_iter().flatten())
}

struct FunctionDisposable {
    id: ObjectId,
    function: Mutex<Option<Box<dyn FnOnce() + Send + 'static>>>,
}

impl IDisposable for FunctionDisposable {
    fn dispose(&self) {
        let Some(function) = self.function.lock().take() else {
            return;
        };
        mark_as_disposed(self.id);
        function();
    }
}

static NEXT_DISPOSABLE_ID: AtomicUsize = AtomicUsize::new(1);

fn next_disposable_id() -> ObjectId {
    NEXT_DISPOSABLE_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn to_disposable(function: impl FnOnce() + Send + 'static) -> DisposableRef {
    let disposable = Arc::new(FunctionDisposable {
        id: next_disposable_id(),
        function: Mutex::new(Some(Box::new(function))),
    });
    track_disposable(disposable)
}

pub fn combined_disposable(disposables: Vec<DisposableRef>) -> DisposableRef {
    let children = disposables.clone();
    let parent = to_disposable(move || {
        let _ = dispose_all(disposables);
    });
    set_parent_of_disposables(&children, Some(parent.clone()));
    parent
}

#[derive(Clone)]
pub struct DisposableStore {
    inner: Arc<DisposableStoreInner>,
}

struct DisposableStoreInner {
    id: ObjectId,
    ledger: Ledger,
    entries: Mutex<IndexMap<ObjectId, (DisposableRef, LedgerEntry)>>,
    is_disposed: Mutex<bool>,
}

impl Default for DisposableStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DisposableStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DisposableStoreInner {
                id: next_disposable_id(),
                ledger: Ledger::new("DisposableStore"),
                entries: Mutex::new(IndexMap::new()),
                is_disposed: Mutex::new(false),
            }),
        }
    }

    pub fn ledger(&self) -> Ledger {
        self.inner.ledger.clone()
    }

    pub fn add(&self, disposable: DisposableRef) -> DisposableRef {
        assert_ne!(disposable_id(&disposable), self.inner.id, "Cannot register a disposable on itself!");
        set_parent_of_disposable(
            disposable.clone(),
            Some(self.clone() as DisposableRef),
        );
        if self.is_disposed() {
            disposable.dispose();
            return disposable;
        }
        let id = disposable_id(&disposable);
        if !self.inner.entries.lock().contains_key(&id) {
            let callback = disposable.clone();
            let entry = self.inner.ledger.register(
                move |_| {
                    callback.dispose();
                },
                &disposable_label(&disposable),
            );
            self.inner
                .entries
                .lock()
                .insert(id, (disposable.clone(), entry));
        }
        disposable
    }

    pub fn delete(&self, disposable: &DisposableRef) {
        if self.is_disposed() {
            return;
        }
        assert_ne!(disposable_id(disposable), self.inner.id, "Cannot dispose a disposable on itself!");
        if let Some((_, entry)) = self
            .inner
            .entries
            .lock()
            .shift_remove(&disposable_id(disposable))
        {
            entry.release();
        }
        disposable.dispose();
    }

    pub fn delete_and_leak(&self, disposable: &DisposableRef) {
        if self.is_disposed() {
            return;
        }
        if let Some((value, entry)) = self
            .inner
            .entries
            .lock()
            .shift_remove(&disposable_id(disposable))
        {
            entry.release();
            set_parent_of_disposable(value, None);
        }
    }

    pub fn clear(&self) {
        if self.inner.entries.lock().is_empty() {
            return;
        }
        if let Some(future) = self.inner.ledger.clear("scope-close") {
            tokio::spawn(future);
        }
        self.inner.entries.lock().clear();
    }

    pub fn is_disposed(&self) -> bool {
        *self.inner.is_disposed.lock()
    }

    pub fn assert_not_disposed(&self) {
        if self.is_disposed() {
            on_unexpected_error("Object disposed");
        }
    }
}

impl IDisposable for DisposableStore {
    fn dispose(&self) {
        if *self.inner.is_disposed.lock() {
            return;
        }
        *self.inner.is_disposed.lock() = true;
        mark_as_disposed(self.inner.id);
        if let Some(future) = self.inner.ledger.teardown("scope-close") {
            tokio::spawn(future);
        }
        self.inner.entries.lock().clear();
    }
}

#[derive(Clone, Default)]
pub struct Disposable {
    pub store: DisposableStore,
}

impl Disposable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, disposable: DisposableRef) -> DisposableRef {
        self.store.add(disposable)
    }
}

impl IDisposable for Disposable {
    fn dispose(&self) {
        self.store.dispose();
    }
}

pub struct NoneDisposable;

impl IDisposable for NoneDisposable {
    fn dispose(&self) {}
}

pub static DISPOSABLE_NONE: Lazy<DisposableRef> = Lazy::new(|| Arc::new(NoneDisposable));

pub struct MutableDisposable<T>
where
    T: IDisposable + 'static,
{
    id: ObjectId,
    ledger: Ledger,
    entry: Mutex<Option<LedgerEntry>>,
    value: Mutex<Option<Arc<T>>>,
    is_disposed: Mutex<bool>,
}

impl<T> Default for MutableDisposable<T>
where
    T: IDisposable + 'static,
{
    fn default() -> Self {
        Self {
            id: next_disposable_id(),
            ledger: Ledger::new("MutableDisposable"),
            entry: Mutex::new(None),
            value: Mutex::new(None),
            is_disposed: Mutex::new(false),
        }
    }
}

impl<T> MutableDisposable<T>
where
    T: IDisposable + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    pub fn value(&self) -> Option<Arc<T>> {
        if *self.is_disposed.lock() {
            None
        } else {
            self.value.lock().clone()
        }
    }

    pub fn set_value(&self, value: Option<Arc<T>>) {
        if *self.is_disposed.lock() {
            if let Some(value) = value {
                value.dispose();
            }
            return;
        }
        let current = self.value.lock().clone();
        if match (&current, &value) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        } {
            return;
        }
        if let Some(entry) = self.entry.lock().take() {
            entry.release();
        }
        if let Some(previous) = current {
            previous.dispose();
        }
        if let Some(value) = &value {
            set_parent_of_disposable(value.clone(), Some(Arc::new(MutableDisposableParent { id: self.id })));
        }
        *self.value.lock() = value.clone();
        if let Some(value) = value {
            let callback = value.clone();
            *self.entry.lock() = Some(self.ledger.register(
                move |_| {
                    callback.dispose();
                },
                "disposable:anonymous",
            ));
        }
    }

    pub fn clear(&self) {
        if !*self.is_disposed.lock() {
            self.set_value(None);
        }
    }

    pub fn clear_and_leak(&self) -> Option<Arc<T>> {
        if *self.is_disposed.lock() {
            return None;
        }
        let previous = self.value.lock().take();
        if let Some(entry) = self.entry.lock().take() {
            entry.release();
        }
        if let Some(previous) = &previous {
            set_parent_of_disposable(previous.clone(), None);
        }
        previous
    }
}

struct MutableDisposableParent {
    id: ObjectId,
}

impl IDisposable for MutableDisposableParent {
    fn dispose(&self) {
        mark_as_disposed(self.id);
    }
}

impl<T> IDisposable for MutableDisposable<T>
where
    T: IDisposable + 'static,
{
    fn dispose(&self) {
        if *self.is_disposed.lock() {
            return;
        }
        *self.is_disposed.lock() = true;
        mark_as_disposed(self.id);
        if let Some(entry) = self.entry.lock().take() {
            entry.release();
        }
        let previous = self.value.lock().take();
        if let Some(future) = self.ledger.teardown("scope-close") {
            tokio::spawn(future);
        }
        if let Some(previous) = previous {
            previous.dispose();
        }
    }
}

pub struct MandatoryMutableDisposable<T>
where
    T: IDisposable + 'static,
{
    disposable: Arc<MutableDisposable<T>>,
    is_disposed: Mutex<bool>,
}

impl<T> MandatoryMutableDisposable<T>
where
    T: IDisposable + 'static,
{
    pub fn new(initial_value: Arc<T>) -> Self {
        let disposable = Arc::new(MutableDisposable::new());
        disposable.set_value(Some(initial_value));
        Self {
            disposable,
            is_disposed: Mutex::new(false),
        }
    }

    pub fn value(&self) -> Arc<T> {
        self.disposable.value().expect("mandatory disposable has no value")
    }

    pub fn set_value(&self, value: Arc<T>) {
        if *self.is_disposed.lock() || Arc::ptr_eq(&value, &self.value()) {
            return;
        }
        self.disposable.set_value(Some(value));
    }
}

impl<T> IDisposable for MandatoryMutableDisposable<T>
where
    T: IDisposable + 'static,
{
    fn dispose(&self) {
        if *self.is_disposed.lock() {
            return;
        }
        *self.is_disposed.lock() = true;
        self.disposable.dispose();
    }
}

pub struct RefCountedDisposable {
    counter: Mutex<i64>,
    disposable: DisposableRef,
}

impl RefCountedDisposable {
    pub fn new(disposable: DisposableRef) -> Arc<Self> {
        Arc::new(Self {
            counter: Mutex::new(1),
            disposable,
        })
    }

    pub fn acquire(self: &Arc<Self>) -> Arc<Self> {
        *self.counter.lock() += 1;
        self.clone()
    }

    pub fn release(self: &Arc<Self>) -> Arc<Self> {
        let mut counter = self.counter.lock();
        *counter -= 1;
        let dispose = *counter == 0;
        drop(counter);
        if dispose {
            self.disposable.dispose();
        }
        self.clone()
    }
}

pub trait IReference<T>: IDisposable {
    fn object(&self) -> Arc<T>;
}

pub struct Reference<T>
where
    T: Send + Sync + 'static,
{
    object: Arc<T>,
    dispose: Mutex<Option<Box<dyn FnOnce() + Send + 'static>>>,
}

impl<T> IReference<T> for Reference<T>
where
    T: Send + Sync + 'static,
{
    fn object(&self) -> Arc<T> {
        self.object.clone()
    }
}

impl<T> IDisposable for Reference<T>
where
    T: Send + Sync + 'static,
{
    fn dispose(&self) {
        if let Some(dispose) = self.dispose.lock().take() {
            dispose();
        }
    }
}

pub trait ReferenceCollection<T>: Send + Sync
where
    T: Send + Sync + 'static,
{
    fn create_referenced_object(&self, key: &str, args: Vec<DynamicValue>) -> Arc<T>;
    fn destroy_referenced_object(&self, key: &str, object: Arc<T>);
    fn references(&self) -> &Mutex<IndexMap<String, ReferenceEntry<T>>>;

    fn acquire(self: &Arc<Self>, key: &str, args: Vec<DynamicValue>) -> Arc<dyn IReference<T>> {
        let mut references = self.references().lock();
        let reference = references.entry(key.to_string()).or_insert_with(|| ReferenceEntry {
            object: self.create_referenced_object(key, args),
            counter: 0,
        });
        let object = reference.object.clone();
        reference.counter += 1;
        drop(references);
        let this = self.clone();
        let key = key.to_string();
        let disposed = Arc::new(Mutex::new(false));
        let dispose = {
            let disposed = disposed.clone();
            move || {
                if *disposed.lock() {
                    return;
                }
                *disposed.lock() = true;
                let mut references = this.references().lock();
                let Some(reference) = references.get_mut(&key) else {
                    return;
                };
                reference.counter -= 1;
                if reference.counter != 0 {
                    return;
                }
                let object = reference.object.clone();
                this.destroy_referenced_object(&key, object);
                references.shift_remove(&key);
            }
        };
        Arc::new(Reference {
            object,
            dispose: Mutex::new(Some(Box::new(dispose))),
        })
    }
}

pub struct ReferenceEntry<T> {
    pub object: Arc<T>,
    pub counter: usize,
}

pub type SharedReferenceFuture<T> = Shared<BoxFuture<'static, Result<Arc<T>, String>>>;

pub struct AsyncReferenceCollection<T, C>
where
    T: Send + Sync + 'static,
    C: ReferenceCollection<SharedReferenceFuture<T>>,
{
    reference_collection: Arc<C>,
}

impl<T, C> AsyncReferenceCollection<T, C>
where
    T: Send + Sync + 'static,
    C: ReferenceCollection<SharedReferenceFuture<T>>,
{
    pub fn new(reference_collection: Arc<C>) -> Self {
        Self {
            reference_collection,
        }
    }

    pub async fn acquire(
        &self,
        key: &str,
        args: Vec<DynamicValue>,
    ) -> Result<Arc<dyn IReference<T>>, String> {
        let reference = self.reference_collection.acquire(key, args);
        let future = reference.object().as_ref().clone();
        match future.await {
            Ok(object) => {
                let disposed = Arc::new(Mutex::new(false));
                let dispose = {
                    let disposed = disposed.clone();
                    let reference = reference.clone();
                    move || {
                        if *disposed.lock() {
                            return;
                        }
                        *disposed.lock() = true;
                        reference.dispose();
                    }
                };
                Ok(Arc::new(Reference {
                    object,
                    dispose: Mutex::new(Some(Box::new(dispose))),
                }))
            }
            Err(error) => {
                reference.dispose();
                Err(error)
            }
        }
    }
}

pub struct ImmortalReference<T>
where
    T: Send + Sync + 'static,
{
    pub object: Arc<T>,
}

impl<T> IReference<T> for ImmortalReference<T>
where
    T: Send + Sync + 'static,
{
    fn object(&self) -> Arc<T> {
        self.object.clone()
    }
}

impl<T> IDisposable for ImmortalReference<T>
where
    T: Send + Sync + 'static,
{
    fn dispose(&self) {}
}

pub struct DisposableMap<K, V>
where
    K: Eq + std::hash::Hash,
    V: IDisposable + 'static,
{
    store: Mutex<IndexMap<K, Arc<V>>>,
    is_disposed: Mutex<bool>,
}

impl<K, V> Default for DisposableMap<K, V>
where
    K: Eq + std::hash::Hash,
    V: IDisposable + 'static,
{
    fn default() -> Self {
        Self {
            store: Mutex::new(IndexMap::new()),
            is_disposed: Mutex::new(false),
        }
    }
}

impl<K, V> DisposableMap<K, V>
where
    K: Eq + std::hash::Hash,
    V: IDisposable + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear_and_dispose_all(&self) {
        let values = self
            .store
            .lock()
            .drain(..)
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        for value in values {
            value.dispose();
        }
    }

    pub fn has(&self, key: &K) -> bool {
        self.store.lock().contains_key(key)
    }

    pub fn size(&self) -> usize {
        self.store.lock().len()
    }

    pub fn get(&self, key: &K) -> Option<Arc<V>> {
        self.store.lock().get(key).cloned()
    }

    pub fn set(&self, key: K, value: Arc<V>, skip_dispose_on_overwrite: bool) {
        if *self.is_disposed.lock() {
            return;
        }
        if !skip_dispose_on_overwrite {
            let previous = self.store.lock().get(&key).cloned();
            if let Some(previous) = previous
                && !Arc::ptr_eq(&previous, &value)
            {
                previous.dispose();
            }
        }
        self.store.lock().insert(key, value);
    }

    pub fn delete_and_dispose(&self, key: &K) {
        if let Some(value) = self.store.lock().shift_remove(key) {
            value.dispose();
        }
    }

    pub fn delete_and_leak(&self, key: &K) -> Option<Arc<V>> {
        self.store.lock().shift_remove(key)
    }

    pub fn keys(&self) -> Vec<K>
    where
        K: Clone,
    {
        self.store.lock().keys().cloned().collect()
    }

    pub fn values(&self) -> Vec<Arc<V>> {
        self.store.lock().values().cloned().collect()
    }
}

impl<K, V> IDisposable for DisposableMap<K, V>
where
    K: Eq + std::hash::Hash + Send + Sync,
    V: IDisposable + 'static,
{
    fn dispose(&self) {
        if *self.is_disposed.lock() {
            return;
        }
        *self.is_disposed.lock() = true;
        self.clear_and_dispose_all();
    }
}

pub struct DisposableSet<V>
where
    V: IDisposable + 'static,
{
    store: Mutex<IndexMap<ObjectId, Arc<V>>>,
    is_disposed: Mutex<bool>,
}

impl<V> Default for DisposableSet<V>
where
    V: IDisposable + 'static,
{
    fn default() -> Self {
        Self {
            store: Mutex::new(IndexMap::new()),
            is_disposed: Mutex::new(false),
        }
    }
}

impl<V> DisposableSet<V>
where
    V: IDisposable + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    fn id(value: &Arc<V>) -> ObjectId {
        Arc::as_ptr(value) as *const () as ObjectId
    }

    pub fn clear_and_dispose_all(&self) {
        let values = self
            .store
            .lock()
            .drain(..)
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        for value in values {
            value.dispose();
        }
    }

    pub fn has(&self, value: &Arc<V>) -> bool {
        self.store.lock().contains_key(&Self::id(value))
    }

    pub fn size(&self) -> usize {
        self.store.lock().len()
    }

    pub fn add(&self, value: Arc<V>) {
        if *self.is_disposed.lock() {
            return;
        }
        self.store.lock().insert(Self::id(&value), value);
    }

    pub fn delete_and_dispose(&self, value: &Arc<V>) {
        if let Some(value) = self.store.lock().shift_remove(&Self::id(value)) {
            value.dispose();
        }
    }

    pub fn delete_and_leak(&self, value: &Arc<V>) -> Option<Arc<V>> {
        self.store.lock().shift_remove(&Self::id(value))
    }

    pub fn values(&self) -> Vec<Arc<V>> {
        self.store.lock().values().cloned().collect()
    }
}

impl<V> IDisposable for DisposableSet<V>
where
    V: IDisposable + 'static,
{
    fn dispose(&self) {
        if *self.is_disposed.lock() {
            return;
        }
        *self.is_disposed.lock() = true;
        self.clear_and_dispose_all();
    }
}

pub fn dispose_on_return(callback: impl FnOnce(&DisposableStore)) {
    let store = DisposableStore::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(&store)));
    store.dispose();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

pub fn then_if_not_disposed<F>(
    future: F,
    then: impl FnOnce(F::Output) + Send + 'static,
) -> DisposableRef
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let disposed = Arc::new(Mutex::new(false));
    let task_disposed = disposed.clone();
    tokio::spawn(async move {
        let result = future.await;
        if !*task_disposed.lock() {
            then(result);
        }
    });
    to_disposable(move || {
        *disposed.lock() = true;
    })
}

pub async fn then_register_or_dispose<T>(
    future: impl Future<Output = Arc<T>>,
    store: DisposableStore,
) -> Arc<T>
where
    T: IDisposable + 'static,
{
    let disposable = future.await;
    if store.is_disposed() {
        disposable.dispose();
    } else {
        store.add(disposable.clone());
    }
    disposable
}
