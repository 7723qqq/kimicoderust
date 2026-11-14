use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::shell_payload::{
    ShellPayloadArchive, ShellPayloadKind, ShellPayloadPin, ShellPayloadStatus,
    hash_file_sha256, path_exists, read_shell_payload_status, shell_payload_exe_path,
    shell_payload_install_dir, shell_payload_pin_of, shell_payloads_dir,
    write_shell_payload_status,
};

#[derive(Clone, Debug)]
pub struct ShellPayloadIntegrityError {
    pub pin: &'static ShellPayloadPin,
    pub actual_sha256: String,
}

impl fmt::Display for ShellPayloadIntegrityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Shell payload integrity check failed for {} {}: expected sha256 {}, got {}",
            self.pin.kind, self.pin.version, self.pin.sha256, self.actual_sha256
        )
    }
}

impl std::error::Error for ShellPayloadIntegrityError {}

#[derive(Clone, Debug)]
pub struct ShellPayloadExtractError {
    pub pin: &'static ShellPayloadPin,
    pub detail: String,
}

impl fmt::Display for ShellPayloadExtractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Shell payload extraction failed for {} {}: {}",
            self.pin.kind, self.pin.version, self.detail
        )
    }
}

impl std::error::Error for ShellPayloadExtractError {}

#[derive(Debug)]
pub enum ShellProvisionError {
    Integrity(ShellPayloadIntegrityError),
    Extract(ShellPayloadExtractError),
    General(String),
}

impl fmt::Display for ShellProvisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integrity(error) => fmt::Display::fmt(error, formatter),
            Self::Extract(error) => fmt::Display::fmt(error, formatter),
            Self::General(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for ShellProvisionError {}

pub type Download = Arc<dyn Fn(String, String) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;
pub type Extract = Arc<
    dyn Fn(String, ShellPayloadArchive, String) -> BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct ShellProvisionDeps {
    pub download: Download,
    pub extract: Extract,
    pub now: Option<Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>>,
}

#[derive(Clone, Debug)]
pub struct ShellProvisionResult {
    pub status: ShellPayloadStatus,
    pub installed: bool,
}

pub fn apply_download_mirror(url: &str, mirror: Option<&str>) -> String {
    let Some(base) = mirror.map(str::trim).filter(|base| !base.is_empty()) else {
        return url.to_string();
    };
    format!("{}/{}", base.trim_end_matches('/'), url)
}

pub async fn provision_shell_payload(
    kimi_home: &str,
    kind: ShellPayloadKind,
    deps: &ShellProvisionDeps,
    mirror: Option<&str>,
) -> Result<ShellProvisionResult, ShellProvisionError> {
    provision_shell_payload_with_pin(shell_payload_pin_of(kind), kimi_home, deps, mirror).await
}

pub async fn provision_shell_payload_with_pin(
    pin: &'static ShellPayloadPin,
    kimi_home: &str,
    deps: &ShellProvisionDeps,
    mirror: Option<&str>,
) -> Result<ShellProvisionResult, ShellProvisionError> {
    let now = || deps.now.as_ref().map(|now| now()).unwrap_or_else(chrono::Utc::now);
    if let Some(existing) = read_existing_status(kimi_home, pin.kind).await
        && existing.version == pin.version
        && path_exists(&existing.exe_path).await
    {
        return Ok(ShellProvisionResult {
            status: existing,
            installed: false,
        });
    }
    let root = shell_payloads_dir(kimi_home);
    let tmp_dir = Path::new(&root)
        .join(format!(".tmp-{}-{}", pin.kind, now().timestamp_millis()))
        .to_string_lossy()
        .into_owned();
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .map_err(|error| ShellProvisionError::General(error.to_string()))?;
    let result = async {
        let archive_path = Path::new(&tmp_dir)
            .join(format!("payload.{}", archive_file_extension(pin.archive)))
            .to_string_lossy()
            .into_owned();
        let download = download_shell_payload_archive(pin, &archive_path, deps, mirror).await?;
        let stage_dir = Path::new(&tmp_dir)
            .join("stage")
            .to_string_lossy()
            .into_owned();
        tokio::fs::create_dir_all(&stage_dir)
            .await
            .map_err(|error| ShellProvisionError::General(error.to_string()))?;
        (deps.extract)(archive_path, pin.archive, stage_dir.clone())
            .await
            .map_err(ShellProvisionError::General)?;
        let staged_exe = Path::new(&stage_dir).join(pin.exe_rel_path);
        if !path_exists(&staged_exe.to_string_lossy()).await {
            return Err(ShellProvisionError::Extract(ShellPayloadExtractError {
                pin,
                detail: format!("expected executable not found: {}", pin.exe_rel_path),
            }));
        }
        let install_dir = shell_payload_install_dir(kimi_home, pin);
        remove_dir_all_force(&install_dir).await?;
        tokio::fs::rename(&stage_dir, &install_dir)
            .await
            .map_err(|error| ShellProvisionError::General(error.to_string()))?;
        let exe_path = shell_payload_exe_path(kimi_home, pin);
        let exe_sha256 = hash_file_sha256(&exe_path)
            .await
            .map_err(|error| ShellProvisionError::General(error.to_string()))?;
        let status = ShellPayloadStatus {
            kind: pin.kind,
            version: pin.version.to_string(),
            sha256: pin.sha256.to_string(),
            exe_path,
            exe_sha256,
            verified_at: now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        };
        write_shell_payload_status(kimi_home, &status)
            .await
            .map_err(|error| ShellProvisionError::General(error.to_string()))?;
        let _ = download;
        Ok(ShellProvisionResult {
            status,
            installed: true,
        })
    }
    .await;
    remove_dir_all_force(&tmp_dir).await?;
    result
}

#[derive(Clone, Default)]
pub struct WindowsShellProvisionDepsOptions {
    pub curl_path: Option<String>,
    pub tar_path: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ShellPayloadArchiveDownload {
    pub path: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub source: String,
}

pub fn resolve_download_candidates(url: &str, mirror: Option<&str>) -> Vec<String> {
    let mirrored = apply_download_mirror(url, mirror);
    if mirrored == url {
        vec![url.to_string()]
    } else {
        vec![mirrored, url.to_string()]
    }
}

pub async fn download_shell_payload_archive(
    pin: &'static ShellPayloadPin,
    dest_path: &str,
    deps: &ShellProvisionDeps,
    mirror: Option<&str>,
) -> Result<ShellPayloadArchiveDownload, ShellProvisionError> {
    let candidates = resolve_download_candidates(pin.url, mirror);
    let mut failures = Vec::new();
    let mut transport_failed = false;
    let mut integrity_error = None;
    for candidate in candidates {
        if let Err(error) = (deps.download)(candidate.clone(), dest_path.to_string()).await {
            transport_failed = true;
            failures.push(format!("{candidate}: {error}"));
            continue;
        }
        let sha256 = hash_file_sha256(dest_path)
            .await
            .map_err(|error| ShellProvisionError::General(error.to_string()))?;
        if sha256 == pin.sha256 {
            let size = tokio::fs::metadata(dest_path)
                .await
                .map_err(|error| ShellProvisionError::General(error.to_string()))?
                .len();
            return Ok(ShellPayloadArchiveDownload {
                path: dest_path.to_string(),
                sha256,
                size_bytes: size,
                source: candidate,
            });
        }
        integrity_error = Some(ShellPayloadIntegrityError {
            pin,
            actual_sha256: sha256.clone(),
        });
        failures.push(format!("{candidate}: sha256 mismatch (got {sha256})"));
        match tokio::fs::remove_file(dest_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ShellProvisionError::General(error.to_string())),
        }
    }
    if let Some(error) = integrity_error
        && !transport_failed
    {
        return Err(ShellProvisionError::Integrity(error));
    }
    Err(ShellProvisionError::General(format!(
        "download failed from every source:\n{}",
        failures.join("\n")
    )))
}

pub fn create_windows_shell_provision_deps(
    options: WindowsShellProvisionDepsOptions,
) -> ShellProvisionDeps {
    let curl = options
        .curl_path
        .unwrap_or_else(|| system_tool_path("curl.exe"));
    let tar = options
        .tar_path
        .unwrap_or_else(|| system_tool_path("tar.exe"));
    ShellProvisionDeps {
        download: Arc::new(move |url, dest_path| {
            let curl = curl.clone();
            async move {
                run_command(
                    &curl,
                    &[
                        "-sL".to_string(),
                        "--ssl-no-revoke".to_string(),
                        "--retry".to_string(),
                        "5".to_string(),
                        "--retry-all-errors".to_string(),
                        "--retry-delay".to_string(),
                        "2".to_string(),
                        "--max-time".to_string(),
                        "1800".to_string(),
                        "-o".to_string(),
                        dest_path,
                        url,
                    ],
                )
                .await
            }
            .boxed()
        }),
        extract: Arc::new(move |archive_path, archive, dest_dir| {
            let tar = tar.clone();
            async move {
                if archive == ShellPayloadArchive::Zip {
                    run_command(
                        &tar,
                        &[
                            "-xf".to_string(),
                            archive_path,
                            "-C".to_string(),
                            dest_dir,
                        ],
                    )
                    .await
                } else {
                    run_command(&archive_path, &[format!("-o{dest_dir}"), "-y".to_string()]).await
                }
            }
            .boxed()
        }),
        now: None,
    }
}

async fn read_existing_status(kimi_home: &str, kind: ShellPayloadKind) -> Option<ShellPayloadStatus> {
    read_shell_payload_status(kimi_home, kind).await.ok().flatten()
}

fn archive_file_extension(archive: ShellPayloadArchive) -> &'static str {
    match archive {
        ShellPayloadArchive::Zip => "zip",
        ShellPayloadArchive::SelfExtract7z => "7z.exe",
    }
}

fn system_tool_path(name: &str) -> String {
    let root = std::env::var("SystemRoot")
        .ok()
        .map(|root| root.trim().to_string())
        .filter(|root| !root.is_empty())
        .unwrap_or_else(|| "C:\\Windows".to_string());
    format!("{root}\\System32\\{name}")
}

async fn run_command(file: &str, args: &[String]) -> Result<(), String> {
    let mut command = tokio::process::Command::new(file);
    command
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let execution = async {
        let (_stdout, stderr, status) = tokio::try_join!(
            read_capped(stdout, file),
            read_capped(stderr, file),
            async { child.wait().await.map_err(|error| error.to_string()) },
        )?;
        let stderr = String::from_utf8_lossy(&stderr);
        if !status.success() {
            return Err(format!(
                "{file} failed: {}{}",
                status,
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!("\n{stderr}")
                }
            ));
        }
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(20 * 60), execution)
        .await
        .map_err(|_| format!("{file} failed: timed out"))?
}

async fn read_capped<R>(mut reader: R, file: &str) -> Result<Vec<u8>, String>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await.map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(output);
        }
        if output.len() + read > 10 * 1024 * 1024 {
            return Err(format!("{file} failed: maxBuffer exceeded"));
        }
        output.extend_from_slice(&chunk[..read]);
    }
}

async fn remove_dir_all_force(path: &str) -> Result<(), ShellProvisionError> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ShellProvisionError::General(error.to_string())),
    }
}
