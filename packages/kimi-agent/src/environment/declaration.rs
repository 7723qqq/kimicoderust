//! `[environments]` declarations.
//!
//! Upstream `remote/declaration/remoteEnvironmentDeclaration.ts` declares
//! remote execution targets (ssh / docker / command) and validates them when
//! the config section loads. The fork has no remote transport, so the only
//! environment this engine can actually serve is the built-in `local` one —
//! but declaring a remote target must still fail at the right moment and with
//! the right code, instead of being silently treated as local.
//!
//! This module therefore owns two things:
//!
//! - parsing and validating the section, including the reserved ids, the
//!   closed transport union (`ssh`/`docker`/`command` — `local` is built in and
//!   cannot be declared) and the "`default` must reference a configured entry"
//!   rule, so a typo is reported when the config is read;
//! - resolving an id (or the configured default) to either the local
//!   environment or a typed `environment.unavailable` failure naming the
//!   transport the declaration asked for.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{EnvironmentError, codes};

/// Reserved ids that may not be declared as entries (upstream
/// `RESERVED_ENVIRONMENT_IDS`).
pub const RESERVED_ENVIRONMENT_IDS: [&str; 2] = ["local", "default"];

/// Upstream `ENVIRONMENT_ID_MAX_LENGTH`.
pub const ENVIRONMENT_ID_MAX_LENGTH: usize = 64;

/// The transports a declaration may name (upstream
/// `RemoteEnvironmentEntrySchema`'s closed union). `local` is built in and is
/// not a declarable type, so it is deliberately absent.
pub const DECLARED_ENVIRONMENT_TRANSPORTS: [&str; 3] = ["ssh", "docker", "command"];

/// The built-in environment id every session falls back to.
pub const LOCAL_ENVIRONMENT_ID: &str = "local";

/// The `[environments]` section.
///
/// `default` is a sibling of the entries, so the entry map is captured with
/// `flatten` — the same catchall shape upstream uses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentsConfig {
    /// Id used when a session names no environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Every other key is a declared environment entry.
    #[serde(flatten)]
    pub entries: BTreeMap<String, EnvironmentEntry>,
}

/// One declared environment. Declarations are remote by definition: the built-in
/// local environment needs no entry, and `type = "local"` is refused while
/// loading rather than treated as a transport this engine lacks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentEntry {
    /// `ssh`, `docker` or `command` (upstream's closed union). A missing type,
    /// the built-in `local`, or any other value is refused while loading.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Directory used when the binding carries no cwd.
    #[serde(
        rename = "defaultCwd",
        alias = "default_cwd",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub default_cwd: Option<String>,
    /// Environment variables the transport contributes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Transport-specific fields (`host`, `container`, `context`, `command`,
    /// `args`, `remoteBin`). Kept raw: they belong to a transport this engine
    /// does not implement, and dropping them would make a round-trip through
    /// the config layer lossy.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// What a selected environment can actually do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentKind {
    /// The process's own filesystem and shell.
    Local,
    /// A declared transport this engine cannot serve.
    Remote,
}

/// A validated declaration set.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvironmentDeclaration {
    default: Option<String>,
    entries: BTreeMap<String, EnvironmentEntry>,
}

impl Default for EnvironmentDeclaration {
    fn default() -> Self {
        Self {
            default: None,
            entries: BTreeMap::new(),
        }
    }
}

/// The outcome of resolving one id: what to use, and why it is or is not
/// usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEnvironment {
    pub id: String,
    pub kind: EnvironmentKind,
    pub default_cwd: Option<String>,
    pub env: BTreeMap<String, String>,
}

impl EnvironmentsConfig {
    /// Validate the section.
    ///
    /// A bad id and a dangling `default` are both refused here rather than at
    /// first use: upstream validates the section while loading the config, so
    /// a broken declaration never silently degrades to local execution.
    pub fn validate(&self) -> Result<EnvironmentDeclaration, EnvironmentError> {
        for (id, entry) in &self.entries {
            if let Some(problem) = environment_id_problem(id) {
                return Err(EnvironmentError::new(
                    codes::ENVIRONMENT_DECLARATION_INVALID,
                    format!("environment id '{id}' {problem}"),
                ));
            }
            if entry.kind.as_deref().is_none_or(str::is_empty) {
                return Err(EnvironmentError::new(
                    codes::ENVIRONMENT_DECLARATION_INVALID,
                    format!("environment '{id}' has no type"),
                ));
            }
            // Upstream's entry union is closed: `type` must be a transport it
            // knows (`ssh`/`docker`/`command`). `local` is built in and cannot
            // be declared, and any other value is a typo rather than an
            // unreachable host — both are config errors, not
            // `environment.unavailable`.
            let kind = entry.kind.as_deref().unwrap_or_default();
            if kind == LOCAL_ENVIRONMENT_ID {
                return Err(EnvironmentError::new(
                    codes::ENVIRONMENT_DECLARATION_INVALID,
                    format!(
                        "environment '{id}' declares type '{LOCAL_ENVIRONMENT_ID}', which is built in and cannot be declared"
                    ),
                ));
            }
            if !DECLARED_ENVIRONMENT_TRANSPORTS.contains(&kind) {
                return Err(EnvironmentError::new(
                    codes::ENVIRONMENT_DECLARATION_INVALID,
                    format!(
                        "environment '{id}' has type '{kind}', expected one of: {}",
                        DECLARED_ENVIRONMENT_TRANSPORTS.join(", ")
                    ),
                ));
            }
        }
        if let Some(default) = &self.default {
            if !self.entries.contains_key(default) {
                return Err(EnvironmentError::new(
                    codes::ENVIRONMENT_DECLARATION_INVALID,
                    format!("environments.default references unconfigured environment '{default}'"),
                ));
            }
        }
        Ok(EnvironmentDeclaration {
            default: self.default.clone(),
            entries: self.entries.clone(),
        })
    }
}

/// Upstream `environmentIdProblem`: the same four rules in the same order.
#[must_use]
pub fn environment_id_problem(id: &str) -> Option<String> {
    if id.is_empty() {
        return Some("must not be empty".into());
    }
    if id != id.trim() {
        return Some("must not have leading or trailing whitespace".into());
    }
    if id.chars().count() > ENVIRONMENT_ID_MAX_LENGTH {
        return Some(format!(
            "must be at most {ENVIRONMENT_ID_MAX_LENGTH} characters"
        ));
    }
    if RESERVED_ENVIRONMENT_IDS.contains(&id) {
        return Some(format!(
            "is reserved ({})",
            RESERVED_ENVIRONMENT_IDS.join(", ")
        ));
    }
    None
}

impl EnvironmentDeclaration {
    /// Build a declaration from a config section.
    pub fn from_config(config: &EnvironmentsConfig) -> Result<Self, EnvironmentError> {
        config.validate()
    }

    /// The configured default id, if any.
    #[must_use]
    pub fn default_id(&self) -> Option<&str> {
        self.default.as_deref()
    }

    #[must_use]
    pub fn entries(&self) -> &BTreeMap<String, EnvironmentEntry> {
        &self.entries
    }

    /// Resolve `requested`, falling back to `default` and then `local`.
    ///
    /// A declared entry is always a remote transport: selecting one fails with
    /// `environment.unavailable` and names the transport, rather than pretending
    /// this engine can run there.
    pub fn resolve(
        &self,
        requested: Option<&str>,
    ) -> Result<ResolvedEnvironment, EnvironmentError> {
        let requested = match requested {
            Some(id) if !id.is_empty() => id,
            _ => self.default.as_deref().unwrap_or(LOCAL_ENVIRONMENT_ID),
        };
        if requested == "default" {
            return self.resolve(self.default.as_deref());
        }
        if requested == LOCAL_ENVIRONMENT_ID {
            return Ok(ResolvedEnvironment {
                id: LOCAL_ENVIRONMENT_ID.to_string(),
                kind: EnvironmentKind::Local,
                default_cwd: None,
                env: BTreeMap::new(),
            });
        }
        let Some(entry) = self.entries.get(requested) else {
            return Err(EnvironmentError::new(
                codes::ENVIRONMENT_NOT_FOUND,
                format!("environment '{requested}' is not declared"),
            ));
        };
        Err(EnvironmentError::new(
            codes::ENVIRONMENT_UNAVAILABLE,
            format!(
                "environment '{requested}' uses transport '{}', which this engine does not implement",
                entry.kind.as_deref().unwrap_or("unknown")
            ),
        ))
    }

    /// Read a declared entry by id.
    ///
    /// Only declared (remote) entries are present: the built-in local
    /// environment has no entry to read.
    #[must_use]
    pub fn entry(&self, id: &str) -> Option<&EnvironmentEntry> {
        self.entries.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> EnvironmentDeclaration {
        EnvironmentDeclaration::default()
    }

    #[test]
    fn an_empty_section_resolves_to_local() {
        let resolved = local().resolve(None).unwrap();
        assert_eq!(resolved.id, LOCAL_ENVIRONMENT_ID);
        assert_eq!(resolved.kind, EnvironmentKind::Local);
    }

    #[test]
    fn reserved_and_malformed_ids_are_refused_while_loading() {
        for id in ["local", "default", "", " padded", &"x".repeat(65)] {
            let config = EnvironmentsConfig {
                default: None,
                entries: BTreeMap::from([(
                    id.to_string(),
                    EnvironmentEntry {
                        kind: Some("ssh".into()),
                        ..EnvironmentEntry::default()
                    },
                )]),
            };
            let error = config.validate().unwrap_err();
            assert_eq!(error.code(), codes::ENVIRONMENT_DECLARATION_INVALID);
        }
    }

    #[test]
    fn declaring_the_builtin_local_environment_is_refused_while_loading() {
        let config = EnvironmentsConfig {
            default: None,
            entries: BTreeMap::from([(
                "dev".to_string(),
                EnvironmentEntry {
                    kind: Some(LOCAL_ENVIRONMENT_ID.into()),
                    ..EnvironmentEntry::default()
                },
            )]),
        };
        let error = config.validate().unwrap_err();
        assert_eq!(error.code(), codes::ENVIRONMENT_DECLARATION_INVALID);
        assert!(error.detail().contains("'local'"), "{}", error.detail());
        assert!(error.detail().contains("built in"), "{}", error.detail());
    }

    #[test]
    fn an_unknown_transport_is_refused_while_loading() {
        let config = EnvironmentsConfig {
            default: None,
            entries: BTreeMap::from([(
                "dev".to_string(),
                EnvironmentEntry {
                    kind: Some("podman".into()),
                    ..EnvironmentEntry::default()
                },
            )]),
        };
        let error = config.validate().unwrap_err();
        assert_eq!(error.code(), codes::ENVIRONMENT_DECLARATION_INVALID);
        assert!(error.detail().contains("podman"), "{}", error.detail());
        assert!(
            error.detail().contains("ssh, docker, command"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn a_default_must_reference_a_declared_entry() {
        let config = EnvironmentsConfig {
            default: Some("missing".into()),
            entries: BTreeMap::new(),
        };
        let error = config.validate().unwrap_err();
        assert_eq!(error.code(), codes::ENVIRONMENT_DECLARATION_INVALID);
        assert!(error.detail().contains("missing"));
    }

    #[test]
    fn a_declared_remote_transport_is_unavailable_not_local() {
        let config = EnvironmentsConfig {
            default: Some("dev".into()),
            entries: BTreeMap::from([(
                "dev".to_string(),
                EnvironmentEntry {
                    kind: Some("ssh".into()),
                    default_cwd: Some("/srv/app".into()),
                    ..EnvironmentEntry::default()
                },
            )]),
        };
        let declaration = config.validate().unwrap();

        let error = declaration.resolve(None).unwrap_err();
        assert_eq!(error.code(), codes::ENVIRONMENT_UNAVAILABLE);
        assert!(error.detail().contains("'dev'"));
        assert!(error.detail().contains("ssh"));

        // An undeclared id is a different failure from an unavailable one.
        let error = declaration.resolve(Some("nope")).unwrap_err();
        assert_eq!(error.code(), codes::ENVIRONMENT_NOT_FOUND);
    }

    #[test]
    fn the_default_keyword_resolves_the_configured_default() {
        let config = EnvironmentsConfig {
            default: Some("dev".into()),
            entries: BTreeMap::from([(
                "dev".to_string(),
                EnvironmentEntry {
                    kind: Some("docker".into()),
                    ..EnvironmentEntry::default()
                },
            )]),
        };
        let declaration = config.validate().unwrap();
        let error = declaration.resolve(Some("default")).unwrap_err();
        assert_eq!(error.code(), codes::ENVIRONMENT_UNAVAILABLE);
        assert!(error.detail().contains("docker"));
    }

    #[test]
    fn transport_specific_fields_survive_parsing() {
        let config: EnvironmentsConfig = serde_json::from_value(serde_json::json!({
            "default": "ci",
            "ci": {
                "type": "docker",
                "container": "builder",
                "defaultCwd": "/work",
                "env": { "CI": "1" }
            }
        }))
        .unwrap();
        let declaration = config.validate().unwrap();
        let entry = declaration.entry("ci").unwrap();
        assert_eq!(entry.kind.as_deref(), Some("docker"));
        assert_eq!(entry.default_cwd.as_deref(), Some("/work"));
        assert_eq!(entry.env.get("CI").map(String::as_str), Some("1"));
        assert_eq!(
            entry.extra.get("container").and_then(|v| v.as_str()),
            Some("builder")
        );
    }
}
