//! Per-agent working directory and environment.
//!
//! Upstream binds every agent to the environment its session was created
//! with (`agent/environmentBinding/environmentBindingService.ts`): a child
//! agent may narrow its cwd, but it may not switch environments or leave the
//! parent workspace. The fork had no child-level cwd at all, so a subagent
//! either ran in the parent's directory or in whatever the process cwd
//! happened to be — the second is an unnoticed sandbox escape.
//!
//! [`AgentEnvironment::resolve`] is the only constructor. It always inherits
//! the parent's environment id, resolves the child cwd through
//! [`super::workdir`], and refuses anything outside the parent's roots. There
//! is deliberately no "allow escape" flag: a child does not get to widen its
//! own boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::binding::EnvironmentSnapshot;
use super::workdir;
use super::{EnvironmentError, WorkspaceRoots};

/// What a child agent asks for. Everything is optional: the default child
/// inherits the parent unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentEnvironmentRequest {
    /// Narrower working directory, relative or absolute. Resolved against the
    /// parent workspace, never against the process cwd.
    pub cwd: Option<PathBuf>,
    /// Variables that override the parent's values key by key.
    pub env: BTreeMap<String, String>,
}

/// The resolved view a child agent runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEnvironment {
    agent_id: String,
    environment_id: String,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
}

impl AgentEnvironment {
    /// Resolve a child environment against its parent.
    ///
    /// A relative cwd is joined to the parent cwd before resolution, so it can
    /// only ever point deeper inside the workspace. The result is then checked
    /// against the parent roots; an escape is `fs.path_escapes`,
    /// not a clamped path.
    pub fn resolve(
        agent_id: impl Into<String>,
        parent: &EnvironmentSnapshot,
        parent_roots: &WorkspaceRoots,
        request: AgentEnvironmentRequest,
    ) -> Result<Self, EnvironmentError> {
        let agent_id = agent_id.into();
        let raw_cwd = match request.cwd {
            Some(cwd) if cwd.is_absolute() => cwd,
            Some(cwd) => parent.cwd().join(cwd),
            None => parent.cwd().to_path_buf(),
        };
        let cwd = workdir::resolve_within(parent_roots, &raw_cwd, false).map_err(|error| {
            // Keep the refusal readable: name the agent, the requested cwd and
            // the parent it tried to leave, in addition to the shared code.
            if error.code() == super::codes::WORKDIR_OUTSIDE_WORKSPACE {
                EnvironmentError::new(
                    error.code(),
                    format!(
                        "agent '{agent_id}' requested '{}', which is outside the parent workspace '{}'",
                        raw_cwd.display(),
                        parent.cwd().display()
                    ),
                )
            } else {
                error
            }
        })?;

        let mut env = parent.env().clone();
        env.extend(request.env);

        Ok(Self {
            agent_id,
            environment_id: parent.environment_id().to_string(),
            cwd,
            env,
        })
    }

    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Inherited from the session; a child never changes it.
    #[must_use]
    pub fn environment_id(&self) -> &str {
        &self.environment_id
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The explicit overrides this agent runs with (parent plus its own).
    #[must_use]
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// The process environment with this agent's overrides applied.
    ///
    /// Non-Unicode entries are skipped with a warning rather than panicking:
    /// `std::env::vars()` would abort the turn on a single badly-encoded
    /// variable, which is a host problem the agent should not inherit.
    #[must_use]
    pub fn effective_env(&self) -> BTreeMap<String, String> {
        let mut effective = BTreeMap::new();
        for (key, value) in std::env::vars_os() {
            let (Ok(key), Ok(value)) = (key.into_string(), value.into_string()) else {
                continue;
            };
            effective.insert(key, value);
        }
        effective.extend(self.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        effective
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::binding::EnvironmentSnapshot;
    use crate::environment::declaration::EnvironmentDeclaration;
    use std::fs;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kimi-environment-agent-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn parent(work: &Path, env: BTreeMap<String, String>) -> (EnvironmentSnapshot, WorkspaceRoots) {
        let roots = WorkspaceRoots::new(work, Vec::new()).unwrap();
        let snapshot = EnvironmentSnapshot::capture(
            &EnvironmentDeclaration::default(),
            None,
            Some(work),
            env,
            &roots,
            false,
        )
        .unwrap();
        (snapshot, roots)
    }

    #[test]
    fn a_child_with_no_request_inherits_the_parent_unchanged() {
        let work = tempdir();
        let parent_env = BTreeMap::from([("A".to_string(), "1".to_string())]);
        let (parent, roots) = parent(&work, parent_env.clone());
        let child =
            AgentEnvironment::resolve("child", &parent, &roots, AgentEnvironmentRequest::default())
                .unwrap();
        assert_eq!(child.environment_id(), "local");
        assert_eq!(child.cwd(), parent.cwd());
        assert_eq!(child.env(), &parent_env);
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn a_child_may_narrow_its_cwd_and_override_a_variable() {
        let work = tempdir();
        let nested = work.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let (parent, roots) = parent(
            &work,
            BTreeMap::from([("A".to_string(), "parent".to_string())]),
        );
        let child = AgentEnvironment::resolve(
            "child",
            &parent,
            &roots,
            AgentEnvironmentRequest {
                cwd: Some(PathBuf::from("nested")),
                env: BTreeMap::from([("B".to_string(), "child".to_string())]),
            },
        )
        .unwrap();
        assert_eq!(child.cwd(), fs::canonicalize(&nested).unwrap());
        assert_eq!(child.env().get("A").map(String::as_str), Some("parent"));
        assert_eq!(child.env().get("B").map(String::as_str), Some("child"));
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn a_child_may_not_leave_the_parent_workspace() {
        let work = tempdir();
        let outside = tempdir();
        let (parent, roots) = parent(&work, BTreeMap::new());
        let error = AgentEnvironment::resolve(
            "child",
            &parent,
            &roots,
            AgentEnvironmentRequest {
                cwd: Some(outside.clone()),
                env: BTreeMap::new(),
            },
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            crate::environment::codes::WORKDIR_OUTSIDE_WORKSPACE
        );
        assert!(error.detail().contains("child"));
        fs::remove_dir_all(&work).ok();
        fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn a_missing_child_cwd_is_reported_not_created() {
        let work = tempdir();
        let (parent, roots) = parent(&work, BTreeMap::new());
        let error = AgentEnvironment::resolve(
            "child",
            &parent,
            &roots,
            AgentEnvironmentRequest {
                cwd: Some(PathBuf::from("does-not-exist")),
                env: BTreeMap::new(),
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), crate::environment::codes::WORKDIR_NOT_FOUND);
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn the_effective_env_applies_explicit_overrides() {
        let work = tempdir();
        let (parent, roots) = parent(
            &work,
            BTreeMap::from([("KIMI_TEST_OVERRIDE".to_string(), "1".to_string())]),
        );
        let child =
            AgentEnvironment::resolve("child", &parent, &roots, AgentEnvironmentRequest::default())
                .unwrap();
        assert_eq!(
            child
                .effective_env()
                .get("KIMI_TEST_OVERRIDE")
                .map(String::as_str),
            Some("1")
        );
        fs::remove_dir_all(&work).ok();
    }
}
