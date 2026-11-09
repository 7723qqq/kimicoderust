//! The per-session environment binding: fixed at creation, restored on resume.
//!
//! Upstream `app/environmentDeclaration/environmentService.ts` acquires an
//! environment when a session is created and stores `environmentId` plus
//! `environmentCwd` on the session (`createSession`). A resume reads those back
//! instead of re-deriving them from the current config, so editing
//! `[environments]` cannot silently move a session that is already running.
//!
//! The fork persisted the two columns but never wrote them, so every resume
//! re-derived the cwd from whatever the request happened to say. This module
//! makes the snapshot explicit: [`EnvironmentSnapshot::capture`] is the only
//! writer, [`EnvironmentSnapshot::restore`] is the only reader, and a config
//! change that would alter the environment surfaces as
//! [`EnvironmentDrift`] rather than being applied behind the caller's back.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::declaration::{EnvironmentDeclaration, EnvironmentKind};
use super::workdir;
use super::{EnvironmentError, WorkspaceRoots, codes};

/// The two values persisted on the session row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentBinding {
    pub environment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

/// What the store hands back on resume: the binding plus the frozen variables.
///
/// Kept separate from [`EnvironmentSnapshot`] because a stored snapshot has not
/// been validated yet — its cwd may no longer exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEnvironmentSnapshot {
    pub binding: EnvironmentBinding,
    pub env: BTreeMap<String, String>,
}

/// The frozen environment a session runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentSnapshot {
    binding: EnvironmentBinding,
    env: BTreeMap<String, String>,
}

/// A difference between the stored snapshot and the current declaration.
///
/// Drift is reported, not applied: the stored values keep winning, and the
/// caller decides whether to log, warn the user, or rebind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentDrift {
    /// A stored variable differs from what the declaration now says.
    EnvChanged {
        key: String,
        stored: Option<String>,
        declared: Option<String>,
    },
    /// The binding carried no cwd, so the declaration's `defaultCwd` was used.
    CwdFromDeclaration { cwd: PathBuf },
}

/// A restore result: the snapshot and any drift observed while rebuilding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    pub snapshot: EnvironmentSnapshot,
    pub drift: Vec<EnvironmentDrift>,
}

impl EnvironmentSnapshot {
    /// Freeze the environment for a new session.
    ///
    /// `cwd` wins over the declaration's `defaultCwd`; `env` overrides the
    /// declaration's variables key by key. The cwd is resolved through
    /// [`workdir`], so a missing directory fails here with its own code instead
    /// of producing a session that cannot start.
    pub fn capture(
        declaration: &EnvironmentDeclaration,
        requested: Option<&str>,
        cwd: Option<&Path>,
        env: BTreeMap<String, String>,
        roots: &WorkspaceRoots,
        allow_outside: bool,
    ) -> Result<Self, EnvironmentError> {
        let resolved = declaration.resolve(requested)?;
        if resolved.kind != EnvironmentKind::Local {
            // `resolve` refuses remote transports today; this guard keeps the
            // invariant local to the snapshot if that ever changes.
            return Err(EnvironmentError::new(
                codes::ENVIRONMENT_UNAVAILABLE,
                format!("environment '{}' is not a local environment", resolved.id),
            ));
        }
        let raw_cwd = match cwd {
            Some(cwd) => cwd.to_path_buf(),
            None => match resolved.default_cwd {
                Some(default_cwd) => PathBuf::from(default_cwd),
                None => {
                    return Err(EnvironmentError::new(
                        codes::WORKDIR_REQUIRED,
                        format!("environment '{}' needs a working directory", resolved.id),
                    ));
                }
            },
        };
        let resolved_cwd = workdir::resolve_within(roots, &raw_cwd, allow_outside)?;
        let mut merged_env = resolved.env;
        merged_env.extend(env);
        Ok(Self {
            binding: EnvironmentBinding {
                environment_id: resolved.id,
                cwd: Some(resolved_cwd),
            },
            env: merged_env,
        })
    }

    /// Rebuild the snapshot from the persisted binding.
    ///
    /// The stored cwd is authoritative: if it no longer resolves, this fails
    /// with `environment.invalid_cwd`. Falling back to the declaration's
    /// `defaultCwd` would silently relocate the session.
    pub fn restore(
        declaration: &EnvironmentDeclaration,
        binding: &EnvironmentBinding,
        stored_env: Option<BTreeMap<String, String>>,
        roots: &WorkspaceRoots,
        allow_outside: bool,
    ) -> Result<RestoreOutcome, EnvironmentError> {
        let resolved = declaration.resolve(Some(&binding.environment_id))?;
        if resolved.kind != EnvironmentKind::Local {
            return Err(EnvironmentError::new(
                codes::ENVIRONMENT_UNAVAILABLE,
                format!("environment '{}' is not a local environment", resolved.id),
            ));
        }

        let mut drift = Vec::new();
        let raw_cwd = match &binding.cwd {
            Some(cwd) => cwd.clone(),
            None => match resolved.default_cwd {
                Some(default_cwd) => {
                    let cwd = PathBuf::from(default_cwd);
                    drift.push(EnvironmentDrift::CwdFromDeclaration { cwd: cwd.clone() });
                    cwd
                }
                None => {
                    return Err(EnvironmentError::new(
                        codes::WORKDIR_REQUIRED,
                        format!("environment '{}' has no persisted cwd", resolved.id),
                    ));
                }
            },
        };
        let resolved_cwd = workdir::resolve_within(roots, &raw_cwd, allow_outside)?;

        let env = match stored_env {
            Some(stored) => {
                // Only variables the declaration still manages are compared.
                // A stored-only key came from an explicit per-session
                // override, not from the declaration, so it is not drift.
                for (key, declared_value) in &resolved.env {
                    let stored_value = stored.get(key).cloned();
                    if stored_value.as_ref() != Some(declared_value) {
                        drift.push(EnvironmentDrift::EnvChanged {
                            key: key.clone(),
                            stored: stored_value,
                            declared: Some(declared_value.clone()),
                        });
                    }
                }
                drift.sort_by(|a, b| drift_key(a).cmp(&drift_key(b)));
                stored
            }
            None => resolved.env,
        };

        Ok(RestoreOutcome {
            snapshot: Self {
                binding: EnvironmentBinding {
                    environment_id: resolved.id,
                    cwd: Some(resolved_cwd),
                },
                env,
            },
            drift,
        })
    }

    #[must_use]
    pub fn environment_id(&self) -> &str {
        &self.binding.environment_id
    }

    /// The resolved, existing cwd. Always present after `capture`/`restore`.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        self.binding
            .cwd
            .as_deref()
            .expect("a captured or restored snapshot always has a cwd")
    }

    #[must_use]
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    #[must_use]
    pub fn binding(&self) -> &EnvironmentBinding {
        &self.binding
    }
}

fn drift_key(drift: &EnvironmentDrift) -> String {
    match drift {
        EnvironmentDrift::EnvChanged { key, .. } => format!("env:{key}"),
        EnvironmentDrift::CwdFromDeclaration { .. } => "cwd".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kimi-environment-binding-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn local_declaration() -> EnvironmentDeclaration {
        EnvironmentDeclaration::default()
    }

    #[test]
    fn capture_freezes_the_resolved_cwd_and_overrides() {
        let work = tempdir();
        let roots = WorkspaceRoots::new(&work, Vec::new()).unwrap();
        let env = BTreeMap::from([("FOO".to_string(), "bar".to_string())]);
        let snapshot = EnvironmentSnapshot::capture(
            &local_declaration(),
            None,
            Some(&work),
            env.clone(),
            &roots,
            false,
        )
        .unwrap();
        assert_eq!(snapshot.environment_id(), "local");
        assert_eq!(snapshot.cwd(), fs::canonicalize(&work).unwrap());
        assert_eq!(snapshot.env(), &env);
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn capture_without_a_cwd_is_required_not_defaulted() {
        let work = tempdir();
        let roots = WorkspaceRoots::new(&work, Vec::new()).unwrap();
        let error = EnvironmentSnapshot::capture(
            &local_declaration(),
            None,
            None,
            BTreeMap::new(),
            &roots,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code(), codes::WORKDIR_REQUIRED);
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn capture_refuses_an_undeclared_or_remote_environment() {
        let work = tempdir();
        let roots = WorkspaceRoots::new(&work, Vec::new()).unwrap();
        let error = EnvironmentSnapshot::capture(
            &local_declaration(),
            Some("nope"),
            Some(&work),
            BTreeMap::new(),
            &roots,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code(), codes::ENVIRONMENT_NOT_FOUND);

        let config: super::super::EnvironmentsConfig = serde_json::from_value(
            serde_json::json!({ "dev": { "type": "ssh", "host": "example.test" } }),
        )
        .unwrap();
        let declaration = config.validate().unwrap();
        let error = EnvironmentSnapshot::capture(
            &declaration,
            Some("dev"),
            Some(&work),
            BTreeMap::new(),
            &roots,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code(), codes::ENVIRONMENT_UNAVAILABLE);
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn restore_uses_the_persisted_cwd_and_keeps_the_stored_env() {
        let work = tempdir();
        let roots = WorkspaceRoots::new(&work, Vec::new()).unwrap();
        let binding = EnvironmentBinding {
            environment_id: "local".into(),
            cwd: Some(fs::canonicalize(&work).unwrap()),
        };
        let stored = BTreeMap::from([("FOO".to_string(), "stored".to_string())]);
        let outcome = EnvironmentSnapshot::restore(
            &local_declaration(),
            &binding,
            Some(stored.clone()),
            &roots,
            false,
        )
        .unwrap();
        assert_eq!(outcome.snapshot.env(), &stored);
        assert_eq!(outcome.snapshot.cwd(), fs::canonicalize(&work).unwrap());
        assert!(outcome.drift.is_empty());
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn restore_reports_a_removed_cwd_instead_of_relocating() {
        let work = tempdir();
        let gone = work.join("gone");
        fs::create_dir_all(&gone).unwrap();
        let binding = EnvironmentBinding {
            environment_id: "local".into(),
            cwd: Some(fs::canonicalize(&gone).unwrap()),
        };
        fs::remove_dir_all(&gone).unwrap();
        let roots = WorkspaceRoots::new(&work, Vec::new()).unwrap();
        let error =
            EnvironmentSnapshot::restore(&local_declaration(), &binding, None, &roots, false)
                .unwrap_err();
        assert_eq!(error.code(), codes::WORKDIR_NOT_FOUND);
        fs::remove_dir_all(&work).ok();
    }
}
