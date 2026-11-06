//! The default workspace (upstream #4224 + #4277): where a session lives when
//! the request names no workspace and no cwd.
//!
//! Resolution, in upstream order
//! (`agent-core-v2/src/app/defaultWorkspace/defaultWorkspaceService.ts:76-86`
//! @ `a2afdbc7d`, post-#4277 shape):
//!
//! 1. `[default_workspace] path` in `config.toml`, when set to an absolute
//!    path (or `~/…`). A relative or empty value is refused, never guessed —
//!    guessing would create a second default workspace on a machine whose
//!    intent was a different location.
//! 2. Otherwise `<home>/KimiCode/Default` (`DEFAULT_WORKSPACE_RELATIVE_ROOT`).
//!    `~/KimiCode/Default` on every platform is the whole point of #4277:
//!    `~/Documents` triggered macOS privacy prompts and iCloud/OneDrive
//!    sync, and a denial blocked every later chat.
//!
//! `ensure` creates the directory on first use and reuses it afterwards; a
//! failure is classified (blocked by a file / permission / missing parent /
//! no space / read-only) rather than silently relocating the workspace — the
//! server never relocates silently (upstream #4277's stated rule).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::KimiConfig;

/// `<home>/KimiCode/Default`, as path segments
/// (`DEFAULT_WORKSPACE_RELATIVE_ROOT`, `defaultWorkspacePaths.ts:5`).
pub const DEFAULT_WORKSPACE_RELATIVE_ROOT: [&str; 2] = ["KimiCode", "Default"];

/// Why the default workspace could not be ensured. Wire names follow the
/// upstream schema (`kap-server/src/protocol/default-workspace.ts` @
/// `e5e56bc5fe`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultWorkspaceProblem {
    BlockedByFile,
    PermissionDenied,
    MissingParent,
    NoSpace,
    ReadOnly,
    InvalidPath,
    Unknown,
}

impl DefaultWorkspaceProblem {
    /// The user-facing sentence, matching
    /// `DEFAULT_WORKSPACE_PROBLEM_MESSAGES` upstream.
    pub fn message(self) -> &'static str {
        match self {
            Self::BlockedByFile => "a file exists where the directory would be created",
            Self::PermissionDenied => "permission denied",
            Self::MissingParent => "the parent directory does not exist",
            Self::NoSpace => "no space left on device",
            Self::ReadOnly => "the file system is read-only",
            Self::InvalidPath => "the configured path is not an absolute path",
            Self::Unknown => "unknown error",
        }
    }
}

/// A failure to ensure the default workspace, carrying the upstream reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultWorkspaceUnavailable {
    pub path: String,
    pub reason: DefaultWorkspaceProblem,
}

impl std::fmt::Display for DefaultWorkspaceUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "default workspace {} is unavailable: {}",
            self.path,
            self.reason.message()
        )
    }
}

/// The user home directory, the way [`crate::prompt::agents_md`] resolves it
/// (HOME, else USERPROFILE). One helper so every consumer of this module sees
/// the same home even when the env differs from the process shell's.
pub fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The configured custom location, if usable.
///
/// Upstream keeps this in `[defaultWorkspace]` (`config.toml:
/// default_workspace.path`); the fork's snake_case file spelling matches
/// every other section.
fn custom_path(config: &KimiConfig) -> Option<String> {
    let raw = config.default_workspace.path.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(raw.to_string())
}

/// Resolve the configured custom path against the home directory.
///
/// `~/…` expands; anything not absolute on its platform dialect is refused
/// (`resolveCustomRoot`, `defaultWorkspacePaths.ts:26-35` @ `e5e56bc5fe` —
/// the post-#4277 shape, which dropped the XDG/PowerShell lookups).
pub fn resolve_custom_root(raw: &str, home: &Path) -> Option<PathBuf> {
    let expanded = if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        home.join(rest)
    } else if raw == "~" {
        home.to_path_buf()
    } else {
        PathBuf::from(raw)
    };
    // An absolute path on either dialect is accepted; a relative one is not
    // (`Path::is_absolute` is dialect-aware, which is the whole test).
    if expanded.is_absolute() {
        Some(normalize_separators(&expanded))
    } else {
        None
    }
}

/// Forward-slash form, matching the engine's wire paths.
fn normalize_separators(path: &Path) -> PathBuf {
    PathBuf::from(path.display().to_string().replace('\\', "/"))
}

/// The configured-or-default root, without touching the disk.
pub fn resolve_root(
    config: &KimiConfig,
    home: &Path,
) -> Result<PathBuf, DefaultWorkspaceUnavailable> {
    let path = match custom_path(config) {
        Some(raw) => {
            resolve_custom_root(&raw, home).ok_or_else(|| DefaultWorkspaceUnavailable {
                path: raw.clone(),
                reason: DefaultWorkspaceProblem::InvalidPath,
            })?
        }
        None => home.join(DEFAULT_WORKSPACE_RELATIVE_ROOT.iter().collect::<PathBuf>()),
    };
    Ok(path)
}

/// Ensure the root exists (create on first use, reuse afterwards) and return
/// it — upstream `IDefaultWorkspace.ensure`.
pub fn ensure(config: &KimiConfig, home: &Path) -> Result<PathBuf, DefaultWorkspaceUnavailable> {
    let root = resolve_root(config, home)?;
    let path = root.display().to_string();
    match std::fs::create_dir_all(&root) {
        Ok(()) => Ok(root),
        // `create_dir_all` reports the *deepest* cause; the classification
        // reads the existing-entry case first, because "a file is sitting
        // where the directory would be" is by far the common way this fails.
        Err(error) => {
            // `create_dir_all` fails on the first existing non-directory
            // ancestor, which is usually a *parent* of the leaf ("a file
            // named KimiCode sits where the folder chain would go"). Scan
            // upward for that — checking only the leaf misses it.
            let blocked_by_file = root
                .ancestors()
                .skip(1)
                .take_while(|p| *p != home)
                .find(|p| p.exists() && !p.is_dir())
                .is_some();
            let reason = if blocked_by_file || root.is_file() {
                DefaultWorkspaceProblem::BlockedByFile
            } else if error.kind() == std::io::ErrorKind::PermissionDenied {
                DefaultWorkspaceProblem::PermissionDenied
            } else {
                match error.raw_os_error() {
                    Some(code) if is_no_space(code) => DefaultWorkspaceProblem::NoSpace,
                    Some(code) if is_read_only(code) => DefaultWorkspaceProblem::ReadOnly,
                    _ => DefaultWorkspaceProblem::Unknown,
                }
            };
            Err(DefaultWorkspaceUnavailable { path, reason })
        }
    }
}

#[cfg(unix)]
fn is_no_space(code: i32) -> bool {
    code == nix_errno_enospc()
}

#[cfg(unix)]
fn is_read_only(code: i32) -> bool {
    code == nix_errno_erofs()
}

#[cfg(unix)]
fn nix_errno_enospc() -> i32 {
    28 // ENOSPC
}

#[cfg(unix)]
fn nix_errno_erofs() -> i32 {
    30 // EROFS
}

#[cfg(not(unix))]
fn is_no_space(_code: i32) -> bool {
    false
}

#[cfg(not(unix))]
fn is_read_only(_code: i32) -> bool {
    false
}

/// Whether `root` *is* the default workspace — the `is_default` wire flag.
///
/// Compared through the same root key the workspaces table uses
/// (`workspaceRootKey` upstream), so case and separator spelling cannot make
/// one directory into two workspaces.
pub fn is_default_root(root: &str, config: &KimiConfig, home: &Path) -> bool {
    match resolve_root(config, home) {
        Ok(default) => is_default_root_resolved(root, &default),
        Err(_) => false,
    }
}

/// The same comparison against an already-resolved root — the wire path uses
/// this, so nine `format_wire_session` call sites never re-parse config.
pub fn is_default_root_resolved(root: &str, default_root: &Path) -> bool {
    crate::session::sqlite_store::workspace_root_key(root)
        == crate::session::sqlite_store::workspace_root_key(&default_root.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(path: Option<&str>) -> KimiConfig {
        let mut config = KimiConfig::default();
        config.default_workspace.path = path.map(str::to_string);
        config
    }

    /// A home directory that is absolute *on the running platform*, so the
    /// fixture holds on Windows (where `/home/u` is not absolute) and Unix.
    fn absolute_home(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "kimi-default-ws-{tag}-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn default_root_is_home_kimicode_default() {
        let home = absolute_home("def");
        let root = resolve_root(&config_with(None), &home).unwrap();
        assert_eq!(root, home.join("KimiCode").join("Default"));
        std::fs::remove_dir_all(&home).ok();
    }

    /// An absolute custom path is used as-is (forward-slash form).
    #[test]
    fn custom_absolute_path_is_used() {
        let home = absolute_home("abs");
        let custom = home.join("srv").join("default");
        let root = resolve_root(&config_with(Some(&custom.display().to_string())), &home).unwrap();
        assert_eq!(root, custom);
        std::fs::remove_dir_all(&home).ok();
    }

    /// `~/…` expands against the home directory.
    #[test]
    fn tilde_expands() {
        let home = absolute_home("tilde");
        let root = resolve_root(&config_with(Some("~/ws")), &home).unwrap();
        assert_eq!(root, home.join("ws"));
        std::fs::remove_dir_all(&home).ok();
    }

    /// A relative custom path is refused, not resolved against the cwd:
    /// guessing would create a second default workspace on a machine whose
    /// intent was elsewhere.
    #[test]
    fn relative_custom_path_is_refused() {
        let err =
            resolve_root(&config_with(Some("relative/path")), Path::new("/home/u")).unwrap_err();
        assert_eq!(err.reason, DefaultWorkspaceProblem::InvalidPath);
        // The config file's whitespace is not intent either.
        assert!(
            resolve_root(&config_with(Some("   ")), Path::new("/home/u")).is_ok(),
            "an empty value means 'use the default'"
        );
    }

    #[test]
    fn ensure_creates_and_reuses() {
        let dir = std::env::temp_dir().join(format!(
            "kimi-default-ws-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        let home = dir.join("home");
        let config = config_with(None);
        let first = ensure(&config, &home).unwrap();
        assert!(first.is_dir());
        let second = ensure(&config, &home).unwrap();
        assert_eq!(first, second);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file sitting where the directory would be is the common failure, and
    /// it is classified rather than relocated.
    #[test]
    fn a_blocking_file_is_reported_not_relocated() {
        let dir = std::env::temp_dir().join(format!(
            "kimi-default-ws-block-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("KimiCode"), "not a directory").unwrap();

        let err = ensure(&config_with(None), &home).unwrap_err();
        assert_eq!(err.reason, DefaultWorkspaceProblem::BlockedByFile);
        assert!(err.path.contains("Default"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The wire flag compares through the same root key the table uses, so
    /// `C:/x` and `c:/x/` are one workspace.
    #[test]
    fn is_default_compares_by_root_key() {
        let home = absolute_home("flag");
        let config = config_with(None);
        let default = resolve_root(&config, &home).unwrap();
        let as_string = default.display().to_string();
        assert!(is_default_root(&as_string, &config, &home));
        assert!(is_default_root(&format!("{as_string}/"), &config, &home));
        assert!(!is_default_root(
            &home.join("other").display().to_string(),
            &config,
            &home
        ));
        std::fs::remove_dir_all(&home).ok();
    }
}
