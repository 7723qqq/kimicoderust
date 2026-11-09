//! Environment layer: where a session runs, and what it may reach.
//!
//! Upstream v2 splits this across `environment/*`,
//! `app/environmentDeclaration/*`, `session/workdir/*`,
//! `session/workspaceContext/*` and `workspace/workspaceDirs/*`. The fork has
//! no equivalent layer: a session carries a workspace root, and the default
//! workspace resolver is the only place a directory is turned into a path.
//! That leaves three failures unrepresentable — a declared environment that
//! does not exist, a working directory that vanished, and a child agent whose
//! cwd escaped the parent workspace — so each one used to collapse into a
//! generic error or a silently substituted default.
//!
//! This module is that layer. It owns:
//!
//! - [`declaration`] — the `[environments]` config: which ids are declared and
//!   what each one contributes (`kind`, `defaultCwd`, `env`).
//! - [`binding`] — the per-session snapshot fixed at creation and restored on
//!   resume, so a config edit does not silently move a running session.
//! - [`workdir`] — the single place that resolves a directory, realpaths it,
//!   and decides whether it is inside the workspace.
//! - [`workspace`] — the workspace roots plus their revision and change feed.
//! - [`agent`] — per-agent cwd/env inheritance and the parent boundary check.
//! - [`agents_md`] — AGENTS.md presence, readability and change observation.
//!
//! Failure discipline follows [`crate::primitives`]: a read that failed is not
//! a read that was absent, and a wait on a change feed is bounded by
//! [`crate::primitives::Deadline`]. Every error carries a stable `code` so the
//! HTTP/napi boundary can map it without guessing from a message.

pub mod agent;
pub mod agents_md;
pub mod binding;
pub mod declaration;
pub mod workdir;
pub mod workspace;

pub use agent::{AgentEnvironment, AgentEnvironmentRequest};
pub use agents_md::{
    AgentsMdChange, AgentsMdFile, AgentsMdProbe, AgentsMdStatus, AgentsMdUnreadable,
    AgentsMdWarning, AgentsMdWatch,
};
pub use binding::{
    EnvironmentBinding, EnvironmentDrift, EnvironmentSnapshot, RestoreOutcome,
    StoredEnvironmentSnapshot,
};
pub use declaration::{
    EnvironmentDeclaration, EnvironmentEntry, EnvironmentKind, EnvironmentsConfig,
    LOCAL_ENVIRONMENT_ID, ResolvedEnvironment,
};
pub use workspace::{WorkspaceChange, WorkspaceContext, WorkspaceRoots, WorkspaceSubscription};

use std::fmt;

/// Stable error codes. They mirror upstream's typed environment failures and
/// are the only thing a caller may branch on; `detail` is diagnostic text.
pub mod codes {
    /// The requested environment id is neither declared nor built in.
    pub const ENVIRONMENT_NOT_FOUND: &str = "environment.not_found";
    /// The environment is declared with a transport this engine cannot serve.
    pub const ENVIRONMENT_UNAVAILABLE: &str = "environment.unavailable";
    /// The persisted environment no longer matches its declaration.
    pub const ENVIRONMENT_BINDING_CONFLICT: &str = "environment.binding_conflict";
    /// The `[environments]` section is malformed.
    pub const ENVIRONMENT_DECLARATION_INVALID: &str = "environment.declaration_invalid";
    /// The environment snapshot could not be read from or written to the store.
    pub const ENVIRONMENT_STORE_FAILED: &str = "environment.store_failed";
    /// An AGENTS.md candidate exists but cannot be read.
    pub const AGENTS_MD_UNREADABLE: &str = "agents_md.unreadable";
    /// An AGENTS.md candidate changed after the prompt was built.
    pub const AGENTS_MD_CHANGED: &str = "agents_md.changed";
    /// No working directory was supplied and none could be derived.
    ///
    /// v2's code for a request without a working directory
    /// (`agent/prompt/errors.ts`, `PromptErrors.REQUEST_WORK_DIR_REQUIRED`).
    pub const WORKDIR_REQUIRED: &str = "request.work_dir_required";
    /// A relative path was supplied where an absolute one is required.
    ///
    /// v2 rejects a relative environment cwd with `environment.invalid_cwd`
    /// (`environment/environmentDefaults.ts:12-15`).
    pub const WORKDIR_NOT_ABSOLUTE: &str = "environment.invalid_cwd";
    /// The path does not exist.
    ///
    /// v2 `openWorkdir` reports a cwd it cannot stat as
    /// `environment.invalid_cwd` (`workspace/sessionLifecycle/
    /// sessionLifecycleService.ts:398`).
    pub const WORKDIR_NOT_FOUND: &str = "environment.invalid_cwd";
    /// The path exists but is not a directory.
    ///
    /// v2 `openWorkdir` reports a non-directory cwd as
    /// `environment.invalid_cwd` (`sessionLifecycleService.ts:403`).
    pub const WORKDIR_NOT_DIRECTORY: &str = "environment.invalid_cwd";
    /// The path exists but could not be resolved.
    ///
    /// v2 folds an unreadable cwd into `environment.invalid_cwd`
    /// (`sessionLifecycleService.ts:398`).
    pub const WORKDIR_UNREADABLE: &str = "environment.invalid_cwd";
    /// The directory resolves outside every workspace root.
    ///
    /// v2's escape check is `FileSystemErrors.PATH_ESCAPES`
    /// (`environment/environmentWorkspaceView.ts:56`).
    pub const WORKDIR_OUTSIDE_WORKSPACE: &str = "fs.path_escapes";
    /// An additional workspace root does not exist.
    ///
    /// v2 `requireDirectory` reports a missing workspace root as
    /// `FileSystemErrors.PATH_NOT_FOUND`
    /// (`app/workspace/workspaceService.ts:227,237`).
    pub const WORKSPACE_ROOT_NOT_FOUND: &str = "fs.path_not_found";
}

/// One environment-layer failure.
///
/// `code` is part of the engine's observable contract; `detail` names the
/// subject (path, id) and the underlying cause so a log line is actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentError {
    code: &'static str,
    detail: String,
}

impl EnvironmentError {
    #[must_use]
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for EnvironmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for EnvironmentError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_renders_both_the_code_and_the_subject() {
        let error = EnvironmentError::new(codes::WORKDIR_NOT_FOUND, "/tmp/missing");
        assert_eq!(error.code(), codes::WORKDIR_NOT_FOUND);
        assert_eq!(error.to_string(), "environment.invalid_cwd: /tmp/missing");
    }
}
