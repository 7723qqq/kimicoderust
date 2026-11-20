use std::collections::HashMap;

use futures_util::{Stream, StreamExt};

use crate::_base::utils::abort::AbortSignal;
use crate::os::interface::host_process::IHostProcessService;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Default)]
pub struct RunCommandOptions {
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub signal: Option<AbortSignal>,
}

pub async fn run_command(
    runner: &dyn IHostProcessService,
    args: &[String],
    options: RunCommandOptions,
) -> Result<RunResult, WireAsyncError> {
    let Some(command) = args.first() else {
        return Err("runCommand requires a command".into());
    };
    let proc = runner
        .spawn(command, &args[1..], options.cwd.as_deref(), options.env.as_ref())
        .await?;
    let on_abort: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new({
        let proc = proc.clone();
        move || {
            let proc = proc.clone();
            tokio::spawn(async move {
                let _ = proc.kill("SIGKILL").await;
            });
        }
    });
    if let Some(signal) = &options.signal {
        if signal.aborted() {
            on_abort();
        } else {
            signal.add_event_listener("abort", on_abort.clone(), true);
        }
    }
    let result = async {
        let (stdout, stderr, exit_code) = tokio::join!(
            read_stream(proc.stdout()),
            read_stream(proc.stderr()),
            async { proc.wait().await.unwrap_or(-1) }
        );
        Ok(RunResult {
            exit_code,
            stdout: stdout?,
            stderr: stderr?,
        })
    }
    .await;
    if let Some(signal) = &options.signal {
        signal.remove_event_listener("abort", on_abort);
    }
    result
}

pub async fn read_stream(
    stream: impl Stream<Item = Result<String, WireAsyncError>>,
) -> Result<String, WireAsyncError> {
    futures_util::pin_mut!(stream);
    let mut data = String::new();
    while let Some(chunk) = stream.next().await {
        data.push_str(&chunk?);
    }
    Ok(data)
}
