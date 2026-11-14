use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::Stream;
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde_json::Value;

use crate::os::interface::host_timers::IHostTimers;
use crate::wire::record::WireAsyncError;

pub const OS_PROCESS_SPAWN_FAILED: &str = "os.process.spawn_failed";
pub const OS_PROCESS_KILL_FAILED: &str = "os.process.kill_failed";
pub const SHELL_GIT_BASH_NOT_FOUND: &str = "shell.git_bash_not_found";

pub type HostProcessErrorCode = &'static str;

#[derive(Debug)]
pub struct HostProcessError {
    inner: Error2,
}

impl HostProcessError {
    pub fn new(
        code: HostProcessErrorCode,
        message: impl Into<String>,
        details: Option<Value>,
    ) -> Self {
        Self::with_options(
            code,
            message,
            Error2Options {
                details: details.and_then(|details| details.as_object().cloned()),
                ..Default::default()
            },
        )
    }

    pub fn with_cause(
        code: HostProcessErrorCode,
        message: impl Into<String>,
        details: Option<Value>,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::with_options(
            code,
            message,
            Error2Options {
                details: details.and_then(|details| details.as_object().cloned()),
                cause: Some(Box::new(cause)),
                name: None,
            },
        )
    }

    pub fn with_options(
        code: HostProcessErrorCode,
        message: impl Into<String>,
        mut options: Error2Options,
    ) -> Self {
        options.name = Some("HostProcessError".to_string());
        Self {
            inner: Error2::with_options(code, message, options),
        }
    }
}

impl Deref for HostProcessError {
    type Target = Error2;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl fmt::Display for HostProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl std::error::Error for HostProcessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostProcessShell {
    Bool(bool),
    Path(String),
}

#[derive(Clone, Default)]
pub struct HostProcessOptions {
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub shell: Option<HostProcessShell>,
    pub detached: Option<bool>,
    pub windows_hide: Option<bool>,
    pub merge_stderr: Option<bool>,
    pub timeout: Option<u64>,
}

pub type HostOutputStream = Pin<Box<dyn Stream<Item = Result<String, WireAsyncError>> + Send>>;

#[async_trait]
pub trait IHostProcess: Send + Sync {
    fn pid(&self) -> u32;
    fn exit_code(&self) -> Option<i32>;
    fn stdout(&self) -> HostOutputStream;
    fn stderr(&self) -> HostOutputStream;
    async fn write_stdin(&self, data: &[u8]) -> Result<(), WireAsyncError>;
    async fn close_stdin(&self) -> Result<(), WireAsyncError>;
    async fn wait(&self) -> Result<i32, WireAsyncError>;
    async fn kill(&self, signal: &str) -> Result<(), WireAsyncError>;
    async fn dispose(&self);
}

#[async_trait]
pub trait IHostProcessService: Send + Sync {
    async fn spawn(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: Option<&HashMap<String, String>>,
    ) -> Result<Arc<dyn IHostProcess>, WireAsyncError>;

    fn timers(&self) -> Option<Arc<dyn IHostTimers>> {
        None
    }
}

impl<T> crate::environment::environment::EnvironmentResource for T
where
    T: IHostProcess + ?Sized,
{
    fn dispose(&self) -> futures_util::future::BoxFuture<'static, ()> {
        Box::pin(IHostProcess::dispose(self))
    }
}
