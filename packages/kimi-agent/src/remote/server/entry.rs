use std::sync::Arc;

use crate::wire::record::WireAsyncError;

use super::environment::probe_executor_environment;
use super::stdio_host::{StdioHost, StdioHostOptions};

pub struct RunExecServerOptions {
    pub version: String,
}

pub async fn run_exec_server(options: RunExecServerOptions) -> Result<i32, WireAsyncError> {
    let log: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|line| {
        eprintln!("[kimi exec-server] {line}");
    });
    let environment = probe_executor_environment().await?;
    let host = Arc::new(StdioHost::new(StdioHostOptions {
        version: options.version,
        environment,
        input: super::stdio_host::StdioInput::stdin(),
        output: super::stdio_host::StdioOutput::stdout(),
        log,
    }));
    let weak = Arc::downgrade(&host);
    let on_signal = Arc::new(move |signal: String| {
        if let Some(host) = weak.upgrade() {
            tokio::spawn(async move {
                host.shutdown(&format!("signal {signal}")).await;
            });
        }
    });
    #[cfg(unix)]
    {
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let on_signal = on_signal.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = sighup.recv() => on_signal("SIGHUP".to_string()),
                _ = sigterm.recv() => on_signal("SIGTERM".to_string()),
                _ = sigint.recv() => on_signal("SIGINT".to_string()),
            }
        });
    }
    #[cfg(not(unix))]
    let _ = on_signal;
    host.start();
    host.done().await;
    Ok(0)
}
