use futures_util::future::BoxFuture;

use crate::wire::record::WireAsyncError;

pub trait IExtraAgentProfileLoader: Send + Sync {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;
    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;
}
