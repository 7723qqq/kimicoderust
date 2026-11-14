use std::pin::Pin;
use std::sync::Arc;

use futures_util::Stream;
use futures_util::future::BoxFuture;

pub type TeardownReason<'a> = &'a str;
pub type DisposeFuture = BoxFuture<'static, Result<(), String>>;
pub type DisposerOutput = Option<DisposeFuture>;
pub type Disposer = Arc<dyn for<'a> Fn(TeardownReason<'a>) -> DisposerOutput + Send + Sync>;

pub trait IntoDisposerOutput {
    fn into_disposer_output(self) -> DisposerOutput;
}

impl IntoDisposerOutput for () {
    fn into_disposer_output(self) -> DisposerOutput {
        None
    }
}

impl IntoDisposerOutput for DisposerOutput {
    fn into_disposer_output(self) -> DisposerOutput {
        self
    }
}

impl IntoDisposerOutput for BoxFuture<'static, ()> {
    fn into_disposer_output(self) -> DisposerOutput {
        Some(Box::pin(async move {
            self.await;
            Ok(())
        }))
    }
}

impl IntoDisposerOutput for DisposeFuture {
    fn into_disposer_output(self) -> DisposerOutput {
        Some(self)
    }
}

impl IntoDisposerOutput for Option<BoxFuture<'static, ()>> {
    fn into_disposer_output(self) -> DisposerOutput {
        self.map(|future| {
            Box::pin(async move {
                future.await;
                Ok(())
            }) as DisposeFuture
        })
    }
}

pub type AsyncDisposerStream = Pin<Box<dyn Stream<Item = Option<Disposer>> + Send>>;

pub enum EffectResult {
    None,
    Disposer(Disposer),
    Future(BoxFuture<'static, Option<Disposer>>),
    Sync(Vec<Option<Disposer>>),
    Async(AsyncDisposerStream),
}

pub type EffectBody = Arc<dyn Fn() -> EffectResult + Send + Sync>;

pub fn is_promise_like(value: &EffectResult) -> bool {
    matches!(value, EffectResult::Future(_))
}

pub fn is_sync_iterable(value: &EffectResult) -> bool {
    matches!(value, EffectResult::Sync(_))
}

pub fn is_async_iterable(value: &EffectResult) -> bool {
    matches!(value, EffectResult::Async(_))
}
