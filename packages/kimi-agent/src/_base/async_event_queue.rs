use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::future::{BoxFuture, FutureExt};
use futures_util::Stream;
use parking_lot::Mutex;
use tokio::sync::oneshot;

struct QueueState<T> {
    values: VecDeque<T>,
    waiters: VecDeque<oneshot::Sender<Result<Option<T>, String>>>,
    error: Option<String>,
    failed: bool,
    ended: bool,
}

pub struct AsyncEventQueue<T>
where
    T: Send + 'static,
{
    state: Arc<Mutex<QueueState<T>>>,
    pending_next: Option<BoxFuture<'static, Result<Option<T>, String>>>,
}

impl<T> AsyncEventQueue<T>
where
    T: Send + 'static,
{
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(QueueState {
                values: VecDeque::new(),
                waiters: VecDeque::new(),
                error: None,
                failed: false,
                ended: false,
            })),
            pending_next: None,
        }
    }

    pub fn push(&self, mut value: T) {
        let mut state = self.state.lock();
        if state.failed || state.ended {
            return;
        }
        loop {
            let Some(waiter) = state.waiters.pop_front() else {
                state.values.push_back(value);
                return;
            };
            match waiter.send(Ok(Some(value))) {
                Ok(()) => return,
                Err(Ok(Some(returned))) => value = returned,
                Err(_) => return,
            }
        }
    }

    pub fn end(&self) {
        let mut state = self.state.lock();
        if state.failed || state.ended {
            return;
        }
        state.ended = true;
        for waiter in state.waiters.drain(..) {
            let _ = waiter.send(Ok(None));
        }
    }

    pub fn fail(&self, error: impl ToString) {
        let error = error.to_string();
        let mut state = self.state.lock();
        if state.failed || state.ended {
            return;
        }
        state.error = Some(error.clone());
        state.failed = true;
        if !state.values.is_empty() {
            return;
        }
        for waiter in state.waiters.drain(..) {
            let _ = waiter.send(Err(error.clone()));
        }
    }

    pub fn next(&self) -> BoxFuture<'static, Result<Option<T>, String>> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.state.lock();
            if let Some(value) = state.values.pop_front() {
                let _ = sender.send(Ok(Some(value)));
            } else if state.failed {
                let _ = sender.send(Err(state.error.clone().unwrap_or_default()));
            } else if state.ended {
                let _ = sender.send(Ok(None));
            } else {
                state.waiters.push_back(sender);
            }
        }
        async move {
            match receiver.await {
                Ok(result) => result,
                Err(_) => std::future::pending().await,
            }
        }
        .boxed()
    }
}

impl<T> Default for AsyncEventQueue<T>
where
    T: Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Stream for AsyncEventQueue<T>
where
    T: Send + 'static,
{
    type Item = Result<T, String>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if self.pending_next.is_none() {
            self.pending_next = Some(self.next());
        }
        let future = self.pending_next.as_mut().unwrap();
        match future.as_mut().poll(context) {
            Poll::Ready(result) => {
                self.pending_next = None;
                Poll::Ready(match result {
                    Ok(Some(value)) => Some(Ok(value)),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                })
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
