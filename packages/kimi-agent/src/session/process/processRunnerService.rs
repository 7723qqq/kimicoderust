use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::errors::BugIndicatingError;
use crate::os::interface::host_process::{IHostProcess, IHostProcessService};
use crate::session::session_context::session_context::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::process_runner::{IProcess, ISessionProcessRunner, ProcessExecOptions, ProcessStream};

pub struct SessionProcessRunner {
    ctx: Arc<dyn ISessionContext>,
    host_process: Arc<dyn IHostProcessService>,
}

impl SessionProcessRunner {
    pub fn new(
        ctx: Arc<dyn ISessionContext>,
        host_process: Arc<dyn IHostProcessService>,
    ) -> Self {
        Self { ctx, host_process }
    }

    pub async fn exec(
        &self,
        args: &[String],
        options: Option<ProcessExecOptions>,
    ) -> Result<Arc<dyn IProcess>, WireAsyncError> {
        let Some(command) = args.first() else {
            return Err(Box::new(BugIndicatingError::new(
                "SessionProcessRunner.exec(): at least one argument (the command to run) is required.",
            )));
        };
        let options = options.unwrap_or_default();
        let cwd = options.cwd.unwrap_or_else(|| self.ctx.cwd());
        let env = build_exec_env(options.env);
        let process = self
            .host_process
            .spawn(command, &args[1..], cwd.as_deref(), env.as_ref())
            .await?;
        Ok(Arc::new(HostProcessAdapter { process }))
    }
}

#[async_trait]
impl ISessionProcessRunner for SessionProcessRunner {
    async fn exec(
        &self,
        args: &[String],
        options: Option<ProcessExecOptions>,
    ) -> Result<Arc<dyn IProcess>, WireAsyncError> {
        SessionProcessRunner::exec(self, args, options).await
    }
}

struct HostProcessAdapter {
    process: Arc<dyn IHostProcess>,
}

#[async_trait]
impl IProcess for HostProcessAdapter {
    fn pid(&self) -> u32 {
        self.process.pid()
    }

    fn exit_code(&self) -> Option<i32> {
        self.process.exit_code()
    }

    fn stdout(&self) -> ProcessStream {
        self.process.stdout()
    }

    fn stderr(&self) -> ProcessStream {
        self.process.stderr()
    }

    async fn stdin_write(&self, data: &[u8]) -> Result<(), WireAsyncError> {
        self.process.stdin_write(data).await
    }

    async fn stdin_end(&self) -> Result<(), WireAsyncError> {
        self.process.stdin_end().await
    }

    async fn wait(&self) -> Result<i32, WireAsyncError> {
        self.process.wait().await
    }

    async fn kill(&self, signal: &str) -> Result<(), WireAsyncError> {
        self.process.kill(signal).await
    }

    async fn dispose(&self) -> Result<(), WireAsyncError> {
        self.process.dispose().await
    }
}

fn build_exec_env(invocation_env: Option<HashMap<String, String>>) -> Option<HashMap<String, String>> {
    let invocation_env = invocation_env?;
    let mut env = std::env::vars().collect::<HashMap<_, _>>();
    env.extend(invocation_env);
    Some(env)
}
