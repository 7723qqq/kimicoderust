use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::StreamExt;
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use crate::_base::utils::abort::AbortSignal;
use crate::os::backends::host::host_timers_service::HostTimersService;
use crate::os::interface::host_process::{HostOutputStream, IHostProcess, IHostProcessService};
use crate::os::interface::host_timers::IHostTimers;
use crate::wire::record::WireAsyncError;

pub const DEFAULT_TIMEOUT_MS: u64 = 20_000;
pub const SIGTERM_GRACE_MS: u64 = 5_000;
pub const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;

static SYSTEM_TIMERS: Lazy<Arc<HostTimersService>> = Lazy::new(|| Arc::new(HostTimersService::new()));

fn timers_of(process_service: &dyn IHostProcessService) -> Arc<dyn IHostTimers> {
    process_service
        .timers()
        .unwrap_or_else(|| SYSTEM_TIMERS.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRgResult {
    pub exit_code: i32,
    pub stdout_text: String,
    pub stderr_text: String,
    pub buffer_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunRgOutcome {
    Result(RunRgResult),
    Aborted,
}

pub async fn run_rg_once(
    process_service: Arc<dyn IHostProcessService>,
    rg_args: &[String],
    signal: AbortSignal,
    cwd: Option<&str>,
) -> Result<RunRgOutcome, WireAsyncError> {
    if signal.aborted() {
        return Ok(RunRgOutcome::Aborted);
    }
    let timers = timers_of(process_service.as_ref());
    let Some((command, args)) = rg_args.split_first() else {
        return Err("runRgOnce: rgArgs must not be empty".into());
    };
    let proc = process_service.spawn(command, args, cwd, None).await?;
    let _ = proc.close_stdin().await;
    let timed_out = Arc::new(AtomicBool::new(false));
    let aborted = Arc::new(AtomicBool::new(false));
    let killed = Arc::new(AtomicBool::new(false));
    let kill_proc = {
        let proc = proc.clone();
        let timers = timers.clone();
        let killed = killed.clone();
        Arc::new(move || {
            let proc = proc.clone();
            let timers = timers.clone();
            let killed = killed.clone();
            async move {
                if killed.swap(true, Ordering::Relaxed) {
                    return;
                }
                let _ = proc.kill("SIGTERM").await;
                let exited = async {
                    let _ = proc.wait().await;
                };
                let (sender, receiver) = tokio::sync::oneshot::channel();
                let sender = Arc::new(Mutex::new(Some(sender)));
                let handle = timers.set_timeout(
                    Arc::new(move || {
                        if let Some(sender) = sender.lock().take() {
                            let _ = sender.send(());
                        }
                    }),
                    SIGTERM_GRACE_MS,
                );
                tokio::select! {
                    _ = exited => {}
                    _ = receiver => {
                        if proc.exit_code().is_none() {
                            let _ = proc.kill("SIGKILL").await;
                        }
                    }
                }
                timers.clear_timeout(handle);
                proc.dispose().await;
            }
        })
    };
    let on_abort = {
        let aborted = aborted.clone();
        let kill_proc = kill_proc.clone();
        Arc::new(move || {
            aborted.store(true, Ordering::Relaxed);
            let kill_proc = kill_proc.clone();
            tokio::spawn(async move {
                kill_proc().await;
            });
        })
    };
    signal.add_event_listener("abort", on_abort.clone(), false);
    if signal.aborted() {
        on_abort();
    }
    let timeout_handle = {
        let timed_out = timed_out.clone();
        let kill_proc = kill_proc.clone();
        timers.set_timeout(
            Arc::new(move || {
                timed_out.store(true, Ordering::Relaxed);
                let kill_proc = kill_proc.clone();
                tokio::spawn(async move {
                    kill_proc().await;
                });
            }),
            DEFAULT_TIMEOUT_MS,
        )
    };
    let result = async {
        let is_terminating = {
            let timed_out = timed_out.clone();
            let aborted = aborted.clone();
            let killed = killed.clone();
            Arc::new(move || {
                timed_out.load(Ordering::Relaxed)
                    || aborted.load(Ordering::Relaxed)
                    || killed.load(Ordering::Relaxed)
            })
        };
        let on_limit = {
            let kill_proc = kill_proc.clone();
            Arc::new(move || {
                let kill_proc = kill_proc.clone();
                tokio::spawn(async move {
                    kill_proc().await;
                });
            })
        };
        let stdout = read_stream_with_cap(
            proc.stdout(),
            MAX_OUTPUT_BYTES,
            Some(is_terminating.clone()),
            Some(on_limit),
        );
        let stderr = read_stream_with_cap(
            proc.stderr(),
            MAX_OUTPUT_BYTES,
            Some(is_terminating.clone()),
            None,
        );
        let wait = proc.wait();
        let (stdout, stderr, code) = tokio::join!(stdout, stderr, wait);
        let (stdout, stderr) = match (stdout, stderr) {
            (Ok(stdout), Ok(stderr)) => (stdout, stderr),
            (Err(error), _) | (_, Err(error)) => {
                if !is_premature_close_error(&error) || !is_terminating() {
                    return Err(error);
                }
                (CappedStreamResult::default(), CappedStreamResult::default())
            }
        };
        Ok::<_, WireAsyncError>((
            stdout,
            stderr,
            code.unwrap_or(-1),
        ))
    }
    .await;
    timers.clear_timeout(timeout_handle);
    signal.remove_event_listener("abort", on_abort);
    proc.dispose().await;
    if aborted.load(Ordering::Relaxed) {
        return Ok(RunRgOutcome::Aborted);
    }
    let (stdout, stderr, exit_code) = result?;
    Ok(RunRgOutcome::Result(RunRgResult {
        exit_code,
        stdout_text: stdout.text,
        stderr_text: stderr.text,
        buffer_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        timed_out: timed_out.load(Ordering::Relaxed),
    }))
}

pub fn should_retry_ripgrep_eagain(result: &RunRgResult) -> bool {
    result.exit_code != 0
        && result.exit_code != 1
        && !result.timed_out
        && is_eagain_ripgrep_error(&result.stderr_text)
}

fn is_eagain_ripgrep_error(stderr: &str) -> bool {
    stderr.contains("os error 11") || stderr.contains("Resource temporarily unavailable")
}

fn is_premature_close_error(error: &WireAsyncError) -> bool {
    let message = error.to_string();
    message.contains("ERR_STREAM_PREMATURE_CLOSE") || message.contains("premature close")
}

#[derive(Default)]
struct CappedStreamResult {
    text: String,
    truncated: bool,
}

async fn read_stream_with_cap(
    mut stream: HostOutputStream,
    max_bytes: usize,
    suppress_premature_close: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    on_limit: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<CappedStreamResult, WireAsyncError> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    loop {
        let next = match stream.next().await {
            Some(next) => next,
            None => break,
        };
        let chunk = match next {
            Ok(chunk) => chunk,
            Err(error) => {
                if is_premature_close_error(&error)
                    && suppress_premature_close.as_ref().is_some_and(|suppress| suppress())
                {
                    break;
                }
                return Err(error);
            }
        };
        if truncated {
            continue;
        }
        let chunk = chunk.into_bytes();
        if bytes.len() + chunk.len() > max_bytes {
            let remaining = max_bytes.saturating_sub(bytes.len());
            bytes.extend(&chunk[..remaining]);
            truncated = true;
            if let Some(on_limit) = &on_limit {
                on_limit();
            }
            continue;
        }
        bytes.extend(chunk);
    }
    Ok(CappedStreamResult {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
    })
}
