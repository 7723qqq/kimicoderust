use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::wire::migration::migration::WIRE_PROTOCOL_VERSION;

use super::session_export::{ExportSessionManifest, ShellEnvironment};
use super::wire_scan::SessionWireScan;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSessionManifestSummary {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_dir: Option<String>,
}

pub struct BuildExportManifestArgs {
    pub summary: ExportSessionManifestSummary,
    pub now: DateTime<Utc>,
    pub version: String,
    pub wire_protocol_version: Option<String>,
    pub session_scan: SessionWireScan,
    pub session_log_path: Option<String>,
    pub global_log_path: Option<String>,
    pub web_log_path: Option<String>,
    pub desktop_version: Option<String>,
    pub install_source: Option<String>,
    pub shell_env: Option<ShellEnvironment>,
}

pub fn build_export_manifest(args: BuildExportManifestArgs) -> ExportSessionManifest {
    ExportSessionManifest {
        session_id: args.summary.id,
        exported_at: iso_date(args.now),
        kimi_code_version: args.version,
        wire_protocol_version: args
            .wire_protocol_version
            .unwrap_or_else(|| WIRE_PROTOCOL_VERSION.to_string()),
        os: format!("{} {}", node_platform(), node_arch()),
        nodejs_version: nodejs_version(),
        session_first_activity: args.session_scan.first_activity_ms.map(iso_timestamp),
        session_last_activity: args.session_scan.last_activity_ms.map(iso_timestamp),
        title: args.summary.title,
        workspace_dir: args.summary.workspace_dir,
        session_log_path: args.session_log_path,
        global_log_path: args.global_log_path,
        web_log_path: args.web_log_path,
        desktop_version: args.desktop_version,
        install_source: args.install_source,
        shell_env: args.shell_env,
    }
}

fn iso_date(date: DateTime<Utc>) -> String {
    date.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn iso_timestamp(value: i64) -> String {
    DateTime::from_timestamp_millis(value)
        .expect("session activity timestamp is outside the supported date range")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        platform => platform,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        architecture => architecture,
    }
}

fn nodejs_version() -> String {
    let version = std::env::var("NODE_VERSION")
        .or_else(|_| std::env::var("npm_config_node_version"))
        .unwrap_or_default();
    version.trim_start_matches('v').to_string()
}
