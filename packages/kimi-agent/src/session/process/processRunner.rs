use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;

use crate::wire::record::WireAsyncError;

pub type ProcessStream = Pin<Box<dyn Stream<Item = Result<String, WireAsyncError>> + Send>>;

#[async_trait]
pub trait IProcess: Send + Sync {
    fn pid(&self) -> u32;
    fn exit_code(&self) -> Option<i32>;
    fn stdout(&self) -> ProcessStream;
    fn stderr(&self) -> ProcessStream;
    async fn stdin_write(&self, data: &[u8]) -> Result<(), WireAsyncError>;
    async fn stdin_end(&self) -> Result<(), WireAsyncError>;
    async fn wait(&self) -> Result<i32, WireAsyncError>;
    async fn kill(&self, signal: &str) -> Result<(), WireAsyncError>;
    async fn dispose(&self) -> Result<(), WireAsyncError>;
}

#[derive(Debug, Clone, Default)]
pub struct ProcessExecOptions {
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
}

#[async_trait]
pub trait ISessionProcessRunner: Send + Sync {
    fn terminal_backed(&self) -> Option<bool> {
        None
    }

    async fn exec(
        &self,
        args: &[String],
        options: Option<ProcessExecOptions>,
    ) -> Result<std::sync::Arc<dyn IProcess>, WireAsyncError>;
}
