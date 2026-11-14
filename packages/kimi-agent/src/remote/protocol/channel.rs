use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionCloseInfo {
    pub reason: String,
    pub kind: Option<ConnectionCloseKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionCloseKind {
    Eof,
    PipeError,
    ProtocolError,
    ClientClosed,
    Liveness,
    Other,
}

#[async_trait]
pub trait ExecChannel: Send + Sync {
    async fn call(&self, method: &str, params: Option<Value>) -> Result<Value, WireAsyncError>;
    fn notify(&self, method: &str, params: Option<Value>);
    fn on_notification(
        &self,
        method: &str,
        handler: Arc<dyn Fn(Value) + Send + Sync>,
    ) -> Box<dyn FnOnce() + Send>;
    fn on_did_close(&self, listener: Arc<dyn Fn(ConnectionCloseInfo) + Send + Sync>);
}
