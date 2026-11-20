use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt, join_all};
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{
    DisposableRef, DisposableStore, IDisposable, combined_disposable,
};
use crate::_base::di::util::linked_list::LinkedList;
use crate::_base::errors::unexpected_error::{on_unexpected_error, safely_call_listener};
use crate::_base::utils::abort::AbortSignal;

pub type Listener<T> = Arc<dyn Fn(T) + Send + Sync>;

struct SubscriptionInner {
    removed: AtomicBool,
    remove: Mutex<Option<Box<dyn FnOnce() + Send + 'static>>>,
}

#[derive(Clone)]
pub struct Subscription {
    inner: Arc<SubscriptionInner>,
    pub debug_label: Option<String>,
}

impl Subscription {
    pub fn new(
        debug_name: Option<&str>,
        remove: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(SubscriptionInner {
                removed: AtomicBool::new(false),
                remove: Mutex::new(Some(Box::new(remove))),
            }),
            debug_label: debug_name.map(|name| format!("on:{name}")),
        }
    }

    pub fn none() -> Self {
        Self::new(None, || {})
    }

    pub fn dispose(&self) {
        if self.inner.removed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(remove) = self.inner.remove.lock().take() {
            remove();
        }
    }
}

impl IDisposable for Subscription {
    fn dispose(&self) {
        Subscription::dispose(self);
    }
}

pub type EventSubscription = Subscription;

pub struct Event<T>
where
    T: Clone + Send + Sync + 'static,
{
    subscribe_fn: Arc<dyn Fn(Listener<T>) -> Subscription + Send + Sync>,
}

impl<T> Clone for Event<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            subscribe_fn: self.subscribe_fn.clone(),
        }
    }
}

impl<T> Event<T>
where
    T: Clone + Send + Sync + 'static,
{
    pub fn new(
        subscribe: impl Fn(Listener<T>) -> Subscription + Send + Sync + 'static,
    ) -> Self {
        Self {
            subscribe_fn: Arc::new(subscribe),
        }
    }

    pub fn none() -> Self {
        Self::new(|_| Subscription::none())
    }

    pub fn subscribe(
        &self,
        listener: impl Fn(T) + Send + Sync + 'static,
    ) -> Subscription {
        (self.subscribe_fn)(Arc::new(listener))
    }

    pub fn subscribe_arc(&self, listener: Listener<T>) -> Subscription {
        (self.subscribe_fn)(listener)
    }

    pub fn subscribe_with_store(
        &self,
        listener: impl Fn(T) + Send + Sync + 'static,
        store: &DisposableStore,
    ) -> Subscription {
        let subscription = self.subscribe(listener);
        store.add(Arc::new(subscription.clone()));
        subscription
    }

    pub fn once(self) -> Self {
        let subscribe = self.subscribe_fn.clone();
        Event::new(move |listener| {
            let fired = Arc::new(AtomicBool::new(false));
            let subscription = Arc::new(Mutex::new(None::<Subscription>));
            let current = subscribe(Arc::new({
                let fired = fired.clone();
                let subscription = subscription.clone();
                move |value| {
                    if fired.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    if let Some(subscription) = subscription.lock().as_ref() {
                        subscription.dispose();
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        listener(value);
                    }));
                    if let Err(error) = result {
                        let error = error
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| error.downcast_ref::<&str>().map(|value| (*value).to_string()))
                            .unwrap_or_else(|| "unknown listener error".to_string());
                        on_unexpected_error(error);
                    }
                }
            }));
            *subscription.lock() = Some(current.clone());
            current
        })
    }

    pub fn map<O>(self, map: impl Fn(T) -> O + Send + Sync + 'static) -> Event<O>
    where
        O: Clone + Send + Sync + 'static,
    {
        let subscribe = self.subscribe_fn.clone();
        let map = Arc::new(map);
        Event::new(move |listener| {
            let map = map.clone();
            subscribe(Arc::new(move |value| listener(map(value))))
        })
    }

    pub fn filter(self, filter: impl Fn(&T) -> bool + Send + Sync + 'static) -> Self {
        let subscribe = self.subscribe_fn.clone();
        let filter = Arc::new(filter);
        Event::new(move |listener| {
            let filter = filter.clone();
            subscribe(Arc::new(move |value| {
                if filter(&value) {
                    listener(value);
                }
            }))
        })
    }

    pub fn any(events: Vec<Event<T>>) -> Self {
        Event::new(move |listener| {
            let subscriptions = events
                .iter()
                .map(|event| {
                    Arc::new(event.subscribe_arc(listener.clone())) as DisposableRef
                })
                .collect::<Vec<_>>();
            let combined = combined_disposable(subscriptions);
            Subscription::new(None, move || combined.dispose())
        })
    }
}

struct EmitterInner<T>
where
    T: Clone + Send + Sync + 'static,
{
    listeners: Mutex<IndexMap<u64, Listener<T>>>,
    next_listener: AtomicU64,
    disposed: AtomicBool,
}

pub struct Emitter<T>
where
    T: Clone + Send + Sync + 'static,
{
    inner: Arc<EmitterInner<T>>,
    pub debug_name: Option<String>,
}

impl<T> Clone for Emitter<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            debug_name: self.debug_name.clone(),
        }
    }
}

impl<T> Emitter<T>
where
    T: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self::with_debug_name(None)
    }

    pub fn with_debug_name(debug_name: Option<&str>) -> Self {
        Self {
            inner: Arc::new(EmitterInner {
                listeners: Mutex::new(IndexMap::new()),
                next_listener: AtomicU64::new(1),
                disposed: AtomicBool::new(false),
            }),
            debug_name: debug_name.map(str::to_owned),
        }
    }

    pub fn event(&self) -> Event<T> {
        let inner = self.inner.clone();
        let debug_name = self.debug_name.clone();
        Event::new(move |listener| {
            if inner.disposed.load(Ordering::SeqCst) {
                return Subscription::none();
            }
            let id = inner.next_listener.fetch_add(1, Ordering::Relaxed);
            inner.listeners.lock().insert(id, listener);
            let inner = inner.clone();
            let debug_name = debug_name.clone();
            Subscription::new(debug_name.as_deref(), move || {
                if !inner.disposed.load(Ordering::SeqCst) {
                    inner.listeners.lock().shift_remove(&id);
                }
            })
        })
    }

    pub fn listener_count(&self) -> usize {
        self.inner.listeners.lock().len()
    }

    pub fn fire(&self, value: T) {
        if self.inner.disposed.load(Ordering::SeqCst) {
            return;
        }
        let snapshot = self
            .inner
            .listeners
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in snapshot {
            let value = value.clone();
            safely_call_listener(|| listener(value));
        }
    }

    pub fn dispose(&self) {
        if self.inner.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.listeners.lock().clear();
    }

    pub fn is_disposed(&self) -> bool {
        self.inner.disposed.load(Ordering::SeqCst)
    }

    fn listeners_snapshot(&self) -> Option<Vec<Listener<T>>> {
        if self.is_disposed() {
            return None;
        }
        let listeners = self
            .inner
            .listeners
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        Some(listeners)
    }
}

impl<T> Default for Emitter<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T> IDisposable for Emitter<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn dispose(&self) {
        Emitter::dispose(self);
    }
}

pub type WaitUntilFuture = BoxFuture<'static, Result<(), String>>;
pub type WaitUntil = Arc<dyn Fn(WaitUntilFuture) -> Result<(), String> + Send + Sync>;

pub trait IWaitUntil: Clone + Send + Sync + 'static {
    type Data: Clone + Send + Sync + 'static;

    fn with_wait_until(data: Self::Data, signal: AbortSignal, wait_until: WaitUntil) -> Self;
}

pub type IWaitUntilData<T> = <T as IWaitUntil>::Data;

#[derive(Clone)]
struct AsyncDeliveryEntry<T>
where
    T: IWaitUntil,
{
    seq: u64,
    deliver: Arc<dyn Fn(T) + Send + Sync>,
    data: T::Data,
}

pub struct AsyncEmitter<T>
where
    T: IWaitUntil,
{
    emitter: Emitter<T>,
    async_delivery_queue: Arc<Mutex<LinkedList<AsyncDeliveryEntry<T>>>>,
    async_delivery_seq: Arc<AtomicU64>,
}

impl<T> AsyncEmitter<T>
where
    T: IWaitUntil,
{
    pub fn new() -> Self {
        Self {
            emitter: Emitter::new(),
            async_delivery_queue: Arc::new(Mutex::new(LinkedList::new())),
            async_delivery_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn event(&self) -> Event<T> {
        self.emitter.event()
    }

    pub fn listener_count(&self) -> usize {
        self.emitter.listener_count()
    }

    pub fn dispose(&self) {
        self.emitter.dispose();
    }

    pub fn is_disposed(&self) -> bool {
        self.emitter.is_disposed()
    }

    pub fn fire_async_concurrent(
        &self,
        data: T::Data,
        signal: AbortSignal,
    ) -> BoxFuture<'static, ()> {
        if self.is_disposed() || signal.aborted() {
            return async {}.boxed();
        }
        let Some(listeners) = self.emitter.listeners_snapshot() else {
            return async {}.boxed();
        };
        let deliveries = listeners
            .into_iter()
            .map(|listener| {
                let data = data.clone();
                let signal = signal.clone();
                async move {
                    deliver_async(
                        Arc::new(move |event| listener(event)),
                        data,
                        signal,
                    )
                    .await;
                }
            })
            .collect::<Vec<_>>();
        async move {
            join_all(deliveries).await;
        }
        .boxed()
    }

    pub fn fire_async(&self, data: T::Data, signal: AbortSignal) -> BoxFuture<'static, ()> {
        if self.is_disposed() {
            return async {}.boxed();
        }
        let Some(listeners) = self.emitter.listeners_snapshot() else {
            return async {}.boxed();
        };
        {
            let queue = self.async_delivery_queue.lock();
            for listener in listeners {
                let seq = self.async_delivery_seq.fetch_add(1, Ordering::SeqCst) + 1;
                queue.push(AsyncDeliveryEntry {
                    seq,
                    deliver: Arc::new(move |event| listener(event)),
                    data: data.clone(),
                });
            }
        }
        let queue_handle = self.async_delivery_queue.clone();
        let seq_handle = self.async_delivery_seq.clone();
        async move {
            let mut watermark = u64::MAX;
            loop {
                let head = {
                    let mut queue = queue_handle.lock();
                    let Some(head) = queue.peek() else {
                        break;
                    };
                    if head.seq > watermark || signal.aborted() {
                        break;
                    }
                    queue.shift().unwrap()
                };
                watermark = seq_handle.load(Ordering::SeqCst);
                deliver_async(head.deliver, head.data, signal.clone()).await;
            }
        }
        .boxed()
    }
}

impl<T> Default for AsyncEmitter<T>
where
    T: IWaitUntil,
{
    fn default() -> Self {
        Self::new()
    }
}

async fn deliver_async<T>(
    deliver: Arc<dyn Fn(T) + Send + Sync>,
    data: T::Data,
    signal: AbortSignal,
) where
    T: IWaitUntil,
{
    let thenables = Arc::new(Mutex::new(Vec::<WaitUntilFuture>::new()));
    let frozen = Arc::new(AtomicBool::new(false));
    let wait_until: WaitUntil = Arc::new({
        let thenables = thenables.clone();
        let frozen = frozen.clone();
        move |future| {
            if frozen.load(Ordering::SeqCst) {
                return Err("waitUntil can NOT be called asynchronously".to_string());
            }
            thenables.lock().push(future);
            Ok(())
        }
    });
    let event = T::with_wait_until(data, signal, wait_until);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| deliver(event)));
    if let Err(error) = result {
        let error = error
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| error.downcast_ref::<&str>().map(|value| (*value).to_string()))
            .unwrap_or_else(|| "unknown listener error".to_string());
        on_unexpected_error(error);
        return;
    }
    frozen.store(true, Ordering::SeqCst);
    let thenables = thenables.lock().drain(..).collect::<Vec<_>>();
    for result in join_all(thenables).await {
        if let Err(error) = result {
            on_unexpected_error(error);
        }
    }
}

pub enum Veto {
    Value(bool),
    Future(BoxFuture<'static, Result<bool, String>>),
}

pub fn handle_vetos(
    vetos: Vec<Veto>,
    on_error: Arc<dyn Fn(String) + Send + Sync>,
) -> BoxFuture<'static, bool> {
    if vetos.is_empty() {
        return async { false }.boxed();
    }
    let vetos = vetos
        .into_iter()
        .map(|veto| match veto {
            Veto::Value(value) => Veto::Value(value),
            Veto::Future(future) => {
                let task = tokio::spawn(future);
                Veto::Future(
                    async move {
                        match task.await {
                            Ok(result) => result,
                            Err(_) => std::future::pending::<Result<bool, String>>().await,
                        }
                    }
                    .boxed(),
                )
            }
        })
        .collect::<Vec<_>>();
    let mut promises = Vec::new();
    let lazy_value = Arc::new(AtomicBool::new(false));
    for value in vetos {
        match value {
            Veto::Value(true) => return async { true }.boxed(),
            Veto::Value(false) => {}
            Veto::Future(future) => {
                let lazy_value = lazy_value.clone();
                let on_error = on_error.clone();
                promises.push(async move {
                    match future.await {
                        Ok(value) => {
                            if value {
                                lazy_value.store(true, Ordering::SeqCst);
                            }
                        }
                        Err(error) => {
                            on_error(error);
                            lazy_value.store(true, Ordering::SeqCst);
                        }
                    }
                }
                .boxed());
            }
        }
    }
    async move {
        join_all(promises).await;
        lazy_value.load(Ordering::SeqCst)
    }
    .boxed()
}
