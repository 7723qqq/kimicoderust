use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::_base::errors::errors::SharedError;

#[derive(Clone, Debug)]
pub struct AbortError {
    pub message: String,
}

impl AbortError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for AbortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AbortError {}

pub fn abort_error() -> AbortError {
    AbortError::new("Aborted")
}

pub fn is_abort_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error.is::<AbortError>() || error.is::<UserCancellationError>()
}

#[derive(Clone, Debug)]
pub struct UserCancellationError {
    pub user_cancelled: bool,
}

impl fmt::Display for UserCancellationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Aborted by the user")
    }
}

impl std::error::Error for UserCancellationError {}

pub fn user_cancellation_reason() -> UserCancellationError {
    UserCancellationError {
        user_cancelled: true,
    }
}

pub fn is_user_cancellation(error: &(dyn std::error::Error + 'static)) -> bool {
    error.is::<UserCancellationError>()
}

struct AbortInner {
    aborted: AtomicBool,
    reason: Mutex<Option<SharedError>>,
    listeners: Mutex<Vec<(u64, Arc<dyn Fn() + Send + Sync>)>>,
    next_listener: AtomicU64,
    // Controllers kept alive by derived signals (`any`/`timeout`), cleared on
    // abort so the signal -> controller -> signal cycle is broken.
    keepers: Mutex<Vec<Arc<AbortController>>>,
}

#[derive(Clone)]
pub struct AbortSignal {
    inner: Arc<AbortInner>,
}

impl AbortSignal {
    pub fn aborted(&self) -> bool {
        self.inner.aborted.load(Ordering::SeqCst)
    }

    pub fn reason(&self) -> Option<SharedError> {
        self.inner.reason.lock().clone()
    }

    pub fn throw_if_aborted(&self) -> Result<(), crate::wire::record::WireAsyncError> {
        if !self.aborted() {
            return Ok(());
        }
        let reason = abort_reason(self);
        if let Some(error) = reason.downcast_ref::<AbortError>() {
            return Err(Box::new(error.clone()));
        }
        if let Some(error) = reason.downcast_ref::<UserCancellationError>() {
            return Err(Box::new(error.clone()));
        }
        Err(reason.into())
    }

    pub fn add_event_listener(
        &self,
        type_: &str,
        listener: Arc<dyn Fn() + Send + Sync>,
        _once: bool,
    ) {
        if type_ != "abort" {
            return;
        }
        self.listen(listener);
    }

    pub fn remove_event_listener(&self, type_: &str, listener: Arc<dyn Fn() + Send + Sync>) {
        if type_ != "abort" {
            return;
        }
        self.inner
            .listeners
            .lock()
            .retain(|(_, current)| !Arc::ptr_eq(current, &listener));
    }

    fn listen(&self, listener: Arc<dyn Fn() + Send + Sync>) -> u64 {
        let id = self.inner.next_listener.fetch_add(1, Ordering::Relaxed);
        self.inner.listeners.lock().push((id, listener));
        id
    }

    fn remove_listener(&self, id: u64) {
        self.inner.listeners.lock().retain(|(current, _)| *current != id);
    }

    /// Resolves once this signal aborts (v2 `addEventListener('abort', …)`).
    pub fn cancelled(&self) -> BoxFuture<'static, ()> {
        let signal = self.clone();
        async move {
            if signal.aborted() {
                return;
            }
            let (sender, receiver) = oneshot::channel();
            let sender = Arc::new(Mutex::new(Some(sender)));
            let listener_sender = sender.clone();
            let listener_id = signal.listen(Arc::new(move || {
                if let Some(sender) = listener_sender.lock().take() {
                    let _ = sender.send(());
                }
            }));
            let _listener = AbortListenerGuard { signal: signal.clone(), listener_id };
            if signal.aborted() {
                return;
            }
            let _ = receiver.await;
        }
        .boxed()
    }

    /// Aborts when any source aborts (v2 `AbortSignal.any`).
    pub fn any(signals: &[AbortSignal]) -> AbortSignal {
        let controller = Arc::new(AbortController::new());
        for signal in signals {
            if signal.aborted() {
                controller.abort(signal.reason());
                break;
            }
            let target = Arc::downgrade(&controller);
            let source = signal.clone();
            signal.add_event_listener(
                "abort",
                Arc::new(move || {
                    if let Some(target) = target.upgrade() {
                        target.abort(source.reason());
                    }
                }),
                true,
            );
        }
        let result = controller.signal();
        result.inner.keepers.lock().push(controller);
        result
    }

    /// Aborts after `timeout_ms` (v2 deadline-signal shorthand).
    pub fn timeout(timeout_ms: u64) -> AbortSignal {
        let controller = Arc::new(AbortController::new());
        let deadline_controller = controller.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
            deadline_controller.abort(Some(Arc::new(abort_error()) as SharedError));
        });
        let result = controller.signal();
        result.inner.keepers.lock().push(controller);
        result
    }

    /// Bridges a `CancellationToken` (e.g. an xstate actor's lifetime) into
    /// this signal, the inverse of `cancellation_token`.
    pub fn from_cancellation_token(token: tokio_util::sync::CancellationToken) -> AbortSignal {
        let controller = Arc::new(AbortController::new());
        if token.is_cancelled() {
            controller.abort(Some(Arc::new(abort_error()) as SharedError));
            return controller.signal();
        }
        let target = controller.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            target.abort(Some(Arc::new(abort_error()) as SharedError));
        });
        controller.signal()
    }

    /// Bridges to `CancellationToken` for token-based wire layers.
    pub fn cancellation_token(&self) -> tokio_util::sync::CancellationToken {
        let token = tokio_util::sync::CancellationToken::new();
        if self.aborted() {
            token.cancel();
            return token;
        }
        let signal = self.clone();
        let task_token = token.clone();
        tokio::spawn(async move {
            signal.cancelled().await;
            task_token.cancel();
        });
        token
    }
}

struct AbortListenerGuard {
    signal: AbortSignal,
    listener_id: u64,
}

impl Drop for AbortListenerGuard {
    fn drop(&mut self) {
        self.signal.remove_listener(self.listener_id);
    }
}

#[derive(Clone)]
pub struct AbortController {
    signal: AbortSignal,
}

impl AbortController {
    pub fn new() -> Self {
        Self {
            signal: AbortSignal {
                inner: Arc::new(AbortInner {
                    aborted: AtomicBool::new(false),
                    reason: Mutex::new(None),
                    listeners: Mutex::new(Vec::new()),
                    next_listener: AtomicU64::new(1),
                    keepers: Mutex::new(Vec::new()),
                }),
            },
        }
    }

    pub fn signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    pub fn abort(&self, reason: Option<SharedError>) {
        {
            let mut stored_reason = self.signal.inner.reason.lock();
            if self.signal.aborted() {
                return;
            }
            *stored_reason = reason;
            self.signal.inner.aborted.store(true, Ordering::SeqCst);
        }
        // Break derived-signal cycles once this signal is terminal.
        self.signal.inner.keepers.lock().clear();
        let listeners = self
            .signal
            .inner
            .listeners
            .lock()
            .drain(..)
            .map(|(_, listener)| listener)
            .collect::<Vec<_>>();
        for listener in listeners {
            listener();
        }
    }
}

impl Default for AbortController {
    fn default() -> Self {
        Self::new()
    }
}

pub fn abortable<F>(
    promise: F,
    signal: AbortSignal,
) -> BoxFuture<'static, Result<F::Output, SharedError>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (promise_sender, promise_receiver) = oneshot::channel();
    if signal.aborted() {
        tokio::spawn(async move {
            let result = promise.await;
            let _ = promise_sender.send(result);
        });
        let reason = abort_reason(&signal);
        return async move { Err(reason) }.boxed();
    }
    let (abort_sender, abort_receiver) = oneshot::channel();
    let listener_signal = signal.clone();
    let abort_sender = Arc::new(Mutex::new(Some(abort_sender)));
    let listener_id = signal.listen(Arc::new(move || {
        if let Some(sender) = abort_sender.lock().take() {
            let _ = sender.send(abort_reason(&listener_signal));
        }
    }));
    let task_signal = signal.clone();
    tokio::spawn(async move {
        let result = promise.await;
        task_signal.remove_listener(listener_id);
        let _ = promise_sender.send(result);
    });
    async move {
        tokio::select! {
            result = promise_receiver => {
                match result {
                    Ok(result) => Ok(result),
                    Err(_) => std::future::pending::<Result<F::Output, SharedError>>().await,
                }
            }
            reason = abort_receiver => Err(reason.unwrap_or_else(|_| Arc::new(abort_error()) as SharedError)),
        }
    }
    .boxed()
}

pub fn link_abort_signal(source: &AbortSignal, target: Arc<AbortController>) -> Box<dyn FnOnce() + Send> {
    if source.aborted() {
        target.abort(source.reason());
        return Box::new(|| {});
    }
    let source_reason = source.clone();
    let listener_id = source.listen(Arc::new(move || {
        target.abort(source_reason.reason());
    }));
    let source = source.clone();
    Box::new(move || source.remove_listener(listener_id))
}

fn abort_reason(signal: &AbortSignal) -> SharedError {
    if let Some(reason) = signal.reason()
        && !is_default_abort_reason(reason.as_ref())
    {
        return reason;
    }
    Arc::new(abort_error())
}

fn is_default_abort_reason(reason: &(dyn std::error::Error + 'static)) -> bool {
    reason
        .downcast_ref::<AbortError>()
        .is_some_and(|error| {
            matches!(
                error.message.as_str(),
                "This operation was aborted" | "The operation was aborted."
            )
        })
}

pub struct DeadlineAbortSignal {
    pub signal: AbortSignal,
    timed_out: Arc<AtomicBool>,
    timeout: Mutex<Option<tokio::task::JoinHandle<()>>>,
    unlink: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl DeadlineAbortSignal {
    pub fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }

    pub fn clear(&self) {
        if let Some(timeout) = self.timeout.lock().take() {
            timeout.abort();
        }
        if let Some(unlink) = self.unlink.lock().take() {
            unlink();
        }
    }
}

pub fn create_deadline_abort_signal(
    source: &AbortSignal,
    timeout_ms: i64,
) -> DeadlineAbortSignal {
    let controller = Arc::new(AbortController::new());
    let unlink = link_abort_signal(source, controller.clone());
    let timed_out = Arc::new(AtomicBool::new(false));
    let timeout_controller = controller.clone();
    let timeout_flag = timed_out.clone();
    let timeout = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(timeout_ms.max(0) as u64)).await;
        timeout_flag.store(true, Ordering::SeqCst);
        timeout_controller.abort(Some(Arc::new(abort_error())));
    });
    DeadlineAbortSignal {
        signal: controller.signal(),
        timed_out,
        timeout: Mutex::new(Some(timeout)),
        unlink: Mutex::new(Some(unlink)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dropping_a_cancelled_wait_removes_its_listener() {
        let controller = AbortController::new();
        let signal = controller.signal();
        let mut waiting = signal.cancelled();
        assert!(signal.inner.listeners.lock().is_empty());
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        assert_eq!(signal.inner.listeners.lock().len(), 1);
        drop(waiting);
        assert!(signal.inner.listeners.lock().is_empty());
    }

    #[tokio::test]
    async fn cancellation_before_the_first_poll_does_not_register_a_listener() {
        let controller = AbortController::new();
        let signal = controller.signal();
        let waiting = signal.cancelled();
        controller.abort(None);
        waiting.await;
        assert!(signal.inner.listeners.lock().is_empty());
    }

    #[tokio::test]
    async fn cancelled_resolves_when_the_signal_aborts() {
        let controller = AbortController::new();
        let signal = controller.signal();
        let observed = Arc::new(AtomicBool::new(false));
        let observed_by_listener = observed.clone();
        let task = tokio::spawn(async move {
            signal.cancelled().await;
            observed_by_listener.store(true, Ordering::SeqCst);
        });
        controller.abort(None);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("cancelled must resolve on abort")
            .unwrap();
        assert!(observed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelled_resolves_immediately_for_an_aborted_signal() {
        let controller = AbortController::new();
        controller.abort(None);
        tokio::time::timeout(Duration::from_secs(1), controller.signal().cancelled())
            .await
            .expect("an already-aborted signal resolves immediately");
    }

    #[tokio::test]
    async fn any_aborts_when_one_source_aborts() {
        let first = AbortController::new();
        let second = AbortController::new();
        let combined = AbortSignal::any(&[first.signal(), second.signal()]);
        assert!(!combined.aborted());
        second.abort(None);
        assert!(combined.aborted());
    }

    #[tokio::test]
    async fn timeout_aborts_after_the_deadline() {
        let signal = AbortSignal::timeout(10);
        assert!(!signal.aborted());
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(signal.aborted());
    }

    #[tokio::test]
    async fn cancellation_token_is_cancelled_when_the_signal_aborts() {
        let controller = AbortController::new();
        let token = controller.signal().cancellation_token();
        assert!(!token.is_cancelled());
        controller.abort(None);
        tokio::time::timeout(Duration::from_secs(1), async {
            token.cancelled().await;
        })
        .await
        .expect("the bridged token must be cancelled");
        assert!(token.is_cancelled());
    }
}
