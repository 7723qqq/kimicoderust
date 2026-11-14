use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellPayloadKind {
    Bash,
    Pwsh,
}

impl ShellPayloadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Pwsh => "pwsh",
        }
    }
}

impl fmt::Display for ShellPayloadKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellPayloadArchive {
    Zip,
    SelfExtract7z,
}

#[derive(Clone, Copy, Debug)]
pub struct ShellPayloadPin {
    pub kind: ShellPayloadKind,
    pub version: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub archive: ShellPayloadArchive,
    pub exe_rel_path: &'static str,
    pub license: &'static str,
    pub source: &'static str,
}

pub const SHELL_PAYLOAD_MANIFEST: &[ShellPayloadPin] = &[
    ShellPayloadPin {
        kind: ShellPayloadKind::Bash,
        version: "2.56.0.4",
        url: "https://github.com/git-for-windows/git/releases/download/v2.56.0.windows.4/PortableGit-2.56.0.4-64-bit.7z.exe",
        sha256: "b1f3e52216e2ed6882d1bebfd4d3fdf319bec40d2edbbf9fd26ec4fd3fce7ed1",
        archive: ShellPayloadArchive::SelfExtract7z,
        exe_rel_path: "usr/bin/bash.exe",
        license: "GPL-2.0 (bash, MSYS2 runtime) — corresponding source: https://github.com/git-for-windows/git",
        source: "https://github.com/git-for-windows/git/releases/tag/v2.56.0.windows.4",
    },
    ShellPayloadPin {
        kind: ShellPayloadKind::Pwsh,
        version: "7.6.9",
        url: "https://github.com/PowerShell/PowerShell/releases/download/v7.6.9/PowerShell-7.6.9-win-x64.zip",
        sha256: "ce7484804c0f0ad4dda10c958e71acdaf7eebfb22505c1b480f89e0e9f1da94d",
        archive: ShellPayloadArchive::Zip,
        exe_rel_path: "pwsh.exe",
        license: "MIT (PowerShell) — corresponding source: https://github.com/PowerShell/PowerShell",
        source: "https://github.com/PowerShell/PowerShell/releases/tag/v7.6.9",
    },
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellPayloadStatus {
    pub kind: ShellPayloadKind,
    pub version: String,
    pub sha256: String,
    pub exe_path: String,
    pub exe_sha256: String,
    pub verified_at: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellPayloadVerifyFailure {
    Missing,
    StatusUnreadable,
    ExeMissing,
    ExeHashMismatch,
}

#[derive(Clone, Debug)]
pub enum ShellPayloadVerifyResult {
    Ok(ShellPayloadStatus),
    Failed(ShellPayloadVerifyFailure),
}

#[derive(Clone, Debug)]
pub struct ShellPayloadStatusCorruptError {
    pub path: String,
    pub cause: Option<String>,
}

impl ShellPayloadStatusCorruptError {
    pub fn new(path: impl Into<String>, cause: Option<String>) -> Self {
        Self {
            path: path.into(),
            cause,
        }
    }
}

impl fmt::Display for ShellPayloadStatusCorruptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Shell payload status file is not valid JSON: {}",
            self.path
        )
    }
}

impl std::error::Error for ShellPayloadStatusCorruptError {}

pub fn find_shell_payload_pin(kind: ShellPayloadKind) -> Option<&'static ShellPayloadPin> {
    SHELL_PAYLOAD_MANIFEST
        .iter()
        .find(|entry| entry.kind == kind)
}

pub fn shell_payload_pin_of(kind: ShellPayloadKind) -> &'static ShellPayloadPin {
    find_shell_payload_pin(kind)
        .unwrap_or_else(|| panic!("No shell payload pin registered for kind: {kind}"))
}

pub fn resolve_shell_payload_home(env: &HashMap<String, String>, os_home_dir: &str) -> String {
    if let Some(from_env) = env.get("KIMI_CODE_HOME")
        && !from_env.trim().is_empty()
    {
        return from_env.trim().to_string();
    }
    Path::new(os_home_dir)
        .join(".kimi-code")
        .to_string_lossy()
        .into_owned()
}

pub fn shell_payloads_dir(kimi_home: &str) -> String {
    Path::new(kimi_home)
        .join("shells")
        .to_string_lossy()
        .into_owned()
}

pub fn shell_payload_install_dir(kimi_home: &str, pin: &ShellPayloadPin) -> String {
    Path::new(&shell_payloads_dir(kimi_home))
        .join(format!("{}-{}", pin.kind, pin.version))
        .to_string_lossy()
        .into_owned()
}

pub fn shell_payload_status_path(kimi_home: &str, kind: ShellPayloadKind) -> String {
    Path::new(&shell_payloads_dir(kimi_home))
        .join(format!("{kind}.json"))
        .to_string_lossy()
        .into_owned()
}

pub fn shell_payload_exe_path(kimi_home: &str, pin: &ShellPayloadPin) -> String {
    Path::new(&shell_payload_install_dir(kimi_home, pin))
        .join(pin.exe_rel_path)
        .to_string_lossy()
        .into_owned()
}

pub async fn hash_file_sha256(path: &str) -> std::io::Result<String> {
    let data = tokio::fs::read(path).await?;
    Ok(format!("{:x}", Sha256::digest(&data)))
}

pub async fn path_exists(path: &str) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

pub async fn read_shell_payload_status(
    kimi_home: &str,
    kind: ShellPayloadKind,
) -> Result<Option<ShellPayloadStatus>, ShellPayloadStatusCorruptError> {
    let path = shell_payload_status_path(kimi_home, kind);
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return Ok(None);
    };
    let parsed = serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|error| ShellPayloadStatusCorruptError::new(&path, Some(error.to_string())))?;
    normalize_shell_payload_status(parsed, kind, &path).map(Some)
}

pub async fn write_shell_payload_status(
    kimi_home: &str,
    status: &ShellPayloadStatus,
) -> std::io::Result<()> {
    tokio::fs::create_dir_all(shell_payloads_dir(kimi_home)).await?;
    let path = shell_payload_status_path(kimi_home, status.kind);
    let json = serde_json::to_string_pretty(status).map_err(std::io::Error::other)?;
    tokio::fs::write(path, format!("{json}\n")).await
}

pub async fn verify_shell_payload(
    kimi_home: &str,
    kind: ShellPayloadKind,
) -> std::io::Result<ShellPayloadVerifyResult> {
    let status = match read_shell_payload_status(kimi_home, kind).await {
        Ok(Some(status)) => status,
        Ok(None) => {
            return Ok(ShellPayloadVerifyResult::Failed(
                ShellPayloadVerifyFailure::Missing,
            ));
        }
        Err(_) => {
            return Ok(ShellPayloadVerifyResult::Failed(
                ShellPayloadVerifyFailure::StatusUnreadable,
            ));
        }
    };
    if !path_exists(&status.exe_path).await {
        return Ok(ShellPayloadVerifyResult::Failed(
            ShellPayloadVerifyFailure::ExeMissing,
        ));
    }
    let exe_sha256 = hash_file_sha256(&status.exe_path).await?;
    if exe_sha256 != status.exe_sha256 {
        return Ok(ShellPayloadVerifyResult::Failed(
            ShellPayloadVerifyFailure::ExeHashMismatch,
        ));
    }
    Ok(ShellPayloadVerifyResult::Ok(status))
}

fn normalize_shell_payload_status(
    parsed: serde_json::Value,
    expected_kind: ShellPayloadKind,
    path: &str,
) -> Result<ShellPayloadStatus, ShellPayloadStatusCorruptError> {
    let corrupt = || ShellPayloadStatusCorruptError::new(path, None);
    let record = parsed.as_object().ok_or_else(corrupt.clone())?;
    let kind = record.get("kind").and_then(serde_json::Value::as_str);
    if kind != Some(expected_kind.as_str()) {
        return Err(corrupt());
    }
    let string = |key: &str| {
        record
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(corrupt.clone())
    };
    Ok(ShellPayloadStatus {
        kind: expected_kind,
        version: string("version")?,
        sha256: string("sha256")?,
        exe_path: string("exePath")?,
        exe_sha256: string("exeSha256")?,
        verified_at: string("verifiedAt")?,
    })
}
