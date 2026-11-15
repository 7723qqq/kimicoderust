use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::{FutureExt, StreamExt, future::{BoxFuture, Shared}};
use once_cell::sync::Lazy;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::_base::utils::abort::{AbortSignal, abortable};
use crate::environment::environment::{Environment, LOCAL_ENVIRONMENT_ID};
use crate::environment::share_bin_rg::{
    get_share_bin_rg_path, is_remote_environment, share_bin_rg_path,
};
use crate::region::{ConfiguredOAuthRef, KimiRegion, resolve_kimi_region};
use crate::wire::record::WireAsyncError;

pub use crate::environment::share_bin_rg::get_share_bin_rg_path as share_rg_path;

const RG_VERSION: &str = "15.0.0";
const DOWNLOAD_TIMEOUT_MS: u64 = 600_000;

#[derive(Debug, Clone)]
struct SharedRgError(Arc<String>);

impl SharedRgError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl std::fmt::Display for SharedRgError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0.as_str())
    }
}

impl std::error::Error for SharedRgError {}

type SharedRgResolution = Shared<BoxFuture<'static, Result<RgResolution, SharedRgError>>>;
static REMOTE_RG_RESOLUTIONS: Lazy<parking_lot::Mutex<HashMap<String, (u64, SharedRgResolution)>>> =
    Lazy::new(|| parking_lot::Mutex::new(HashMap::new()));
static REMOTE_RG_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DOWNLOAD_RG_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RgResolutionSource {
    SystemPath,
    Vendor,
    ShareBinCached,
    ShareBinDownloaded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgResolution {
    pub path: String,
    pub source: RgResolutionSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgProbeResult {
    pub exit_code: i32,
}

pub type RgProbe = Arc<
    dyn Fn(Vec<String>) -> BoxFuture<'static, Result<RgProbeResult, WireAsyncError>> + Send + Sync,
>;
pub type ResolveRegion = Arc<dyn Fn() -> Option<KimiRegion> + Send + Sync>;

#[derive(Default)]
pub struct EnsureRgPathOptions {
    pub share_dir: Option<String>,
    pub signal: Option<AbortSignal>,
    pub allow_cached_fallback: bool,
    pub environment: Option<Arc<dyn Environment>>,
    pub resolve_region: Option<ResolveRegion>,
}

pub async fn ensure_rg_path(
    probe: RgProbe,
    options: EnsureRgPathOptions,
) -> Result<RgResolution, WireAsyncError> {
    throw_if_aborted(options.signal.as_ref())?;
    let signal = options.signal.clone();
    if options.environment.as_ref().is_some_and(|environment| {
        environment.identity().environment_id != LOCAL_ENVIRONMENT_ID
    }) {
        let environment = options.environment.as_ref().unwrap().clone();
        let key = {
            let identity = environment.identity();
            format!("{} {}", identity.environment_id, identity.generation)
        };
        let (serial, resolution) = {
            let existing = REMOTE_RG_RESOLUTIONS.lock().get(&key).cloned();
            if let Some(existing) = existing {
                existing
            } else {
                let future = async move {
                    resolve_remote_rg_path(probe, environment, &options)
                        .await
                        .map_err(SharedRgError::new)
                }
                .boxed()
                .shared();
                let serial = REMOTE_RG_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                REMOTE_RG_RESOLUTIONS.lock().insert(key.clone(), (serial, future.clone()));
                tokio::spawn(future.clone());
                (serial, future)
            }
        };
        let wait = async move {
            let result = resolution.await;
            if result.is_err() {
                let mut resolutions = REMOTE_RG_RESOLUTIONS.lock();
                if resolutions.get(&key).is_some_and(|(current, _)| *current == serial) {
                    resolutions.remove(&key);
                }
            }
            result.map_err(SharedRgError::boxed)
        };
        return match signal {
            Some(signal) => abortable(wait, signal).await,
            None => wait.await,
        };
    }
    let share_dir = options.share_dir.clone().unwrap_or_else(get_share_dir);
    let resolution = resolve_rg_path(probe, share_dir, &options);
    match signal {
        Some(signal) => abortable(resolution, signal).await,
        None => resolution.await,
    }
}

async fn resolve_rg_path(
    probe: RgProbe,
    share_dir: String,
    options: &EnsureRgPathOptions,
) -> Result<RgResolution, WireAsyncError> {
    if let Some(existing) = find_existing_rg(probe.clone(), Some(share_dir.clone()), options.allow_cached_fallback).await? {
        return Ok(existing);
    }
    throw_if_aborted(options.signal.as_ref())?;
    if options.allow_cached_fallback {
        return download_rg_with_lock(probe, share_dir, options.resolve_region.clone()).await;
    }
    Err(os_unavailable("ripgrep (rg) is not available on PATH", None))
}

async fn resolve_remote_rg_path(
    probe: RgProbe,
    environment: Arc<dyn Environment>,
    options: &EnsureRgPathOptions,
) -> Result<RgResolution, WireAsyncError> {
    throw_if_aborted(options.signal.as_ref())?;
    let system = probe(vec!["rg".to_string(), "--version".to_string()])
        .await
        .unwrap_or(RgProbeResult { exit_code: -1 });
    if system.exit_code == 0 {
        return Ok(RgResolution {
            path: "rg".to_string(),
            source: RgResolutionSource::SystemPath,
        });
    }
    if !options.allow_cached_fallback {
        return Err(os_unavailable("ripgrep (rg) is not available on PATH", None));
    }
    throw_if_aborted(options.signal.as_ref())?;
    let environment_ref = Some(&environment);
    let bin_path = share_bin_rg_path(environment_ref);
    let bin_dir = environment
        .path()
        .map(|path| path.dirname(&bin_path))
        .unwrap_or_else(|| dirname(&bin_path));
    let cached = probe(vec![bin_path.clone(), "--version".to_string()])
        .await
        .unwrap_or(RgProbeResult { exit_code: -1 });
    if cached.exit_code == 0 {
        return Ok(RgResolution {
            path: bin_path,
            source: RgResolutionSource::ShareBinCached,
        });
    }
    throw_if_aborted(options.signal.as_ref())?;
    install_remote_rg(
        probe.clone(),
        environment.clone(),
        &bin_dir,
        &bin_path,
        options.resolve_region.clone(),
    )
    .await?;
    let installed = probe(vec![bin_path.clone(), "--version".to_string()])
        .await
        .unwrap_or(RgProbeResult { exit_code: -1 });
    if installed.exit_code != 0 {
        return Err(os_unavailable(
            format!("ripgrep was installed to {bin_path} but failed to run"),
            None,
        ));
    }
    Ok(RgResolution {
        path: bin_path,
        source: RgResolutionSource::ShareBinDownloaded,
    })
}

async fn install_remote_rg(
    probe: RgProbe,
    environment: Arc<dyn Environment>,
    bin_dir: &str,
    bin_path: &str,
    resolve_region: Option<ResolveRegion>,
) -> Result<(), WireAsyncError> {
    let host = environment.host().ok_or_else(|| {
        os_unavailable(
            "environment does not provide host information for the ripgrep install",
            None,
        )
    })?;
    let target = detect_remote_target(&host.os_kind, &host.os_arch).ok_or_else(|| {
        os_unavailable(
            format!(
                "Unsupported platform/arch for ripgrep download: {}/{}",
                host.os_kind, host.os_arch
            ),
            Some(serde_json::json!({ "osKind": host.os_kind, "osArch": host.os_arch })),
        )
    })?;
    let fs = environment.fs();
    let binary = download_rg_binary(&target, resolve_region).await?;
    fs.mkdir(bin_dir, true).await?;
    let staged = format!("{bin_path}.download-{}", uuid::Uuid::new_v4());
    let result = async {
        fs.write_bytes(&staged, &binary).await?;
        fs.rename(&staged, bin_path).await
    }
    .await;
    let _ = fs.remove(&staged).await;
    result?;
    if host.path_class == "win32" {
        return Ok(());
    }
    let chmod = probe(vec![
        "chmod".to_string(),
        "755".to_string(),
        bin_path.to_string(),
    ])
    .await
    .unwrap_or(RgProbeResult { exit_code: -1 });
    if chmod.exit_code != 0 {
        return Err(os_unavailable(
            format!("failed to mark {bin_path} executable on the target"),
            None,
        ));
    }
    Ok(())
}

pub async fn find_existing_rg(
    _probe: RgProbe,
    share_dir: Option<String>,
    allow_cached_fallback: bool,
) -> Result<Option<RgResolution>, WireAsyncError> {
    if let Some(system) = find_rg_on_path().await {
        return Ok(Some(RgResolution {
            path: system,
            source: RgResolutionSource::SystemPath,
        }));
    }
    if allow_cached_fallback {
        let cache_path = Path::new(&share_dir.unwrap_or_else(get_share_dir))
            .join("bin")
            .join(rg_binary_name());
        if is_executable_file(&cache_path).await {
            return Ok(Some(RgResolution {
                path: cache_path.to_string_lossy().into_owned(),
                source: RgResolutionSource::ShareBinCached,
            }));
        }
    }
    Ok(None)
}

async fn download_rg_with_lock(
    probe: RgProbe,
    share_dir: String,
    resolve_region: Option<ResolveRegion>,
) -> Result<RgResolution, WireAsyncError> {
    let _guard = DOWNLOAD_RG_LOCK.lock().await;
    if let Some(existing) = find_existing_rg(probe, Some(share_dir.clone()), true).await? {
        return Ok(existing);
    }
    let path = download_and_install_rg(&share_dir, resolve_region).await?;
    Ok(RgResolution {
        path,
        source: RgResolutionSource::ShareBinDownloaded,
    })
}

async fn find_rg_on_path() -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    for directory in path.split(if cfg!(windows) { ';' } else { ':' }) {
        if directory.is_empty() {
            continue;
        }
        let candidate = Path::new(directory).join(rg_binary_name());
        if is_executable_file(&candidate).await {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

async fn is_executable_file(path: &Path) -> bool {
    let Ok(info) = tokio::fs::metadata(path).await else {
        return false;
    };
    if !info.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        info.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn detect_target() -> Option<String> {
    let os_kind = if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(windows) {
        "Windows"
    } else {
        std::env::consts::OS
    };
    rg_target_for(os_kind, std::env::consts::ARCH)
}

pub fn detect_remote_target(os_kind: &str, os_arch: &str) -> Option<String> {
    rg_target_for(os_kind, os_arch)
}

fn rg_target_for(os_kind: &str, os_arch: &str) -> Option<String> {
    let arch = match os_arch {
        "x64" | "x86_64" => "x86_64",
        "arm64" | "aarch64" => "aarch64",
        _ => return None,
    };
    if os_kind == "macOS" {
        return Some(format!("{arch}-apple-darwin"));
    }
    if os_kind == "Linux" {
        return Some(if arch == "x86_64" {
            "x86_64-unknown-linux-musl".to_string()
        } else {
            "aarch64-unknown-linux-gnu".to_string()
        });
    }
    if os_kind == "Windows" {
        return Some(format!("{arch}-pc-windows-msvc"));
    }
    None
}

struct RgArchive {
    archive_name: String,
    expected_sha256: &'static str,
    url: String,
    is_windows: bool,
}

fn rg_archive_for_target(
    target: &str,
    resolve_region: Option<ResolveRegion>,
) -> Result<RgArchive, WireAsyncError> {
    let is_windows = target.contains("windows");
    let extension = if is_windows { "zip" } else { "tar.gz" };
    let archive_name = format!("ripgrep-{RG_VERSION}-{target}.{extension}");
    let expected_sha256 = match archive_name.as_str() {
        "ripgrep-15.0.0-aarch64-apple-darwin.tar.gz" => "98bb2e61e7277ba0ea72d2ae2592497fd8d2940934a16b122448d302a6637e3b",
        "ripgrep-15.0.0-aarch64-pc-windows-msvc.zip" => "572709c8770cb7f9385d725cb06d2bcd9537ec24d4dd17b1be1d65a876f8b591",
        "ripgrep-15.0.0-aarch64-unknown-linux-gnu.tar.gz" => "15f8cc2fab12d88491c54d49f38589922a9d6a7353c29b0a0856727bcdf80754",
        "ripgrep-15.0.0-x86_64-apple-darwin.tar.gz" => "44128c733d127ddbda461e01225a68b5f9997cfe7635242a797f645ca674a71a",
        "ripgrep-15.0.0-x86_64-pc-windows-msvc.zip" => "21a98bf42c4da97ca543c010e764cc6dec8b9b7538d05f8d21874016385e0860",
        "ripgrep-15.0.0-x86_64-unknown-linux-musl.tar.gz" => "253ad0fd5fef0d64cba56c70dccdacc1916d4ed70ad057cc525fcdb0c3bbd2a7",
        _ => {
            return Err(os_unavailable(
                format!("No pinned SHA-256 is configured for ripgrep archive {archive_name}"),
                Some(serde_json::json!({ "archiveName": archive_name })),
            ));
        }
    };
    Ok(RgArchive {
        url: format!("{}/{archive_name}", rg_base_url(resolve_region)),
        archive_name,
        expected_sha256,
        is_windows,
    })
}

fn rg_base_url(resolve_region: Option<ResolveRegion>) -> String {
    let region = resolve_region
        .and_then(|resolve| resolve())
        .unwrap_or_else(|| {
            resolve_kimi_region(
                &ConfiguredOAuthRef {
                    key: None,
                    oauth_host: None,
                },
                Some(&home_dir()),
            )
        });
    let base = match region {
        KimiRegion::MainlandCn => "https://code.kimi.com/kimi-code",
        KimiRegion::Global => "https://code.kimi.ai/kimi-code",
    };
    format!("{base}/rg")
}

async fn fetch_rg_archive(archive: &RgArchive, archive_path: &Path) -> Result<(), WireAsyncError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(DOWNLOAD_TIMEOUT_MS))
        .build()?;
    let response = client.get(&archive.url).send().await?;
    if !response.status().is_success() {
        return Err(os_unavailable(
            format!(
                "Failed to download ripgrep: HTTP {} {}",
                response.status().as_u16(),
                response.status().canonical_reason().unwrap_or("")
            ),
            Some(serde_json::json!({
                "url": archive.url,
                "status": response.status().as_u16(),
                "statusText": response.status().canonical_reason().unwrap_or(""),
            })),
        ));
    }
    let mut file = tokio::fs::File::create(archive_path).await?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        use tokio::io::AsyncWriteExt;
        file.write_all(&chunk?).await?;
    }
    file.sync_all().await?;
    verify_archive_checksum(archive_path, &archive.archive_name, archive.expected_sha256).await
}

async fn extract_rg_archive(
    archive: &RgArchive,
    target: &str,
    archive_path: &Path,
    temporary: &Path,
) -> Result<PathBuf, WireAsyncError> {
    let bin_name = if archive.is_windows { "rg.exe" } else { "rg" };
    if archive.is_windows {
        let destination = temporary.join(bin_name);
        extract_rg_from_zip(archive_path, &destination).await?;
        return Ok(destination);
    }
    let extract_dir = temporary.join("extract");
    tokio::fs::create_dir_all(&extract_dir).await?;
    let status = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive_path)
        .arg("-C")
        .arg(&extract_dir)
        .status()
        .await?;
    if !status.success() {
        return Err(os_unavailable("Failed to extract ripgrep archive", None));
    }
    let extracted = extract_dir
        .join(format!("ripgrep-{RG_VERSION}-{target}"))
        .join(bin_name);
    if !extracted.exists() {
        return Err(os_unavailable(
            format!(
                "Ripgrep archive did not contain expected binary at {}. CDN content may have changed.",
                extracted.to_string_lossy()
            ),
            Some(serde_json::json!({ "path": extracted })),
        ));
    }
    Ok(extracted)
}

async fn download_and_install_rg(
    share_dir: &str,
    resolve_region: Option<ResolveRegion>,
) -> Result<String, WireAsyncError> {
    let target = detect_target().ok_or_else(|| {
        os_unavailable(
            format!(
                "Unsupported platform/arch for ripgrep download: {}/{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
            Some(serde_json::json!({
                "platform": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            })),
        )
    })?;
    let archive = rg_archive_for_target(&target, resolve_region)?;
    let bin_dir = Path::new(share_dir).join("bin");
    tokio::fs::create_dir_all(&bin_dir).await?;
    let destination = bin_dir.join(rg_binary_name());
    let temporary = temporary_dir("kimi-rg-").await?;
    let result = async {
        let archive_path = temporary.join(&archive.archive_name);
        fetch_rg_archive(&archive, &archive_path).await?;
        let extracted = extract_rg_archive(&archive, &target, &archive_path, &temporary).await?;
        if archive.is_windows {
            tokio::fs::copy(extracted, &destination).await?;
        } else {
            let install_dir = unique_dir(&bin_dir, ".rg-install-").await?;
            let staged = install_dir.join(rg_binary_name());
            let result = async {
                tokio::fs::copy(extracted, &staged).await?;
                set_mode(&staged, 0o755).await?;
                replace_file(&staged, &destination).await
            }
            .await;
            let _ = tokio::fs::remove_dir_all(&install_dir).await;
            result?;
        }
        Ok(destination.to_string_lossy().into_owned())
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&temporary).await;
    result
}

async fn download_rg_binary(
    target: &str,
    resolve_region: Option<ResolveRegion>,
) -> Result<Vec<u8>, WireAsyncError> {
    let archive = rg_archive_for_target(target, resolve_region)?;
    let temporary = temporary_dir("kimi-rg-").await?;
    let result = async {
        let archive_path = temporary.join(&archive.archive_name);
        fetch_rg_archive(&archive, &archive_path).await?;
        let extracted = extract_rg_archive(&archive, target, &archive_path, &temporary).await?;
        Ok(tokio::fs::read(extracted).await?)
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&temporary).await;
    result
}

pub async fn verify_archive_checksum(
    archive_path: &Path,
    archive_name: &str,
    expected_sha256: &str,
) -> Result<(), WireAsyncError> {
    let archive = tokio::fs::read(archive_path).await?;
    let actual = format!("{:x}", Sha256::digest(&archive));
    if actual != expected_sha256 {
        return Err(os_unavailable(
            format!(
                "Ripgrep archive checksum mismatch for {archive_name}: expected {expected_sha256}, got {actual}. CDN content may have changed."
            ),
            Some(serde_json::json!({
                "archiveName": archive_name,
                "expectedSha256": expected_sha256,
                "actualSha256": actual,
            })),
        ));
    }
    Ok(())
}

pub async fn extract_rg_from_zip(
    archive_path: &Path,
    destination: &Path,
) -> Result<(), WireAsyncError> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut found = false;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if Path::new(entry.name()).file_name().and_then(|name| name.to_str()) != Some("rg.exe") {
            continue;
        }
        found = true;
        let staging = PathBuf::from(format!(
            "{}.partial-{}-{}",
            destination.to_string_lossy(),
            std::process::id(),
            chrono::Utc::now().timestamp_millis()
        ));
        let result = (|| {
            let mut output = std::fs::File::create(&staging)?;
            std::io::copy(&mut entry, &mut output)?;
            set_mode_sync(&staging, 0o755)?;
            if destination.is_file() {
                let _ = set_mode_sync(destination, 0o644);
            }
            replace_file_sync(&staging, destination)?;
            Ok::<(), WireAsyncError>(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        return result;
    }
    if !found {
        return Err(os_unavailable(
            "Ripgrep archive did not contain expected binary 'rg.exe'. CDN content may have changed.",
            Some(serde_json::json!({ "binary": "rg.exe" })),
        ));
    }
    Ok(())
}

pub fn rg_unavailable_message(
    cause: &WireAsyncError,
    environment: Option<&Arc<dyn Environment>>,
) -> String {
    let detail = cause.to_string();
    if is_remote_environment(environment) {
        let environment = environment.unwrap();
        let share_bin = share_bin_rg_path(environment);
        return format!(
            "ripgrep (rg) is not available on environment \"{}\" and the automatic bootstrap failed.\n\nError: {detail}\n\nFix options (install on the target):\n  macOS:   brew install ripgrep\n  Ubuntu:  sudo apt-get install ripgrep\n  Other:   https://github.com/BurntSushi/ripgrep#installation\n\nAlternatively, drop a static rg binary at {share_bin} on the target",
            environment.identity().environment_id
        );
    }
    let share_bin = get_share_bin_rg_path();
    format!(
        "ripgrep (rg) is not available and the automatic bootstrap failed.\n\nError: {detail}\n\nFix options:\n  macOS:   brew install ripgrep\n  Ubuntu:  sudo apt-get install ripgrep\n  Other:   https://github.com/BurntSushi/ripgrep#installation\n\nAlternatively, drop a static rg binary at {share_bin}"
    )
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), WireAsyncError> {
    if let Some(signal) = signal {
        signal.throw_if_aborted()?;
    }
    Ok(())
}

fn rg_binary_name() -> &'static str {
    if cfg!(windows) { "rg.exe" } else { "rg" }
}

fn get_share_dir() -> String {
    if let Some(override_dir) = std::env::var_os("KIMI_CODE_HOME")
        && !override_dir.is_empty()
    {
        return override_dir.to_string_lossy().into_owned();
    }
    home_dir().join(".kimi-code").to_string_lossy().into_owned()
}

fn home_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default()
    } else {
        std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
    }
}

fn dirname(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

async fn temporary_dir(prefix: &str) -> Result<PathBuf, WireAsyncError> {
    unique_dir(&std::env::temp_dir(), prefix).await
}

async fn unique_dir(parent: &Path, prefix: &str) -> Result<PathBuf, WireAsyncError> {
    let path = parent.join(format!("{prefix}{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&path).await?;
    Ok(path)
}

async fn set_mode(path: &Path, mode: u32) -> Result<(), WireAsyncError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn set_mode_sync(path: &Path, mode: u32) -> Result<(), WireAsyncError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

async fn replace_file(from: &Path, to: &Path) -> Result<(), WireAsyncError> {
    if cfg!(windows) && to.exists() {
        tokio::fs::remove_file(to).await?;
    }
    tokio::fs::rename(from, to).await?;
    Ok(())
}

fn replace_file_sync(from: &Path, to: &Path) -> Result<(), WireAsyncError> {
    if cfg!(windows) && to.exists() {
        std::fs::remove_file(to)?;
    }
    std::fs::rename(from, to)?;
    Ok(())
}

fn os_unavailable(message: impl Into<String>, details: Option<Value>) -> WireAsyncError {
    let details = details.and_then(|details| details.as_object().cloned());
    Box::new(kimi_core::primitives::errors::Error2::with_options(
        kimi_core::primitives::errors::domains::os_fs::OS_FS_UNAVAILABLE,
        message.into(),
        kimi_core::primitives::errors::Error2Options {
            details: Some(details),
            ..Default::default()
        },
    ))
}
