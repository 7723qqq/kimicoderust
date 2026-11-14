use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;

pub struct TimeoutOutcomePromise<Outcome> {
    future: Shared<BoxFuture<'static, Outcome>>,
    timeout: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl<Outcome> Clone for TimeoutOutcomePromise<Outcome> {
    fn clone(&self) -> Self {
        Self {
            future: self.future.clone(),
            timeout: self.timeout.clone(),
        }
    }
}

impl<Outcome> TimeoutOutcomePromise<Outcome> {
    pub fn clear(&self) {
        if let Some(timeout) = self.timeout.lock().take() {
            timeout.abort();
        }
    }
}

impl<Outcome> Future for TimeoutOutcomePromise<Outcome> {
    type Output = Outcome;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.future).poll(context)
    }
}

pub fn timeout_outcome<Outcome>(
    timeout_ms: Option<i64>,
    outcome: Outcome,
) -> TimeoutOutcomePromise<Outcome>
where
    Outcome: Clone + Send + Sync + 'static,
{
    let timeout = Arc::new(Mutex::new(None));
    if timeout_ms.is_none_or(|timeout_ms| timeout_ms <= 0) {
        return TimeoutOutcomePromise {
            future: std::future::pending::<Outcome>().boxed().shared(),
            timeout,
        };
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(
            (timeout_ms.unwrap() as u64).min(0x7fffffff),
        ))
        .await;
        let _ = sender.send(outcome);
    });
    *timeout.lock() = Some(task);
    let future = async move {
        match receiver.await {
            Ok(outcome) => outcome,
            Err(_) => std::future::pending::<Outcome>().await,
        }
    }
    .boxed()
    .shared();
    TimeoutOutcomePromise { future, timeout }
}
