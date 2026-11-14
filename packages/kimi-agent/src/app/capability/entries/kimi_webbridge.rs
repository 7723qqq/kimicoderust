use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::join_all;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::_base::utils::fetch_like::FetchInit;
use crate::app::plugin::plugin::{
    InstallPluginInput, SetPluginEnabledInput,
};
use crate::app::web::providers::local_fetch_url::system_fetch;
use crate::region::{ConfiguredOAuthRef, KimiRegion};
use crate::wire::record::WireAsyncError;

use super::super::host::{
    DownloadOptions, DownloadProgress, RunCommandOptions, download_to_file, run_command,
};
use super::super::types::{
    CapabilityDetectResult, CapabilityEntry, CapabilityId, CapabilityInstallOptions,
    CapabilityInstallReporter, CapabilityStep, CapabilityStepState,
};
use super::context::CapabilityEntryContext;

const PLUGIN_ID: &str = "kimi-webbridge";
const PLUGIN_ZIP_PATH: &str = "plugins/official/kimi-webbridge.zip";
const BINARY_CDN_PATH: &str = "webbridge/latest/releases";
const DEFAULT_DAEMON_BASE_URL: &str = "http://127.0.0.1:10086";
const STATUS_TIMEOUT_MS: u64 = 1_500;
const START_TIMEOUT_MS: u64 = 30_000;
const START_POLL_INTERVAL_MS: u64 = 500;
const START_POLL_ATTEMPTS: usize = 20;

#[derive(Debug, Clone, Default)]
struct DaemonStatus {
    running: Option<bool>,
    version: Option<String>,
    extension_connected: Option<bool>,
}

#[derive(Clone)]
struct SourceSkillDir {
    label: &'static str,
    path: PathBuf,
}

#[derive(Default)]
struct MigrationState {
    backup_path: Option<String>,
    error: Option<String>,
}

pub struct KimiWebbridgeEntry {
    ctx: CapabilityEntryContext,
    base_url: String,
    bin_dir: PathBuf,
    bin_path: PathBuf,
    user_source_skill_dirs: Vec<SourceSkillDir>,
    standalone_skill_backup_dir: PathBuf,
    supported: bool,
    migration: Mutex<MigrationState>,
}

impl KimiWebbridgeEntry {
    fn new(ctx: CapabilityEntryContext) -> Self {
        let base_url = ctx
            .webbridge_base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_DAEMON_BASE_URL.to_string());
        let bin_dir = Path::new(&ctx.user_home_dir)
            .join(".kimi-webbridge")
            .join("bin");
        let bin_name = if ctx.platform == "win32" {
            "kimi-webbridge.exe"
        } else {
            "kimi-webbridge"
        };
        let bin_path = bin_dir.join(bin_name);
        let user_source_skill_dirs = vec![
            SourceSkillDir {
                label: "kimi-code",
                path: Path::new(&ctx.kimi_home_dir)
                    .join("skills")
                    .join("kimi-webbridge"),
            },
            SourceSkillDir {
                label: "agents",
                path: Path::new(&ctx.user_home_dir)
                    .join(".agents")
                    .join("skills")
                    .join("kimi-webbridge"),
            },
        ];
        let standalone_skill_backup_dir = Path::new(&ctx.kimi_home_dir)
            .join("backups")
            .join("kimi-webbridge-skills");
        let supported = binary_asset_name(&ctx.platform, &ctx.arch).is_some();
        Self {
            ctx,
            base_url,
            bin_dir,
            bin_path,
            user_source_skill_dirs,
            standalone_skill_backup_dir,
            supported,
            migration: Mutex::new(MigrationState::default()),
        }
    }

    async fn fetch_daemon_status(&self) -> Option<DaemonStatus> {
        let fetch_impl = self.ctx.fetch_impl.clone().unwrap_or_else(system_fetch);
        let controller = CancellationToken::new();
        let request_controller = controller.clone();
        let url = format!("{}/status", self.base_url);
        let request = async move {
            let response = fetch_impl
                .fetch(
                    &url,
                    FetchInit {
                        signal: Some(request_controller.clone()),
                        ..Default::default()
                    },
                )
                .await
                .ok()?;
            if !response.ok {
                return None;
            }
            let raw = response.text().await.ok()?;
            let value = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
            Some(DaemonStatus {
                running: value.get("running").and_then(serde_json::Value::as_bool),
                version: value
                    .get("version")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                extension_connected: value
                    .get("extension_connected")
                    .and_then(serde_json::Value::as_bool),
            })
        };
        match tokio::time::timeout(Duration::from_millis(STATUS_TIMEOUT_MS), request).await {
            Ok(status) => status,
            Err(_) => {
                controller.cancel();
                None
            }
        }
    }

    async fn standalone_skill_dirs(&self) -> Vec<SourceSkillDir> {
        join_all(self.user_source_skill_dirs.iter().cloned().map(|entry| async move {
            if exists(&entry.path).await {
                Some(entry)
            } else {
                None
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect()
    }

    async fn migrate_standalone_skills(&self) -> Result<Option<String>, WireAsyncError> {
        let skills = self.standalone_skill_dirs().await;
        if skills.is_empty() {
            return Ok(None);
        }
        tokio::fs::create_dir_all(&self.standalone_skill_backup_dir).await?;
        let backup_root = make_temp_dir(&self.standalone_skill_backup_dir, "migration-").await?;
        for skill in skills {
            tokio::fs::rename(skill.path, backup_root.join(skill.label)).await?;
        }
        Ok(Some(backup_root.to_string_lossy().into_owned()))
    }

    async fn detect_impl(&self) -> Result<CapabilityDetectResult, WireAsyncError> {
        let mut steps = Vec::new();
        let binary_present = exists(&self.bin_path).await;
        let binary_usable = binary_present
            && (self.ctx.platform == "win32" || executable(&self.bin_path).await);
        steps.push(CapabilityStep {
            id: "daemon-binary".to_string(),
            state: if binary_usable {
                CapabilityStepState::Ok
            } else {
                CapabilityStepState::Missing
            },
            detail: (binary_present && !binary_usable).then(|| "not executable".to_string()),
            optional: None,
        });

        let daemon = self.fetch_daemon_status().await;
        let daemon_running = daemon.as_ref().and_then(|status| status.running) == Some(true);
        steps.push(CapabilityStep {
            id: "daemon".to_string(),
            state: if daemon_running {
                CapabilityStepState::Ok
            } else {
                CapabilityStepState::Missing
            },
            detail: if daemon_running {
                daemon.as_ref().and_then(|status| status.version.clone())
            } else {
                None
            },
            optional: None,
        });

        let installed = self.ctx.plugins.list_plugins().await?;
        let plugin = installed.iter().find(|plugin| plugin.id == PLUGIN_ID);
        let mcp_gap = plugin
            .filter(|plugin| plugin.enabled_mcp_server_count < plugin.mcp_server_count)
            .map(|plugin| {
                format!(
                    "mcp {}/{} enabled",
                    plugin.enabled_mcp_server_count, plugin.mcp_server_count
                )
            });
        let plugin_ok = plugin.is_some_and(|plugin| {
            plugin.enabled
                && plugin.state == crate::app::plugin::types::PluginState::Ok
                && plugin.enabled_mcp_server_count == plugin.mcp_server_count
        });
        steps.push(CapabilityStep {
            id: "skill".to_string(),
            state: if plugin_ok {
                CapabilityStepState::Ok
            } else {
                CapabilityStepState::Missing
            },
            detail: mcp_gap.or_else(|| plugin.and_then(|plugin| plugin.version.clone())),
            optional: None,
        });

        let standalone_skills = self.standalone_skill_dirs().await;
        if !standalone_skills.is_empty() {
            let migration_error = self.migration.lock().error.clone();
            steps.push(CapabilityStep {
                id: "standalone-skill-migration".to_string(),
                state: CapabilityStepState::Missing,
                detail: Some(migration_error.unwrap_or_else(|| {
                    standalone_skills
                        .iter()
                        .map(|item| item.path.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(", ")
                })),
                optional: Some(true),
            });
        } else if exists(&self.standalone_skill_backup_dir).await {
            let backup_path = self.migration.lock().backup_path.clone();
            steps.push(CapabilityStep {
                id: "standalone-skill-migration".to_string(),
                state: CapabilityStepState::Ok,
                detail: Some(
                    backup_path.unwrap_or_else(|| {
                        self.standalone_skill_backup_dir
                            .to_string_lossy()
                            .into_owned()
                    }),
                ),
                optional: Some(true),
            });
        }

        steps.push(CapabilityStep {
            id: "extension".to_string(),
            state: if daemon
                .as_ref()
                .and_then(|status| status.extension_connected)
                == Some(true)
            {
                CapabilityStepState::Ok
            } else {
                CapabilityStepState::Missing
            },
            detail: None,
            optional: Some(true),
        });
        Ok(CapabilityDetectResult {
            version: daemon.and_then(|status| status.version),
            steps,
        })
    }

    async fn wait_for_daemon(&self) -> Result<(), WireAsyncError> {
        for _ in 0..START_POLL_ATTEMPTS {
            let status = self.fetch_daemon_status().await;
            if status.and_then(|status| status.running) == Some(true) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(START_POLL_INTERVAL_MS)).await;
        }
        Err(std::io::Error::other(format!(
            "WebBridge daemon did not come up on {} — check ~/.kimi-webbridge/logs",
            self.base_url
        ))
        .into())
    }

    async fn install_impl(
        &self,
        report: CapabilityInstallReporter,
    ) -> Result<Option<String>, WireAsyncError> {
        let Some(asset) = binary_asset_name(&self.ctx.platform, &self.ctx.arch) else {
            return Err(std::io::Error::other(format!(
                "kimi-webbridge is not supported on {}/{}",
                self.ctx.platform, self.ctx.arch
            ))
            .into());
        };
        let before = self.detect_impl().await?;
        let step_states = before
            .steps
            .iter()
            .map(|step| (step.id.clone(), step.state))
            .collect::<HashMap<_, _>>();
        let ready_before = before
            .steps
            .iter()
            .filter(|step| step.optional != Some(true))
            .all(|step| step.state == CapabilityStepState::Ok);
        let standalone_skill_migration_pending = step_states
            .get("standalone-skill-migration")
            == Some(&CapabilityStepState::Missing);
        if step_states.get("daemon-binary") != Some(&CapabilityStepState::Ok) || ready_before {
            self.install_binary(report.clone(), asset).await?;
        }

        let status = self.fetch_daemon_status().await;
        if status.and_then(|status| status.running) != Some(true) {
            report("daemon".to_string(), None);
            let started = run_command(
                self.ctx.host_process.as_ref(),
                &self.bin_path.to_string_lossy(),
                &string_args(&["start"]),
                RunCommandOptions {
                    timeout_ms: Some(START_TIMEOUT_MS),
                },
            )
            .await?;
            if started.code != 0 {
                return Err(std::io::Error::other(format!(
                    "kimi-webbridge start failed: {}",
                    if !started.stderr.is_empty() {
                        started.stderr
                    } else {
                        started.stdout
                    }
                ))
                .into());
            }
            self.wait_for_daemon().await?;
        }

        report("skill".to_string(), None);
        let region = if let Some(resolve) = &self.ctx.resolve_region {
            resolve().await
        } else {
            crate::region::resolve_kimi_region(
                &ConfiguredOAuthRef::default(),
                Some(Path::new(&self.ctx.kimi_home_dir)),
            )
        };
        let summary = self
            .ctx
            .plugins
            .install_plugin(InstallPluginInput {
                source: format!("{}/{PLUGIN_ZIP_PATH}", region_cdn_base(region)),
            })
            .await?;
        if !summary.enabled {
            self.ctx
                .plugins
                .set_plugin_enabled(SetPluginEnabledInput {
                    id: PLUGIN_ID.to_string(),
                    enabled: true,
                })
                .await?;
        }

        if standalone_skill_migration_pending {
            report("standalone-skill-migration".to_string(), None);
            match self.migrate_standalone_skills().await {
                Ok(path) => {
                    let mut migration = self.migration.lock();
                    migration.backup_path = path;
                    migration.error = None;
                }
                Err(error) => {
                    self.migration.lock().error = Some(format!(
                        "Could not back up the standalone kimi-webbridge skill: {error}"
                    ));
                }
            }
        }
        let migrated = standalone_skill_migration_pending && self.migration.lock().error.is_none();
        Ok(if migrated {
            Some("user-skill-migrated".to_string())
        } else {
            None
        })
    }

    async fn install_binary(
        &self,
        report: CapabilityInstallReporter,
        asset: &str,
    ) -> Result<(), WireAsyncError> {
        report("download".to_string(), Some(0.0));
        let url = kimi_cdn_content_url(&format!("{BINARY_CDN_PATH}/{asset}"));
        let suffix = if self.ctx.platform == "win32" {
            ".exe"
        } else {
            ""
        };
        let staging = std::env::temp_dir().join(format!(
            "kimi-webbridge-{}-{}{suffix}",
            now_ms(),
            random_base36()
        ));
        let result = async {
            let progress_report = report.clone();
            download_to_file(
                &url,
                &staging,
                Some(Arc::new(move |percent| {
                    progress_report("download".to_string(), Some(percent));
                }) as DownloadProgress),
                self.ctx.fetch_impl.clone(),
                DownloadOptions::default(),
            )
            .await?;
            tokio::fs::create_dir_all(&self.bin_dir).await?;
            move_file(&staging, &self.bin_path).await?;
            if self.ctx.platform != "win32" {
                set_executable(&self.bin_path).await?;
            }
            Ok::<(), WireAsyncError>(())
        }
        .await;
        let _ = tokio::fs::remove_file(&staging).await;
        result
    }
}

#[async_trait]
impl CapabilityEntry for KimiWebbridgeEntry {
    fn id(&self) -> CapabilityId {
        CapabilityId::KimiWebbridge
    }

    fn plugin_id(&self) -> Option<&str> {
        Some(PLUGIN_ID)
    }

    fn display_name(&self) -> &str {
        "Kimi Browser Extension"
    }

    fn description(&self) -> &str {
        "Control your real browser (with your login sessions) — navigate, click, type, read pages, and screenshot any website."
    }

    fn supported(&self) -> bool {
        self.supported
    }

    async fn detect(&self) -> Result<CapabilityDetectResult, WireAsyncError> {
        self.detect_impl().await
    }

    async fn install(
        &self,
        report: CapabilityInstallReporter,
        _options: Option<CapabilityInstallOptions>,
    ) -> Result<Option<String>, WireAsyncError> {
        self.install_impl(report).await
    }
}

pub fn create_kimi_webbridge_entry(ctx: CapabilityEntryContext) -> Arc<dyn CapabilityEntry> {
    Arc::new(KimiWebbridgeEntry::new(ctx))
}

pub fn binary_asset_name(platform: &str, arch: &str) -> Option<&'static str> {
    match (platform, arch) {
        ("darwin", "arm64") => Some("kimi-webbridge-darwin-arm64"),
        ("darwin", "x64") => Some("kimi-webbridge-darwin-amd64"),
        ("linux", "arm64") => Some("kimi-webbridge-linux-arm64"),
        ("linux", "x64") => Some("kimi-webbridge-linux-amd64"),
        ("win32", "x64") => Some("kimi-webbridge-windows-amd64.exe"),
        _ => None,
    }
}

pub async fn rename_across_devices_fallback(
    from: &Path,
    to: &Path,
) -> Result<(), WireAsyncError> {
    let mut sibling = OsString::from(to.as_os_str());
    sibling.push(format!(".{}.{}.tmp", std::process::id(), now_ms()));
    let sibling = PathBuf::from(sibling);
    let result = async {
        tokio::fs::copy(from, &sibling).await?;
        replace_file(&sibling, to).await?;
        Ok::<(), WireAsyncError>(())
    }
    .await;
    let _ = tokio::fs::remove_file(&sibling).await;
    result?;
    tokio::fs::remove_file(from).await?;
    Ok(())
}

async fn move_file(from: &Path, to: &Path) -> Result<(), WireAsyncError> {
    match tokio::fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == std::io::ErrorKind::CrossesDevices
                || error.raw_os_error() == Some(17) =>
        {
            rename_across_devices_fallback(from, to).await
        }
        Err(error) if cfg!(windows) && to.exists() => {
            tokio::fs::remove_file(to).await?;
            tokio::fs::rename(from, to).await?;
            Ok(())
        }
        Err(error) => Err(Box::new(error)),
    }
}

async fn replace_file(from: &Path, to: &Path) -> Result<(), WireAsyncError> {
    if cfg!(windows) && to.exists() {
        tokio::fs::remove_file(to).await?;
    }
    tokio::fs::rename(from, to).await?;
    Ok(())
}

async fn exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

#[cfg(unix)]
async fn executable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

#[cfg(not(unix))]
async fn executable(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

#[cfg(unix)]
async fn set_executable(path: &Path) -> Result<(), WireAsyncError> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_executable(_path: &Path) -> Result<(), WireAsyncError> {
    Ok(())
}

async fn make_temp_dir(parent: &Path, prefix: &str) -> Result<PathBuf, WireAsyncError> {
    tokio::fs::create_dir_all(parent).await?;
    loop {
        let path = parent.join(format!("{prefix}{}", uuid::Uuid::new_v4()));
        match tokio::fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Box::new(error)),
        }
    }
}

fn region_cdn_base(region: KimiRegion) -> &'static str {
    match region {
        KimiRegion::MainlandCn => "https://code.kimi.com/kimi-code",
        KimiRegion::Global => "https://code.kimi.ai/kimi-code",
    }
}

fn kimi_cdn_content_url(path: &str) -> String {
    format!("https://cdn.kimi.com/{}", path.trim_start_matches('/'))
}

fn random_base36() -> String {
    (0..6)
        .map(|_| char::from_digit(fastrand::u32(0..36), 36).expect("base36 digit"))
        .collect()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn string_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}
