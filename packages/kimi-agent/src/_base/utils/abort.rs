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

pub fn is_abort_error(error: &dyn std::error::Error) -> bool {
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

pub fn is_user_cancellation(error: &dyn std::error::Error) -> bool {
    error.is::<UserCancellationError>()
}

struct AbortInner {
    aborted: AtomicBool,
    reason: Mutex<Option<SharedError>>,
    listeners: Mutex<Vec<(u64, Arc<dyn Fn() + Send + Sync>)>>,
    next_listener: AtomicU64,
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

    fn listen(&self, listener: Arc<dyn Fn() + Send + Sync>) -> u64 {
        let id = self.inner.next_listener.fetch_add(1, Ordering::Relaxed);
        self.inner.listeners.lock().push((id, listener));
        id
    }

    fn remove_listener(&self, id: u64) {
        self.inner.listeners.lock().retain(|(current, _)| *current != id);
    }
}

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
                }),
            },
        }
    }

    pub fn signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    pub fn abort(&self, reason: Option<SharedError>) {
        if self.signal.inner.aborted.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.signal.inner.reason.lock() = reason;
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

fn is_default_abort_reason(reason: &dyn std::error::Error) -> bool {
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
