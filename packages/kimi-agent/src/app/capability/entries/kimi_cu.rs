use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::try_join;
use kimi_core::primitives::errors::Error2;
use regex::Regex;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::_base::utils::fetch_like::FetchLike;
use crate::app::plugin::plugin::{
    GetPluginInfoInput, InstallPluginInput, PluginSuspensionTask, SetPluginEnabledInput,
    SetPluginMcpServerEnabledInput,
};
use crate::app::plugin::types::PluginState;
use crate::wire::record::WireAsyncError;

use super::super::errors::{
    CAPABILITY_PROCESSES_RUNNING, register_capability_errors,
};
use super::super::host::{
    CommandResult, DownloadOptions, DownloadProgress, RunCommandOptions, download_to_file,
    run_command,
};
use super::super::types::{
    CapabilityDetectResult, CapabilityEntry, CapabilityId, CapabilityInstallOptions,
    CapabilityInstallReporter, CapabilityStep, CapabilityStepState,
};
use super::context::CapabilityEntryContext;
use super::windows_cu_processes::{find_windows_cu_processes, manage_windows_cu_processes};

const MAC_PLUGIN_ID: &str = "kimi-cu";
const WINDOWS_PLUGIN_ID: &str = "kimi-cu-win";
const APP_BUNDLE: &str = "KimiCU.app";
const LAUNCHD_LABEL: &str = "ai.kimi.cu.service";
const COMMAND_TIMEOUT_MS: u64 = 30_000;
const PERMISSIONS_TIMEOUT_MS: u64 = 15_000;
const DETECT_PROBE_TIMEOUT_MS: u64 = 3_000;
const WINDOWS_INSTALLER_PROBE_TIMEOUT_MS: u64 = 10_000;
const WINDOWS_INSTALL_TIMEOUT_MS: u64 = 180_000;
const DEFAULT_WINDOWS_SYSTEM_ROOT: &str = "C:\\Windows";
const DEFAULT_WINDOWS_PROGRAM_FILES: &str = "C:\\Program Files";
const WINDOWS_INSTALLER_PROBE_SCRIPT: &str = concat!(
    "$required = @('Get-FileHash', 'Expand-Archive', 'Get-AuthenticodeSignature', 'Get-CimInstance', 'Invoke-WebRequest', 'Invoke-RestMethod', 'ConvertFrom-Json', 'ConvertTo-Json'); ",
    "$missing = @($required | Where-Object { -not (Get-Command $_ -CommandType Cmdlet,Function -ErrorAction SilentlyContinue) }); ",
    "$issues = @(); ",
    "if ($PSVersionTable.PSVersion -lt [Version]'5.1') { $issues += ('requires PowerShell 5.1 or newer; found ' + $PSVersionTable.PSVersion) }; ",
    "if ($missing.Count -gt 0) { $issues += ('missing commands: ' + ($missing -join ', ')) }; ",
    "if ($issues.Count -gt 0) { [Console]::Error.Write(($issues -join '; ')); exit 2 }; ",
    "[Console]::Out.Write(('PowerShell ' + $PSVersionTable.PSVersion));"
);
const WINDOWS_DOCTOR_SCRIPT: &str = concat!(
    "$candidates = @($env:KIMI_CU_WINDOWS_EXE); ",
    "if ($env:KIMI_CU_WINDOWS_HOME) { $candidates += (Join-Path $env:KIMI_CU_WINDOWS_HOME 'kimi-cu.exe') }; ",
    "if ($env:LOCALAPPDATA) { $candidates += (Join-Path $env:LOCALAPPDATA 'KimiCU\\kimi-cu.exe') }; ",
    "if ($env:ProgramFiles) { $candidates += (Join-Path $env:ProgramFiles 'KimiCU\\kimi-cu.exe') }; ",
    "$exe = $candidates | Where-Object { -not [string]::IsNullOrWhiteSpace($_) -and (Test-Path -LiteralPath $_ -PathType Leaf) } | Select-Object -First 1; ",
    "if (-not $exe) { exit 3 }; & $exe doctor; exit $LASTEXITCODE"
);

pub fn windows_cu_exe_candidates(env: Option<&HashMap<String, String>>) -> Vec<String> {
    let process_env;
    let env = if let Some(env) = env {
        env
    } else {
        process_env = std::env::vars().collect::<HashMap<_, _>>();
        &process_env
    };
    let mut candidates = Vec::new();
    if let Some(candidate) = env.get("KIMI_CU_WINDOWS_EXE") {
        candidates.push(candidate.clone());
    }
    if let Some(home) = env.get("KIMI_CU_WINDOWS_HOME").filter(|value| !value.is_empty()) {
        candidates.push(win32_join(&[home, "kimi-cu.exe"]));
    }
    if let Some(local_app_data) = env.get("LOCALAPPDATA").filter(|value| !value.is_empty()) {
        candidates.push(win32_join(&[local_app_data, "KimiCU", "kimi-cu.exe"]));
    }
    if let Some(program_files) = env.get("ProgramFiles").filter(|value| !value.is_empty()) {
        candidates.push(win32_join(&[program_files, "KimiCU", "kimi-cu.exe"]));
    }
    candidates
        .into_iter()
        .filter(|candidate| !candidate.trim().is_empty())
        .collect()
}

pub async fn resolve_windows_cu_exe(candidates: &[String]) -> Option<String> {
    for candidate in candidates {
        if tokio::fs::metadata(candidate)
            .await
            .map(|entry| entry.is_file())
            .unwrap_or(false)
        {
            return Some(candidate.clone());
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionStatus {
    pub accessibility: bool,
    pub screen_recording: bool,
}

pub fn parse_permission_status(output: &str) -> Option<PermissionStatus> {
    let regex = Regex::new(
        r"(?:permissions|permissionStatus):\s*accessibility=(true|false)\s+screenRecording=(true|false)",
    )
    .ok()?;
    let captures = regex.captures(output)?;
    Some(PermissionStatus {
        accessibility: captures.get(1)?.as_str() == "true",
        screen_recording: captures.get(2)?.as_str() == "true",
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsDoctorOutput {
    pub version: Option<String>,
}

pub fn parse_windows_doctor_output(output: &str) -> Option<WindowsDoctorOutput> {
    let mut fields = HashMap::new();
    for line in output
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
    {
        let Some(separator) = line.find('=') else {
            continue;
        };
        if separator == 0 {
            continue;
        }
        fields.insert(
            line[..separator].trim().to_string(),
            line[separator + 1..].trim().to_string(),
        );
    }
    if fields.get("mcp").map(String::as_str) != Some("true")
        || fields.get("helper").map(String::as_str) != Some("embedded")
    {
        return None;
    }
    Some(WindowsDoctorOutput {
        version: fields.get("version").cloned(),
    })
}

pub fn windows_power_shell_path(system_root: Option<&str>) -> String {
    let configured = system_root
        .map(str::to_string)
        .or_else(|| std::env::var("SystemRoot").ok())
        .unwrap_or_else(|| DEFAULT_WINDOWS_SYSTEM_ROOT.to_string());
    let root = if win32_is_absolute(&configured) {
        configured
    } else {
        DEFAULT_WINDOWS_SYSTEM_ROOT.to_string()
    };
    win32_join(&[
        &root,
        "System32",
        "WindowsPowerShell",
        "v1.0",
        "powershell.exe",
    ])
}

pub fn windows_power_shell7_path(program_files: Option<&str>) -> String {
    let configured = program_files
        .map(str::to_string)
        .or_else(|| std::env::var("ProgramW6432").ok())
        .or_else(|| std::env::var("ProgramFiles").ok())
        .unwrap_or_else(|| DEFAULT_WINDOWS_PROGRAM_FILES.to_string());
    let root = if win32_is_absolute(&configured) {
        configured
    } else {
        DEFAULT_WINDOWS_PROGRAM_FILES.to_string()
    };
    win32_join(&[&root, "PowerShell", "7", "pwsh.exe"])
}

pub async fn read_app_bundle_version(info_plist_path: &Path) -> Option<String> {
    let xml = tokio::fs::read_to_string(info_plist_path).await.ok()?;
    let regex = Regex::new(
        r"<key>CFBundleShortVersionString</key>\s*<string>([^<]+)</string>",
    )
    .ok()?;
    Some(regex.captures(&xml)?.get(1)?.as_str().to_string())
}

pub fn elevated_swap_script(from: &str, to: &str, staged: &str) -> String {
    let backup = format!("{to}.bak");
    format!(
        "set -e; rm -rf {}; rm -rf {}; /usr/bin/ditto {} {}; if [ -e {} ]; then /bin/mv {} {}; fi; if ! /bin/mv {} {}; then if [ -e {} ]; then /bin/mv {} {}; fi; exit 1; fi; rm -rf {}",
        sh_quote(&backup),
        sh_quote(staged),
        sh_quote(from),
        sh_quote(staged),
        sh_quote(to),
        sh_quote(to),
        sh_quote(&backup),
        sh_quote(staged),
        sh_quote(to),
        sh_quote(&backup),
        sh_quote(&backup),
        sh_quote(to),
        sh_quote(&backup),
    )
}

#[derive(Clone)]
struct PluginLayerConfig {
    id: &'static str,
    zip_url: String,
}

struct PluginLayerDetection {
    step: CapabilityStep,
    version: Option<String>,
}

struct RuntimeDetection {
    step: CapabilityStep,
    version: Option<String>,
}

struct LegacyMcpFile {
    raw: String,
    value: Value,
    servers: Map<String, Value>,
}

fn mac_plugin() -> PluginLayerConfig {
    PluginLayerConfig {
        id: MAC_PLUGIN_ID,
        zip_url: kimi_cdn_content_url("kimi-computer-use/latest/kimi-cu-plugin.zip"),
    }
}

fn windows_plugin() -> PluginLayerConfig {
    PluginLayerConfig {
        id: WINDOWS_PLUGIN_ID,
        zip_url: kimi_cdn_content_url(
            "kimi-computer-use-windows/latest/kimi-cu-win-plugin.zip",
        ),
    }
}

async fn detect_plugin_layer(
    ctx: &CapabilityEntryContext,
    config: &PluginLayerConfig,
) -> Result<PluginLayerDetection, WireAsyncError> {
    let installed = ctx.plugins.list_plugins().await?;
    let plugin = installed.iter().find(|candidate| candidate.id == config.id);
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
            && plugin.state == PluginState::Ok
            && plugin.enabled_mcp_server_count == plugin.mcp_server_count
    });
    Ok(PluginLayerDetection {
        step: CapabilityStep {
            id: "plugin".to_string(),
            state: if plugin_ok {
                CapabilityStepState::Ok
            } else {
                CapabilityStepState::Missing
            },
            detail: mcp_gap.or_else(|| plugin.and_then(|plugin| plugin.version.clone())),
            optional: None,
        },
        version: plugin.and_then(|plugin| plugin.version.clone()),
    })
}

async fn install_plugin_layer(
    ctx: &CapabilityEntryContext,
    config: &PluginLayerConfig,
) -> Result<(), WireAsyncError> {
    let summary = ctx
        .plugins
        .install_plugin(InstallPluginInput {
            source: config.zip_url.clone(),
        })
        .await?;
    if !summary.enabled {
        ctx.plugins
            .set_plugin_enabled(SetPluginEnabledInput {
                id: config.id.to_string(),
                enabled: true,
            })
            .await?;
    }
    if summary.enabled_mcp_server_count >= summary.mcp_server_count {
        return Ok(());
    }
    let info = ctx
        .plugins
        .get_plugin_info(GetPluginInfoInput {
            id: config.id.to_string(),
            locale: None,
        })
        .await?;
    for server in info.mcp_servers {
        if !server.enabled {
            ctx.plugins
                .set_plugin_mcp_server_enabled(SetPluginMcpServerEnabledInput {
                    id: config.id.to_string(),
                    server: server.name,
                    enabled: true,
                })
                .await?;
        }
    }
    Ok(())
}

fn parse_legacy_mcp_file(raw: &str, app_bin: &str) -> Option<LegacyMcpFile> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let root = value.as_object()?;
    let servers = root.get("mcpServers")?.as_object()?;
    let legacy = servers.get("kimi-cu")?.as_object()?;
    if legacy.get("command")?.as_str()? != app_bin {
        return None;
    }
    if legacy.get("enabled").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let args = legacy.get("args")?.as_array()?;
    if !args.iter().all(|arg| arg.is_string()) {
        return None;
    }
    let args = args
        .iter()
        .map(|arg| arg.as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    let known_args = (args.len() == 1 && args[0] == "mcp")
        || (args.len() == 3 && args[0] == "mcp" && args[1] == "-s" && args[2] == "user");
    if !known_args {
        return None;
    }
    let known_keys = HashSet::from(["args", "command"]);
    if legacy.keys().any(|key| !known_keys.contains(key.as_str())) {
        return None;
    }
    Some(LegacyMcpFile {
        raw: raw.to_string(),
        value,
        servers: servers.clone(),
    })
}

pub struct MacKimiCuEntry {
    ctx: CapabilityEntryContext,
    applications_dir: PathBuf,
    app_path: PathBuf,
    app_bin: PathBuf,
    info_plist: PathBuf,
    probe_timeout_ms: u64,
    command_timeout_ms: u64,
    supported: bool,
    user_mcp_config_path: PathBuf,
}

impl MacKimiCuEntry {
    fn new(ctx: CapabilityEntryContext) -> Self {
        let applications_dir = ctx
            .applications_dir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Applications"));
        let app_path = applications_dir.join(APP_BUNDLE);
        let app_bin = app_path.join("Contents").join("MacOS").join("kimi-cu");
        let info_plist = app_path.join("Contents").join("Info.plist");
        let probe_timeout_ms = ctx
            .detect_probe_timeout_ms
            .unwrap_or(DETECT_PROBE_TIMEOUT_MS);
        let command_timeout_ms = ctx.command_timeout_ms.unwrap_or(COMMAND_TIMEOUT_MS);
        let supported = ctx.platform == "darwin";
        let user_mcp_config_path = Path::new(&ctx.kimi_home_dir).join("mcp.json");
        Self {
            ctx,
            applications_dir,
            app_path,
            app_bin,
            info_plist,
            probe_timeout_ms,
            command_timeout_ms,
            supported,
            user_mcp_config_path,
        }
    }

    async fn service_running(&self) -> Result<bool, WireAsyncError> {
        if !exists(&self.app_bin).await {
            return Ok(false);
        }
        let result = run_command(
            self.ctx.host_process.as_ref(),
            &self.app_bin.to_string_lossy(),
            &string_args(&["service-status"]),
            RunCommandOptions {
                timeout_ms: Some(self.probe_timeout_ms),
            },
        )
        .await?;
        Ok(Regex::new(r"status=1\b")?.is_match(&result.stdout))
    }

    async fn permission_status(&self) -> Result<Option<PermissionStatus>, WireAsyncError> {
        if !exists(&self.app_bin).await {
            return Ok(None);
        }
        let result = run_command(
            self.ctx.host_process.as_ref(),
            &self.app_bin.to_string_lossy(),
            &string_args(&["xpc-ping"]),
            RunCommandOptions {
                timeout_ms: Some(self.probe_timeout_ms),
            },
        )
        .await?;
        Ok(parse_permission_status(&result.stdout))
    }

    async fn legacy_mcp_file(&self) -> Option<LegacyMcpFile> {
        let raw = tokio::fs::read_to_string(&self.user_mcp_config_path)
            .await
            .ok()?;
        parse_legacy_mcp_file(&raw, &self.app_bin.to_string_lossy())
    }

    async fn remove_legacy_mcp_registration(
        &self,
        legacy: Option<LegacyMcpFile>,
    ) -> Result<bool, WireAsyncError> {
        let Some(legacy) = legacy else {
            return Ok(false);
        };
        let mut next_servers = legacy.servers;
        next_servers.remove("kimi-cu");
        let mut next = legacy.value.as_object().cloned().unwrap_or_default();
        next.insert(
            "mcpServers".to_string(),
            Value::Object(next_servers),
        );
        let mode = file_mode(&self.user_mcp_config_path).await?;
        let temporary = PathBuf::from(format!(
            "{}.kimi-cu-migration-{}-{}",
            self.user_mcp_config_path.to_string_lossy(),
            std::process::id(),
            now_ms()
        ));
        let result = async {
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(mode);
            }
            let mut file = options.open(&temporary).await?;
            file.write_all(
                format!("{}\n", serde_json::to_string_pretty(&Value::Object(next))?).as_bytes(),
            )
            .await?;
            drop(file);
            if tokio::fs::read_to_string(&self.user_mcp_config_path).await? != legacy.raw {
                return Ok(false);
            }
            tokio::fs::rename(&temporary, &self.user_mcp_config_path).await?;
            Ok::<bool, WireAsyncError>(true)
        }
        .await;
        let _ = tokio::fs::remove_file(&temporary).await;
        result
    }

    async fn detect_impl(&self) -> Result<CapabilityDetectResult, WireAsyncError> {
        let mut steps = Vec::new();
        let plugin = detect_plugin_layer(&self.ctx, &mac_plugin()).await?;
        steps.push(plugin.step);

        if self.legacy_mcp_file().await.is_some() {
            steps.push(CapabilityStep {
                id: "legacy-mcp".to_string(),
                state: CapabilityStepState::Missing,
                detail: Some("duplicate standalone kimi-cu MCP registration".to_string()),
                optional: Some(true),
            });
        }

        let version = read_app_bundle_version(&self.info_plist).await;
        let app_exists = exists(&self.app_bin).await;
        let app_usable = app_exists && executable(&self.app_bin).await && exists(&self.info_plist).await;
        steps.push(CapabilityStep {
            id: "app".to_string(),
            state: if app_usable {
                CapabilityStepState::Ok
            } else {
                CapabilityStepState::Missing
            },
            detail: if app_exists && !app_usable {
                Some("not executable".to_string())
            } else {
                version.clone()
            },
            optional: None,
        });

        match self.service_running().await {
            Ok(running) => steps.push(CapabilityStep {
                id: "service".to_string(),
                state: if running {
                    CapabilityStepState::Ok
                } else {
                    CapabilityStepState::Missing
                },
                detail: None,
                optional: None,
            }),
            Err(error) => steps.push(CapabilityStep {
                id: "service".to_string(),
                state: CapabilityStepState::Failed,
                detail: Some(error.to_string()),
                optional: None,
            }),
        }

        let permissions = self.permission_status().await;
        match permissions {
            Err(error) => steps.push(CapabilityStep {
                id: "permissions".to_string(),
                state: CapabilityStepState::Failed,
                detail: Some(error.to_string()),
                optional: None,
            }),
            Ok(permissions) => {
                let granted = permissions.as_ref().is_some_and(|permissions| {
                    permissions.accessibility && permissions.screen_recording
                });
                let missing = permissions.as_ref().map(|permissions| {
                    let mut missing = Vec::new();
                    if !permissions.accessibility {
                        missing.push("accessibility");
                    }
                    if !permissions.screen_recording {
                        missing.push("screenRecording");
                    }
                    missing.join(",")
                });
                steps.push(CapabilityStep {
                    id: "permissions".to_string(),
                    state: if granted {
                        CapabilityStepState::Ok
                    } else {
                        CapabilityStepState::Missing
                    },
                    detail: if granted || missing.as_deref().unwrap_or_default().is_empty() {
                        None
                    } else {
                        missing
                    },
                    optional: None,
                });
            }
        }
        Ok(CapabilityDetectResult {
            version: version.or(plugin.version),
            steps,
        })
    }

    async fn best_effort(&self, command: &str, args: &[String]) {
        let _ = run_command(
            self.ctx.host_process.as_ref(),
            command,
            args,
            RunCommandOptions {
                timeout_ms: Some(self.command_timeout_ms),
            },
        )
        .await;
    }

    async fn stop_old_processes(&self) {
        let uid = process_uid();
        if exists(&self.app_bin).await {
            self.best_effort(
                &self.app_bin.to_string_lossy(),
                &string_args(&["uninstall"]),
            )
            .await;
        }
        self.best_effort(
            "launchctl",
            &string_args(&["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")]),
        )
        .await;
        for mode in ["service", "overlay"] {
            self.best_effort(
                "pkill",
                &string_args(&[
                    "-f",
                    &format!("{APP_BUNDLE}/Contents/MacOS/kimi-cu[[:space:]]+{mode}"),
                ]),
            )
            .await;
        }
        tokio::time::sleep(Duration::from_millis(1_000)).await;
    }

    async fn leftover_staging_paths(&self) -> Vec<PathBuf> {
        let regex = Regex::new(r"^KimiCU\.app\.(new-\d+-\d+|bak(-\d+-\d+)?)$")
            .expect("valid staging regex");
        let Ok(mut entries) = tokio::fs::read_dir(&self.applications_dir).await else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if regex.is_match(&name) {
                paths.push(self.applications_dir.join(name));
            }
        }
        paths
    }

    async fn remove_leftover_staging_entries(&self) {
        for leftover in self.leftover_staging_paths().await {
            let _ = tokio::fs::remove_dir_all(leftover).await;
        }
    }

    async fn move_app_into_place(&self, unzipped_app: &Path) -> Result<(), WireAsyncError> {
        let tail = format!("{}-{}", std::process::id(), now_ms());
        let staged = PathBuf::from(format!("{}.new-{tail}", self.app_path.to_string_lossy()));
        let backup = PathBuf::from(format!("{}.bak-{tail}", self.app_path.to_string_lossy()));
        self.remove_leftover_staging_entries().await;
        let direct = run_command(
            self.ctx.host_process.as_ref(),
            "ditto",
            &vec![
                unzipped_app.to_string_lossy().into_owned(),
                staged.to_string_lossy().into_owned(),
            ],
            RunCommandOptions {
                timeout_ms: Some(self.command_timeout_ms),
            },
        )
        .await?;
        if direct.code != 0 {
            let script = apple_script_quote(&elevated_swap_script(
                &unzipped_app.to_string_lossy(),
                &self.app_path.to_string_lossy(),
                &staged.to_string_lossy(),
            ));
            let elevated = run_command(
                self.ctx.host_process.as_ref(),
                "osascript",
                &string_args(&[
                    "-e",
                    &format!("do shell script \"{script}\" with administrator privileges"),
                ]),
                RunCommandOptions {
                    timeout_ms: Some(120_000),
                },
            )
            .await?;
            if elevated.code != 0 {
                return Err(std::io::Error::other(format!(
                    "Failed to install {APP_BUNDLE} into {} (direct: {}; elevated: {})",
                    self.applications_dir.to_string_lossy(),
                    command_detail(&direct),
                    command_detail(&elevated),
                ))
                .into());
            }
            self.stop_old_processes().await;
            return Ok(());
        }
        self.stop_old_processes().await;
        let had_previous = exists(&self.app_path).await;
        if had_previous {
            tokio::fs::rename(&self.app_path, &backup).await?;
        }
        let placed = tokio::fs::rename(&staged, &self.app_path).await.is_ok();
        if !placed {
            if had_previous {
                let _ = tokio::fs::rename(&backup, &self.app_path).await;
            }
            return Err(std::io::Error::other(format!(
                "Failed to install {APP_BUNDLE} into {}: the staged bundle could not replace the installed app",
                self.applications_dir.to_string_lossy()
            ))
            .into());
        }
        let _ = tokio::fs::remove_dir_all(backup).await;
        Ok(())
    }

    async fn install_impl(
        &self,
        report: CapabilityInstallReporter,
    ) -> Result<Option<String>, WireAsyncError> {
        if !self.supported {
            return Err(std::io::Error::other(format!(
                "kimi-cu is only supported on macOS (current: {})",
                self.ctx.platform
            ))
            .into());
        }
        let before = self.detect_impl().await?;
        let legacy_mcp_before = self.legacy_mcp_file().await;
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
        report("plugin".to_string(), None);
        install_plugin_layer(&self.ctx, &mac_plugin()).await?;
        if self
            .remove_legacy_mcp_registration(legacy_mcp_before)
            .await
            .unwrap_or(false)
        {
            report("mcp-config".to_string(), None);
        }
        let install_app = step_states.get("app") != Some(&CapabilityStepState::Ok) || ready_before;
        if install_app {
            let work_dir = make_temp_dir(&std::env::temp_dir(), "kimi-cu-install-").await?;
            let result = async {
                report("download".to_string(), Some(0.0));
                let zip_path = work_dir.join("KimiCU.app.zip");
                let progress_report = report.clone();
                download_to_file(
                    &kimi_cdn_content_url("kimi-computer-use/latest/KimiCU.app.zip"),
                    &zip_path,
                    Some(Arc::new(move |percent| {
                        progress_report("download".to_string(), Some(percent));
                    }) as DownloadProgress),
                    self.ctx.fetch_impl.clone(),
                    DownloadOptions::default(),
                )
                .await?;
                report("app".to_string(), None);
                let unzip_dir = work_dir.join("unzipped");
                let unzipped = run_command(
                    self.ctx.host_process.as_ref(),
                    "ditto",
                    &string_args(&[
                        "-x",
                        "-k",
                        &zip_path.to_string_lossy(),
                        &unzip_dir.to_string_lossy(),
                    ]),
                    RunCommandOptions {
                        timeout_ms: Some(120_000),
                    },
                )
                .await?;
                if unzipped.code != 0 {
                    return Err(std::io::Error::other(format!(
                        "Failed to unzip KimiCU.app: {}",
                        if !unzipped.stderr.is_empty() {
                            unzipped.stderr
                        } else {
                            unzipped.stdout
                        }
                    ))
                    .into());
                }
                self.move_app_into_place(&unzip_dir.join(APP_BUNDLE)).await?;
                let _ = run_command(
                    self.ctx.host_process.as_ref(),
                    "xattr",
                    &string_args(&[
                        "-dr",
                        "com.apple.quarantine",
                        &self.app_path.to_string_lossy(),
                    ]),
                    RunCommandOptions {
                        timeout_ms: Some(self.command_timeout_ms),
                    },
                )
                .await?;
                Ok::<(), WireAsyncError>(())
            }
            .await;
            let _ = tokio::fs::remove_dir_all(&work_dir).await;
            result?;
        }
        if install_app || step_states.get("service") != Some(&CapabilityStepState::Ok) {
            report("service".to_string(), None);
            let registered = run_command(
                self.ctx.host_process.as_ref(),
                &self.app_bin.to_string_lossy(),
                &string_args(&["install"]),
                RunCommandOptions {
                    timeout_ms: Some(self.command_timeout_ms),
                },
            )
            .await?;
            if registered.code != 0 {
                return Err(std::io::Error::other(format!(
                    "kimi-cu install failed: {}",
                    if !registered.stderr.is_empty() {
                        registered.stderr
                    } else {
                        registered.stdout
                    }
                ))
                .into());
            }
            tokio::time::sleep(Duration::from_millis(1_000)).await;
            if !self.service_running().await.unwrap_or(false) {
                return Err(std::io::Error::other(
                    "kimi-cu background service is not running after install",
                )
                .into());
            }
        }
        if step_states.get("permissions") != Some(&CapabilityStepState::Ok) {
            report("permissions".to_string(), None);
            let _ = run_command(
                self.ctx.host_process.as_ref(),
                &self.app_bin.to_string_lossy(),
                &string_args(&["request-permissions", "--ax", "--screen"]),
                RunCommandOptions {
                    timeout_ms: Some(PERMISSIONS_TIMEOUT_MS),
                },
            )
            .await;
        }
        Ok(None)
    }
}

#[async_trait]
impl CapabilityEntry for MacKimiCuEntry {
    fn id(&self) -> CapabilityId {
        CapabilityId::KimiCu
    }

    fn plugin_id(&self) -> Option<&str> {
        Some(MAC_PLUGIN_ID)
    }

    fn display_name(&self) -> &str {
        "Kimi Computer Use"
    }

    fn description(&self) -> &str {
        "macOS GUI automation in the background — read app UIs and click, type, scroll, and drag without taking over your mouse or foregrounding apps."
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

pub struct WindowsKimiCuEntry {
    ctx: CapabilityEntryContext,
    supported: bool,
    probe_timeout_ms: u64,
    installer_probe_timeout_ms: u64,
    install_timeout_ms: u64,
    powershell_path: String,
    powershell7_path: String,
    self_weak: Weak<WindowsKimiCuEntry>,
}

impl WindowsKimiCuEntry {
    fn new(ctx: CapabilityEntryContext) -> Arc<Self> {
        Arc::new_cyclic(|weak| {
            let supported = ctx.platform == "win32" && ctx.arch == "x64";
            let probe_timeout_ms = ctx
                .detect_probe_timeout_ms
                .unwrap_or(DETECT_PROBE_TIMEOUT_MS);
            let installer_probe_timeout_ms = ctx
                .detect_probe_timeout_ms
                .unwrap_or(WINDOWS_INSTALLER_PROBE_TIMEOUT_MS);
            let install_timeout_ms = ctx
                .command_timeout_ms
                .unwrap_or(WINDOWS_INSTALL_TIMEOUT_MS);
            Self {
                ctx,
                supported,
                probe_timeout_ms,
                installer_probe_timeout_ms,
                install_timeout_ms,
                powershell_path: windows_power_shell_path(None),
                powershell7_path: windows_power_shell7_path(None),
                self_weak: weak.clone(),
            }
        })
    }

    fn this(&self) -> Arc<Self> {
        self.self_weak.upgrade().expect("Windows KimiCU entry is alive")
    }

    async fn installer_power_shell(&self) -> Result<String, WireAsyncError> {
        let mut failures = Vec::new();
        for (label, command) in [
            ("Windows PowerShell", self.powershell_path.clone()),
            ("PowerShell 7", self.powershell7_path.clone()),
        ] {
            match run_command(
                self.ctx.host_process.as_ref(),
                &command,
                &string_args(&[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    WINDOWS_INSTALLER_PROBE_SCRIPT,
                ]),
                RunCommandOptions {
                    timeout_ms: Some(self.installer_probe_timeout_ms),
                },
            )
            .await
            {
                Ok(result) if result.code == 0 => return Ok(command),
                Ok(result) => failures.push(format!(
                    "{label} ({command}): {}",
                    trimmed_command_detail(&result)
                )),
                Err(error) => failures.push(format!("{label} ({command}): {error}")),
            }
        }
        Err(std::io::Error::other(format!(
            "Kimi Computer Use requires Windows PowerShell 5.1 or PowerShell 7 with the commands required by its official installer. {}",
            failures.join("; ")
        ))
        .into())
    }

    fn runtime_step_from(result: &CommandResult) -> RuntimeDetection {
        if result.code == 3 {
            return RuntimeDetection {
                step: CapabilityStep {
                    id: "runtime".to_string(),
                    state: CapabilityStepState::Missing,
                    detail: None,
                    optional: None,
                },
                version: None,
            };
        }
        if result.code != 0 {
            return RuntimeDetection {
                step: CapabilityStep {
                    id: "runtime".to_string(),
                    state: CapabilityStepState::Failed,
                    detail: Some(trimmed_command_detail(result)),
                    optional: None,
                },
                version: None,
            };
        }
        let Some(doctor) = parse_windows_doctor_output(&result.stdout) else {
            return RuntimeDetection {
                step: CapabilityStep {
                    id: "runtime".to_string(),
                    state: CapabilityStepState::Failed,
                    detail: Some("doctor returned unexpected output".to_string()),
                    optional: None,
                },
                version: None,
            };
        };
        RuntimeDetection {
            step: CapabilityStep {
                id: "runtime".to_string(),
                state: CapabilityStepState::Ok,
                detail: doctor.version.clone(),
                optional: None,
            },
            version: doctor.version,
        }
    }

    async fn runtime_step(&self, command: &str) -> RuntimeDetection {
        match run_command(
            self.ctx.host_process.as_ref(),
            command,
            &string_args(&[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                WINDOWS_DOCTOR_SCRIPT,
            ]),
            RunCommandOptions {
                timeout_ms: Some(self.probe_timeout_ms),
            },
        )
        .await
        {
            Ok(result) => Self::runtime_step_from(&result),
            Err(error) => RuntimeDetection {
                step: CapabilityStep {
                    id: "runtime".to_string(),
                    state: CapabilityStepState::Failed,
                    detail: Some(error.to_string()),
                    optional: None,
                },
                version: None,
            },
        }
    }

    async fn direct_runtime_step(&self, exe: &str) -> Option<RuntimeDetection> {
        let result = match run_command(
            self.ctx.host_process.as_ref(),
            exe,
            &string_args(&["doctor"]),
            RunCommandOptions {
                timeout_ms: Some(self.probe_timeout_ms),
            },
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                return Some(RuntimeDetection {
                    step: CapabilityStep {
                        id: "runtime".to_string(),
                        state: CapabilityStepState::Failed,
                        detail: Some(error.to_string()),
                        optional: None,
                    },
                    version: None,
                });
            }
        };
        if result.code == -1 && result.stdout.is_empty() {
            return None;
        }
        Some(Self::runtime_step_from(&result))
    }

    async fn detect_runtime_step(&self) -> RuntimeDetection {
        let Some(exe) = resolve_windows_cu_exe(&windows_cu_exe_candidates(None)).await else {
            return RuntimeDetection {
                step: CapabilityStep {
                    id: "runtime".to_string(),
                    state: CapabilityStepState::Missing,
                    detail: None,
                    optional: None,
                },
                version: None,
            };
        };
        if let Some(direct) = self.direct_runtime_step(&exe).await {
            return direct;
        }
        let system_runtime = self.runtime_step(&self.powershell_path).await;
        if system_runtime.step.state != CapabilityStepState::Failed {
            return system_runtime;
        }
        let fallback_runtime = self.runtime_step(&self.powershell7_path).await;
        if fallback_runtime.step.state == CapabilityStepState::Ok {
            fallback_runtime
        } else {
            system_runtime
        }
    }

    async fn detect_impl(&self) -> Result<CapabilityDetectResult, WireAsyncError> {
        let (plugin, runtime) = try_join(
            detect_plugin_layer(&self.ctx, &windows_plugin()),
            async { Ok::<RuntimeDetection, WireAsyncError>(self.detect_runtime_step().await) },
        )
        .await?;
        Ok(CapabilityDetectResult {
            steps: vec![plugin.step, runtime.step],
            version: runtime.version.or(plugin.version),
        })
    }

    async fn install_inner(
        &self,
        report: CapabilityInstallReporter,
    ) -> Result<Option<String>, WireAsyncError> {
        if !self.supported {
            return Err(std::io::Error::other(format!(
                "kimi-cu is only supported on macOS or Windows x64 (current: {}/{})",
                self.ctx.platform, self.ctx.arch
            ))
            .into());
        }
        let before = self.detect_impl().await?;
        let step_states = before
            .steps
            .iter()
            .map(|step| (step.id.clone(), step.state))
            .collect::<HashMap<_, _>>();
        let ready_before = before
            .steps
            .iter()
            .all(|step| step.state == CapabilityStepState::Ok);
        let install_plugin =
            step_states.get("plugin") != Some(&CapabilityStepState::Ok) || ready_before;
        let install_runtime =
            step_states.get("runtime") != Some(&CapabilityStepState::Ok) || ready_before;
        let install_power_shell = if install_runtime {
            Some(self.installer_power_shell().await?)
        } else {
            None
        };
        if install_plugin {
            report("plugin".to_string(), None);
            if let Err(error) = install_plugin_layer(&self.ctx, &windows_plugin()).await {
                if !is_ebusy(&error) {
                    return Err(error);
                }
                return Err(std::io::Error::other(
                    "Kimi Computer Use plugin files are still in use. Close other applications using the plugin, then install again.",
                )
                .into());
            }
        }
        if let Some(power_shell) = install_power_shell {
            let work_dir = make_temp_dir(&std::env::temp_dir(), "kimi-cu-windows-install-").await?;
            let result = async {
                let setup_path = work_dir.join("setup_windows.ps1");
                let setup_url = kimi_cdn_content_url(
                    "kimi-computer-use-windows/latest/setup_windows.ps1",
                );
                report("download".to_string(), Some(0.0));
                let progress_report = report.clone();
                download_to_file(
                    &setup_url,
                    &setup_path,
                    Some(Arc::new(move |percent| {
                        progress_report("download".to_string(), Some(percent));
                    }) as DownloadProgress),
                    self.ctx.fetch_impl.clone(),
                    DownloadOptions::default(),
                )
                .await?;
                verify_downloaded_checksum(&setup_url, &setup_path, self.ctx.fetch_impl.clone())
                    .await?;
                report("runtime".to_string(), None);
                let installed = run_command(
                    self.ctx.host_process.as_ref(),
                    &power_shell,
                    &string_args(&[
                        "-NoProfile",
                        "-NonInteractive",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-Command",
                        &power_shell_setup_command(&setup_path.to_string_lossy()),
                    ]),
                    RunCommandOptions {
                        timeout_ms: Some(self.install_timeout_ms),
                    },
                )
                .await?;
                if installed.code != 0 {
                    return Err(std::io::Error::other(format!(
                        "kimi-cu Windows runtime install failed: {}",
                        trimmed_command_detail(&installed)
                    ))
                    .into());
                }
                Ok::<String, WireAsyncError>(power_shell)
            }
            .await;
            let _ = tokio::fs::remove_dir_all(&work_dir).await;
            let power_shell = result?;
            let runtime = self.runtime_step(&power_shell).await;
            if runtime.step.state != CapabilityStepState::Ok {
                return Err(std::io::Error::other(format!(
                    "kimi-cu Windows runtime is not ready after install: {}",
                    runtime.step.detail.unwrap_or_else(|| runtime.step.state.as_str().to_string())
                ))
                .into());
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl CapabilityEntry for WindowsKimiCuEntry {
    fn id(&self) -> CapabilityId {
        CapabilityId::KimiCu
    }

    fn plugin_id(&self) -> Option<&str> {
        Some(WINDOWS_PLUGIN_ID)
    }

    fn display_name(&self) -> &str {
        "Kimi Computer Use for Windows"
    }

    fn description(&self) -> &str {
        "Windows GUI automation — read app UIs and click, type, scroll, and drag in desktop apps."
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
        options: Option<CapabilityInstallOptions>,
    ) -> Result<Option<String>, WireAsyncError> {
        if !self.supported {
            return self.install_inner(report).await;
        }
        let processes = find_windows_cu_processes(
            &self.ctx,
            &[self.powershell_path.clone(), self.powershell7_path.clone()],
        )
        .await?;
        check_process_consent(processes.count, &options)?;
        let this = self.this();
        let powershell = processes.powershell.clone();
        let note = Arc::new(parking_lot::Mutex::new(None));
        let task_note = note.clone();
        let task_report = report.clone();
        let task_options = options.clone();
        let task: PluginSuspensionTask = Arc::new(move || {
            let this = this.clone();
            let powershell = powershell.clone();
            let note = task_note.clone();
            let report = task_report.clone();
            let options = task_options.clone();
            Box::pin(async move {
                let count = manage_windows_cu_processes(&this.ctx, &powershell, false).await?;
                if check_process_consent(count, &options)? > 0 {
                    report("stop-processes".to_string(), None);
                    manage_windows_cu_processes(&this.ctx, &powershell, true).await?;
                }
                let installed = this.install_inner(report).await?;
                *note.lock() = installed;
                Ok(())
            })
        });
        self.ctx
            .plugins
            .with_mcp_servers_suspended(WINDOWS_PLUGIN_ID, task)
            .await?;
        Ok(note.lock().clone())
    }
}

pub fn create_kimi_cu_entry(ctx: CapabilityEntryContext) -> Arc<dyn CapabilityEntry> {
    register_capability_errors();
    if ctx.platform == "win32" {
        WindowsKimiCuEntry::new(ctx)
    } else {
        Arc::new(MacKimiCuEntry::new(ctx))
    }
}

async fn verify_downloaded_checksum(
    url: &str,
    file_path: &Path,
    fetch_impl: Option<Arc<dyn FetchLike>>,
) -> Result<(), WireAsyncError> {
    let checksum_path = PathBuf::from(format!("{}.sha256", file_path.to_string_lossy()));
    if download_to_file(
        &format!("{url}.sha256"),
        &checksum_path,
        None,
        fetch_impl,
        DownloadOptions::default(),
    )
    .await
    .is_err()
    {
        return Ok(());
    }
    let result = async {
        let expected = tokio::fs::read_to_string(&checksum_path)
            .await?
            .trim()
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(());
        }
        let actual = sha256_file(file_path).await?;
        if expected.to_lowercase() != actual {
            return Err(std::io::Error::other(format!(
                "Checksum mismatch for {url}: expected {expected}, got {actual}"
            ))
            .into());
        }
        Ok::<(), WireAsyncError>(())
    }
    .await;
    let _ = tokio::fs::remove_file(&checksum_path).await;
    result
}

async fn sha256_file(file_path: &Path) -> Result<String, WireAsyncError> {
    let mut file = tokio::fs::File::open(file_path).await?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn check_process_consent(
    count: u64,
    options: &Option<CapabilityInstallOptions>,
) -> Result<u64, WireAsyncError> {
    if count > 0
        && options
            .as_ref()
            .and_then(|options| options.stop_running_processes)
            != Some(true)
    {
        return Err(Box::new(Error2::new(
            CAPABILITY_PROCESSES_RUNNING,
            "Kimi Computer Use processes must be stopped before installation",
        )));
    }
    Ok(count)
}

fn is_ebusy(error: &WireAsyncError) -> bool {
    error
        .downcast_ref::<Error2>()
        .is_some_and(|error| error.code().as_str() == "EBUSY")
        || error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            == Some(16)
}

fn power_shell_setup_command(setup_path: &str) -> String {
    format!(
        "$utf8 = New-Object System.Text.UTF8Encoding($false); [Console]::OutputEncoding = $utf8; $OutputEncoding = $utf8; & {}",
        power_shell_string_literal(setup_path)
    )
}

fn power_shell_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn apple_script_quote(script: &str) -> String {
    script.replace('\\', "\\\\").replace('"', "\\\"")
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn trimmed_command_detail(result: &CommandResult) -> String {
    let stderr = result.stderr.trim();
    let stdout = result.stdout.trim();
    if !stderr.is_empty() {
        stderr.to_string()
    } else if !stdout.is_empty() {
        stdout.to_string()
    } else {
        format!("exit code {}", result.code)
    }
}

fn command_detail(result: &CommandResult) -> String {
    let stderr = result.stderr.trim();
    if !stderr.is_empty() {
        stderr.to_string()
    } else {
        result.code.to_string()
    }
}

fn win32_join(parts: &[&str]) -> String {
    let mut output = String::new();
    for part in parts.iter().filter(|part| !part.is_empty()) {
        let part = part.replace('/', "\\");
        if output.is_empty() {
            output.push_str(&part);
        } else {
            if !output.ends_with('\\') {
                output.push('\\');
            }
            output.push_str(part.trim_start_matches('\\'));
        }
    }
    output
}

fn win32_is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.first().is_some_and(|byte| *byte == b'\\' || *byte == b'/')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/'))
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
fn process_uid() -> String {
    unsafe { libc::getuid() }.to_string()
}

#[cfg(not(unix))]
fn process_uid() -> String {
    "501".to_string()
}

#[cfg(unix)]
async fn file_mode(path: &Path) -> Result<u32, WireAsyncError> {
    use std::os::unix::fs::MetadataExt;
    Ok(tokio::fs::metadata(path).await?.mode() & 0o777)
}

#[cfg(not(unix))]
async fn file_mode(_path: &Path) -> Result<u32, WireAsyncError> {
    Ok(0o666)
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

fn kimi_cdn_content_url(path: &str) -> String {
    format!("https://cdn.kimi.com/{}", path.trim_start_matches('/'))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn string_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}
