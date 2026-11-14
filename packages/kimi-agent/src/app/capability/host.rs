use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::_base::utils::fetch_like::FetchInit;
pub use crate::_base::utils::fetch_like::FetchLike;
use crate::app::web::providers::local_fetch_url::system_fetch;
use crate::os::interface::host_process::{HostOutputStream, IHostProcessService};
use crate::wire::record::WireAsyncError;

const DOWNLOAD_IDLE_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunCommandOptions {
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DownloadOptions {
    pub idle_timeout_ms: Option<u64>,
}

pub type DownloadProgress = Arc<dyn Fn(f64) + Send + Sync>;

async fn collect(mut stream: HostOutputStream) -> Result<String, WireAsyncError> {
    let mut output = String::new();
    while let Some(chunk) = stream.next().await {
        output.push_str(&chunk?);
    }
    Ok(output)
}

pub async fn run_command(
    host_process: &dyn IHostProcessService,
    command: &str,
    args: &[String],
    options: RunCommandOptions,
) -> Result<CommandResult, WireAsyncError> {
    let proc = match host_process.spawn(command, args, None, None).await {
        Ok(proc) => proc,
        Err(error) => {
            return Ok(CommandResult {
                code: -1,
                stdout: String::new(),
                stderr: error.to_string(),
            });
        }
    };
    let work = async {
        let (stdout, stderr, code) = tokio::try_join!(
            collect(proc.stdout()),
            collect(proc.stderr()),
            async { Ok::<i32, WireAsyncError>(proc.wait().await.unwrap_or(-1)) }
        )?;
        Ok::<CommandResult, WireAsyncError>(CommandResult {
            code,
            stdout,
            stderr,
        })
    };
    let result = if let Some(timeout_ms) = options.timeout_ms {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), work).await {
            Ok(result) => result,
            Err(_) => {
                let proc = proc.clone();
                tokio::spawn(async move {
                    let _ = proc.kill("SIGTERM").await;
                });
                Err(std::io::Error::other(format!(
                    "command timed out after {timeout_ms}ms: {command}"
                ))
                .into())
            }
        }
    } else {
        work.await
    };
    proc.dispose().await;
    result
}

pub async fn download_to_file(
    url: &str,
    dest_path: &Path,
    on_percent: Option<DownloadProgress>,
    fetch_impl: Option<Arc<dyn FetchLike>>,
    options: DownloadOptions,
) -> Result<u64, WireAsyncError> {
    let idle_timeout_ms = options
        .idle_timeout_ms
        .unwrap_or(DOWNLOAD_IDLE_TIMEOUT_MS);
    let idle_timeout = Duration::from_millis(idle_timeout_ms);
    let fetch_impl = fetch_impl.unwrap_or_else(system_fetch);
    let header_controller = CancellationToken::new();
    let response = tokio::time::timeout(
        idle_timeout,
        fetch_impl.fetch(
            url,
            FetchInit {
                signal: Some(header_controller.clone()),
                ..Default::default()
            },
        ),
    )
    .await;
    let response = match response {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            header_controller.cancel();
            return Err(std::io::Error::other(format!(
                "Failed to download {url}: no response within {idle_timeout_ms}ms"
            ))
            .into());
        }
    };
    let status = response.status;
    let total = response
        .header("content-length")
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0);
    let stream = if response.ok {
        response.stream()
    } else {
        None
    };
    let Some(mut stream) = stream else {
        return Err(std::io::Error::other(format!(
            "Failed to download {url}: HTTP {status}"
        ))
        .into());
    };
    let parent = destination_parent(dest_path);
    tokio::fs::create_dir_all(parent).await?;
    let mut file = tokio::fs::File::create(dest_path).await?;
    let mut received = 0_u64;
    loop {
        let next = tokio::time::timeout(idle_timeout, stream.next())
            .await
            .map_err(|_| {
                std::io::Error::other(format!(
                    "Download stalled for {idle_timeout_ms}ms: {url}"
                ))
            })?;
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk?;
        received += chunk.len() as u64;
        if total > 0.0 && let Some(report) = &on_percent {
            report(((received as f64 / total) * 100.0).floor().min(99.0));
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    if let Some(report) = on_percent {
        report(100.0);
    }
    Ok(received)
}

fn destination_parent(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}
