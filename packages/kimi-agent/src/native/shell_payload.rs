use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "bash" => Some(Self::Bash),
            "pwsh" => Some(Self::Pwsh),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellPayloadArchive {
    Zip,
    SelfExtract7z,
}

impl ShellPayloadArchive {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::SelfExtract7z => "self-extract-7z",
        }
    }
}

#[derive(Debug, Clone, Copy)]
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

pub static SHELL_PAYLOAD_MANIFEST: &[ShellPayloadPin] = &[
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellPayloadStatus {
    pub kind: ShellPayloadKind,
    pub version: String,
    pub sha256: String,
    pub exe_path: String,
    pub exe_sha256: String,
    pub verified_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellPayloadVerifyResult {
    Ok(ShellPayloadStatus),
    Missing,
    StatusUnreadable,
    ExeMissing,
    ExeHashMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPayloadStatusCorrupt {
    pub path: PathBuf,
    pub detail: String,
}

impl ShellPayloadStatusCorrupt {
    fn new(path: &Path, detail: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ShellPayloadStatusCorrupt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Shell payload status file is not valid JSON: {}",
            self.path.display()
        )
    }
}

impl std::error::Error for ShellPayloadStatusCorrupt {}

pub fn find_shell_payload_pin(kind: ShellPayloadKind) -> Option<&'static ShellPayloadPin> {
    SHELL_PAYLOAD_MANIFEST.iter().find(|pin| pin.kind == kind)
}

pub fn shell_payload_pin_of(kind: ShellPayloadKind) -> &'static ShellPayloadPin {
    find_shell_payload_pin(kind).expect("every shell payload kind is registered in the manifest")
}

pub fn resolve_shell_payload_home(kimi_code_home: Option<&str>, os_home_dir: &Path) -> PathBuf {
    if let Some(from_env) = kimi_code_home
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return PathBuf::from(from_env);
    }
    os_home_dir.join(".kimi-code")
}

pub fn shell_payloads_dir(kimi_home: &Path) -> PathBuf {
    kimi_home.join("shells")
}

pub fn shell_payload_install_dir(kimi_home: &Path, pin: &ShellPayloadPin) -> PathBuf {
    shell_payloads_dir(kimi_home).join(format!("{}-{}", pin.kind.as_str(), pin.version))
}

pub fn shell_payload_status_path(kimi_home: &Path, kind: ShellPayloadKind) -> PathBuf {
    shell_payloads_dir(kimi_home).join(format!("{}.json", kind.as_str()))
}

pub fn shell_payload_exe_path(kimi_home: &Path, pin: &ShellPayloadPin) -> PathBuf {
    let mut path = shell_payload_install_dir(kimi_home, pin);
    for component in pin.exe_rel_path.split('/') {
        path = path.join(component);
    }
    path
}

pub fn hash_file_sha256(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

pub fn path_exists(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
}

pub fn read_shell_payload_status(
    kimi_home: &Path,
    kind: ShellPayloadKind,
) -> Result<Option<ShellPayloadStatus>, ShellPayloadStatusCorrupt> {
    let path = shell_payload_status_path(kimi_home, kind);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| ShellPayloadStatusCorrupt::new(&path, error.to_string()))?;
    normalize_shell_payload_status(parsed, kind, &path).map(Some)
}

pub fn write_shell_payload_status(
    kimi_home: &Path,
    status: &ShellPayloadStatus,
) -> std::io::Result<()> {
    std::fs::create_dir_all(shell_payloads_dir(kimi_home))?;
    let path = shell_payload_status_path(kimi_home, status.kind);
    let text = serde_json::to_string_pretty(status).map_err(std::io::Error::other)?;
    std::fs::write(path, format!("{text}\n"))
}

pub fn verify_shell_payload(kimi_home: &Path, kind: ShellPayloadKind) -> ShellPayloadVerifyResult {
    let status = match read_shell_payload_status(kimi_home, kind) {
        Ok(Some(status)) => status,
        Ok(None) => return ShellPayloadVerifyResult::Missing,
        Err(_) => return ShellPayloadVerifyResult::StatusUnreadable,
    };
    let exe_path = Path::new(&status.exe_path);
    if !path_exists(exe_path) {
        return ShellPayloadVerifyResult::ExeMissing;
    }
    match hash_file_sha256(exe_path) {
        Ok(exe_sha256) if exe_sha256 == status.exe_sha256 => ShellPayloadVerifyResult::Ok(status),
        _ => ShellPayloadVerifyResult::ExeHashMismatch,
    }
}

fn normalize_shell_payload_status(
    parsed: serde_json::Value,
    expected_kind: ShellPayloadKind,
    path: &Path,
) -> Result<ShellPayloadStatus, ShellPayloadStatusCorrupt> {
    if !parsed.is_object() {
        return Err(ShellPayloadStatusCorrupt::new(path, "not an object"));
    }
    let status: ShellPayloadStatus = serde_json::from_value(parsed)
        .map_err(|error| ShellPayloadStatusCorrupt::new(path, error.to_string()))?;
    if status.kind != expected_kind {
        return Err(ShellPayloadStatusCorrupt::new(
            path,
            format!("kind mismatch: {}", status.kind.as_str()),
        ));
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_status(home: &Path, status: &ShellPayloadStatus) {
        write_shell_payload_status(home, status).unwrap();
    }

    fn status_for(kind: ShellPayloadKind, exe: &Path, exe_sha256: &str) -> ShellPayloadStatus {
        let pin = shell_payload_pin_of(kind);
        ShellPayloadStatus {
            kind,
            version: pin.version.to_string(),
            sha256: pin.sha256.to_string(),
            exe_path: exe.display().to_string(),
            exe_sha256: exe_sha256.to_string(),
            verified_at: "2026-10-29T17:57:58.946Z".to_string(),
        }
    }

    #[test]
    fn the_manifest_registers_one_pin_per_kind() {
        assert_eq!(SHELL_PAYLOAD_MANIFEST.len(), 2);
        assert_eq!(
            shell_payload_pin_of(ShellPayloadKind::Bash).version,
            "2.56.0.4"
        );
        assert_eq!(
            shell_payload_pin_of(ShellPayloadKind::Pwsh).version,
            "7.6.9"
        );
        assert_eq!(
            shell_payload_pin_of(ShellPayloadKind::Bash)
                .archive
                .as_str(),
            "self-extract-7z"
        );
        assert_eq!(
            shell_payload_pin_of(ShellPayloadKind::Pwsh).exe_rel_path,
            "pwsh.exe"
        );
    }

    #[test]
    fn payload_paths_follow_the_versioned_install_layout() {
        let home = Path::new("D:\\kimi-home");
        let pin = shell_payload_pin_of(ShellPayloadKind::Bash);
        let expected = home
            .join("shells")
            .join("bash-2.56.0.4")
            .join("usr")
            .join("bin")
            .join("bash.exe");
        assert_eq!(shell_payload_exe_path(home, pin), expected);
        assert_eq!(
            shell_payload_status_path(home, ShellPayloadKind::Pwsh),
            home.join("shells").join("pwsh.json")
        );
    }

    #[test]
    fn kimi_home_prefers_a_non_empty_override() {
        let os_home = Path::new("C:\\Users\\tester");
        assert_eq!(
            resolve_shell_payload_home(Some("D:\\custom"), os_home),
            PathBuf::from("D:\\custom")
        );
        assert_eq!(
            resolve_shell_payload_home(Some("   "), os_home),
            os_home.join(".kimi-code")
        );
        assert_eq!(
            resolve_shell_payload_home(None, os_home),
            os_home.join(".kimi-code")
        );
    }

    #[test]
    fn a_missing_status_file_is_missing_not_corrupt() {
        let home = tempdir().unwrap();
        assert_eq!(
            verify_shell_payload(home.path(), ShellPayloadKind::Pwsh),
            ShellPayloadVerifyResult::Missing
        );
    }

    #[test]
    fn an_unparseable_status_file_is_unreadable() {
        let home = tempdir().unwrap();
        std::fs::create_dir_all(shell_payloads_dir(home.path())).unwrap();
        std::fs::write(
            shell_payload_status_path(home.path(), ShellPayloadKind::Pwsh),
            "{",
        )
        .unwrap();
        assert_eq!(
            verify_shell_payload(home.path(), ShellPayloadKind::Pwsh),
            ShellPayloadVerifyResult::StatusUnreadable
        );
        assert!(read_shell_payload_status(home.path(), ShellPayloadKind::Pwsh).is_err());
    }

    #[test]
    fn a_status_file_naming_another_kind_is_corrupt() {
        let home = tempdir().unwrap();
        let exe = home.path().join("pwsh.exe");
        std::fs::write(&exe, b"fake").unwrap();
        write_status(home.path(), &status_for(ShellPayloadKind::Bash, &exe, "00"));
        std::fs::rename(
            shell_payload_status_path(home.path(), ShellPayloadKind::Bash),
            shell_payload_status_path(home.path(), ShellPayloadKind::Pwsh),
        )
        .unwrap();
        assert_eq!(
            verify_shell_payload(home.path(), ShellPayloadKind::Pwsh),
            ShellPayloadVerifyResult::StatusUnreadable
        );
    }

    #[test]
    fn a_missing_exe_is_reported_before_hashing() {
        let home = tempdir().unwrap();
        let exe = home.path().join("does-not-exist.exe");
        write_status(home.path(), &status_for(ShellPayloadKind::Pwsh, &exe, "00"));
        assert_eq!(
            verify_shell_payload(home.path(), ShellPayloadKind::Pwsh),
            ShellPayloadVerifyResult::ExeMissing
        );
    }

    #[test]
    fn a_hash_mismatch_is_reported() {
        let home = tempdir().unwrap();
        let exe = home.path().join("pwsh.exe");
        std::fs::write(&exe, b"fake").unwrap();
        write_status(home.path(), &status_for(ShellPayloadKind::Pwsh, &exe, "00"));
        assert_eq!(
            verify_shell_payload(home.path(), ShellPayloadKind::Pwsh),
            ShellPayloadVerifyResult::ExeHashMismatch
        );
    }

    #[test]
    fn a_matching_hash_verifies_and_returns_the_status() {
        let home = tempdir().unwrap();
        let exe = home.path().join("pwsh.exe");
        std::fs::write(&exe, b"fake").unwrap();
        let hash = hash_file_sha256(&exe).unwrap();
        let status = status_for(ShellPayloadKind::Pwsh, &exe, &hash);
        write_status(home.path(), &status);
        assert_eq!(
            verify_shell_payload(home.path(), ShellPayloadKind::Pwsh),
            ShellPayloadVerifyResult::Ok(status)
        );
    }

    #[test]
    fn the_written_status_uses_the_camel_case_wire_shape() {
        let home = tempdir().unwrap();
        let exe = home.path().join("pwsh.exe");
        std::fs::write(&exe, b"fake").unwrap();
        let status = status_for(ShellPayloadKind::Pwsh, &exe, "00");
        write_status(home.path(), &status);
        let text = std::fs::read_to_string(shell_payload_status_path(
            home.path(),
            ShellPayloadKind::Pwsh,
        ))
        .unwrap();
        assert!(text.ends_with('\n'));
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["kind"], "pwsh");
        assert!(parsed.get("exePath").is_some());
        assert!(parsed.get("exeSha256").is_some());
        assert!(parsed.get("verifiedAt").is_some());
    }
}
