//! Workspace trust (v2 `workspace/workspaceTrust/*`): whether a workspace may
//! load its project-level configuration (`.mcp.json`, `workspace.additional_dir`,
//! skills) without an explicit user confirmation.
//!
//! The decision combines three inputs, in upstream `WorkspaceTrustEntry.isTrusted`
//! order (upstream #4517):
//!
//! 1. `[workspace] require_trust` in `config.toml` — `false` disables the gate
//!    and treats every workspace as trusted;
//! 2. `KIMI_CODE_TRUST_WORKSPACE` (upstream #4059) — an independent per-process
//!    override that ORs over the persisted record while the gate is on; a falsy
//!    spelling does **not** re-enable a disabled gate;
//! 3. the persisted record, `trusted-workspaces.json` in the Kimi home — a JSON
//!    array of workspace roots the user explicitly trusted.
//!
//! Every query re-reads all three from disk: the config file, the process env
//! and the record are all allowed to change between queries (v2
//! `WorkspaceTrustEntry.refresh`, which the MCP registry calls on every
//! cwd-scoped listing). Nothing here caches.

use std::path::{Path, PathBuf};

use crate::config::KimiConfig;

/// The record file's name inside the Kimi home directory.
const TRUSTED_WORKSPACES_FILE: &str = "trusted-workspaces.json";

/// `KIMI_CODE_TRUST_WORKSPACE` as v2 `trustWorkspaceEnvTrusted`: only an
/// explicit truthy spelling opts every workspace in (`parseBooleanEnv` returns
/// `undefined` for anything unrecognised, so a typo keeps the prompt).
pub fn env_trust_override() -> bool {
    crate::env::parse_bool_env(std::env::var("KIMI_CODE_TRUST_WORKSPACE").ok().as_deref())
        == Some(true)
}

/// The comparison key for a workspace root (node-sdk `workspaceTrustKey`,
/// which mirrors v2 `canonicalWorkspaceRoot`).
///
/// The key is the *real* path, not a normalized string: a symlink or junction
/// spelled one way today and another tomorrow names the same directory.
/// A path that cannot be resolved falls back to its absolute normalized form:
/// a workspace deleted after being trusted still has to match its stored
/// entry. Windows compares case-insensitively; `\\?\` verbatim prefixes from
/// `fs::canonicalize` are stripped (Node's `realpathSync.native` does not
/// produce them).
pub fn trust_key(root: &str) -> String {
    let absolute = std::path::absolute(root).unwrap_or_else(|_| PathBuf::from(root));
    let real = std::fs::canonicalize(&absolute).unwrap_or(absolute);
    let real = strip_verbatim_prefix(&real);
    let mut key = real.to_string_lossy().replace('\\', "/");
    while key.ends_with('/') {
        key.pop();
    }
    if cfg!(windows) {
        key = key.to_lowercase();
    }
    key
}

/// The effective trust state for `work_dir`, re-read from disk on every call.
///
/// Fail closed: an unreadable or unparseable `config.toml` keeps the gate on
/// (`require_trust` defaults to `true`), exactly like a missing record does.
pub fn effective_trust(home_dir: &Path, work_dir: &str) -> bool {
    effective_trust_with(home_dir, work_dir, env_trust_override())
}

/// The testable core of [`effective_trust`], with the process-env half
/// injected so unit tests never touch the shared process environment.
fn effective_trust_with(home_dir: &Path, work_dir: &str, env_trusted: bool) -> bool {
    let require = match KimiConfig::from_file(&home_dir.join("config.toml")) {
        Ok(config) => config.resolve_workspace_require_trust(),
        Err(_) => true,
    };
    if !require {
        return true;
    }
    if env_trusted {
        return true;
    }
    let key = trust_key(work_dir);
    read_trusted_workspaces(home_dir)
        .iter()
        .any(|entry| trust_key(entry) == key)
}

/// Persist a trust decision: append `work_dir` to the record unless a matching
/// key is already present. The record is written even when the gate is
/// disabled — upstream preserves existing records so a re-enabled gate finds
/// them intact, and recording early keeps that symmetric.
pub fn record_trust(home_dir: &Path, work_dir: &str) -> Result<(), String> {
    let mut list = read_trusted_workspaces(home_dir);
    let key = trust_key(work_dir);
    if list.iter().any(|entry| trust_key(entry) == key) {
        return Ok(());
    }
    list.push(work_dir.to_string());
    let body = serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?;
    std::fs::write(home_dir.join(TRUSTED_WORKSPACES_FILE), body).map_err(|e| e.to_string())
}

/// The persisted record as a list of roots. Any read/parse failure — a missing
/// file included — reads as an empty record (fail closed), and non-string
/// entries are dropped (the record is user-editable JSON).
fn read_trusted_workspaces(home_dir: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(home_dir.join(TRUSTED_WORKSPACES_FILE)) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) else {
        return Vec::new();
    };
    parsed
        .into_iter()
        .filter_map(|entry| entry.as_str().map(str::to_string))
        .collect()
}

/// `\\?\C:\foo` → `C:\foo` on Windows; a no-op everywhere else.
#[cfg(windows)]
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    const VERBATIM_PREFIX: &str = "\\\\?\\";
    // After stripping `\\?\`, a UNC path (`\\?\UNC\server\share`) starts with
    // `UNC\`; it keeps its leading `\\` (`\\server\share`).
    const UNC_PREFIX: &str = "UNC\\";
    let text = path.to_string_lossy();
    match text.strip_prefix(VERBATIM_PREFIX) {
        Some(rest) if rest.starts_with(UNC_PREFIX) => {
            // `UNC\server\share` → `\\server\share`
            let tail = &rest[UNC_PREFIX.len()..];
            let mut rebuilt = String::with_capacity(tail.len() + 1);
            rebuilt.push('\\');
            rebuilt.push_str(tail);
            PathBuf::from(rebuilt)
        }
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(config: Option<&str>, record: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(text) = config {
            std::fs::write(dir.path().join("config.toml"), text).unwrap();
        }
        if let Some(text) = record {
            std::fs::write(dir.path().join(TRUSTED_WORKSPACES_FILE), text).unwrap();
        }
        dir
    }

    #[test]
    fn the_gate_fails_closed_without_a_config_or_a_record() {
        let home = home_with(None, None);
        let work = tempfile::tempdir().unwrap();
        assert!(!effective_trust_with(
            home.path(),
            work.path().to_str().unwrap(),
            false
        ));
        // An unparseable config keeps the gate on too.
        let broken = home_with(Some("[workspace\nnot toml"), None);
        assert!(!effective_trust_with(
            broken.path(),
            work.path().to_str().unwrap(),
            false
        ));
    }

    #[test]
    fn require_trust_false_short_circuits_every_workspace() {
        // Upstream #4517: with the gate off, an unrecorded workspace is
        // trusted — and a falsy process override must not re-enable the gate.
        let home = home_with(Some("[workspace]\nrequire_trust = false\n"), None);
        let work = tempfile::tempdir().unwrap();
        assert!(effective_trust_with(
            home.path(),
            work.path().to_str().unwrap(),
            false
        ));
    }

    #[test]
    fn the_config_is_re_read_on_every_query() {
        let home = home_with(Some("[workspace]\nrequire_trust = false\n"), None);
        let work = tempfile::tempdir().unwrap();
        let work = work.path().to_str().unwrap().to_string();
        assert!(effective_trust_with(home.path(), &work, false));

        std::fs::write(
            home.path().join("config.toml"),
            "[workspace]\nrequire_trust = true\n",
        )
        .unwrap();
        assert!(!effective_trust_with(home.path(), &work, false));

        std::fs::write(
            home.path().join("config.toml"),
            "[workspace]\nrequire_trust = false\n",
        )
        .unwrap();
        assert!(effective_trust_with(home.path(), &work, false));
    }

    #[test]
    fn the_record_decides_while_the_gate_is_on() {
        let work = tempfile::tempdir().unwrap();
        let work_dir = work.path().to_str().unwrap().to_string();
        let home = home_with(
            Some("[workspace]\nrequire_trust = true\n"),
            Some(&serde_json::to_string(&vec![work_dir.clone()]).unwrap()),
        );
        assert!(effective_trust_with(home.path(), &work_dir, false));

        let other = tempfile::tempdir().unwrap();
        assert!(!effective_trust_with(
            home.path(),
            other.path().to_str().unwrap(),
            false
        ));
    }

    #[test]
    fn the_env_override_ors_over_the_record_but_not_over_the_gate() {
        let work = tempfile::tempdir().unwrap();
        let work_dir = work.path().to_str().unwrap().to_string();

        // Gate on, no record: the truthy override trusts.
        let home = home_with(Some("[workspace]\nrequire_trust = true\n"), None);
        assert!(effective_trust_with(home.path(), &work_dir, true));

        // Gate off: trusted regardless of the override half.
        let home = home_with(Some("[workspace]\nrequire_trust = false\n"), None);
        assert!(effective_trust_with(home.path(), &work_dir, true));
        assert!(effective_trust_with(home.path(), &work_dir, false));
    }

    #[test]
    fn record_trust_roundtrips_and_stays_idempotent() {
        let home = home_with(Some("[workspace]\nrequire_trust = true\n"), None);
        let work = tempfile::tempdir().unwrap();
        let work_dir = work.path().to_str().unwrap().to_string();

        record_trust(home.path(), &work_dir).unwrap();
        assert!(effective_trust_with(home.path(), &work_dir, false));

        // A second record with a differently-spelled path adds nothing.
        record_trust(home.path(), &format!("{work_dir}/")).unwrap();
        let recorded = read_trusted_workspaces(home.path());
        assert_eq!(recorded, vec![work_dir]);
    }

    #[test]
    fn trust_key_normalizes_separators_and_trailing_slashes() {
        let work = tempfile::tempdir().unwrap();
        let plain = work.path().to_str().unwrap().to_string();
        let slashed = format!("{plain}/");
        assert_eq!(trust_key(&plain), trust_key(&slashed));
        assert!(!trust_key(&plain).ends_with('/'));
        #[cfg(windows)]
        assert_eq!(trust_key(&plain.to_uppercase()), trust_key(&plain));
    }

    #[test]
    fn trust_key_survives_a_deleted_workspace() {
        // A missing path cannot be canonicalized: it falls back to its
        // absolute normalized form instead of erroring, so a trusted record
        // still matches the directory that used to exist.
        let missing = tempfile::tempdir().unwrap().path().join("gone");
        let key = trust_key(missing.to_str().unwrap());
        assert!(!key.is_empty());
        assert_eq!(key, trust_key(missing.to_str().unwrap()));
    }
}
