use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::test_scope_guard::{
    TEST_SCOPE_GUARD_ENV, TestScopeInput, parse_boolean_env, repository_root_of, slashes,
    whole_suite_run_warning,
};

pub struct AgentTestScopeGuardService {
    enabled: bool,
    cwd: String,
    repository_root: String,
    warned: AtomicBool,
    pending: Mutex<Option<String>>,
}

impl AgentTestScopeGuardService {
    #[must_use]
    pub fn from_env() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(&cwd, std::env::var(TEST_SCOPE_GUARD_ENV).ok().as_deref())
    }

    #[must_use]
    pub fn new(cwd: &Path, env_value: Option<&str>) -> Self {
        Self {
            enabled: parse_boolean_env(env_value) != Some(false),
            cwd: slashes(cwd),
            repository_root: repository_root_of(cwd),
            warned: AtomicBool::new(false),
            pending: Mutex::new(None),
        }
    }

    pub fn observe(&self, command: &str) -> Option<String> {
        if !self.enabled || self.warned.load(Ordering::Acquire) {
            return None;
        }
        let warning = whole_suite_run_warning(&TestScopeInput {
            command: command.to_string(),
            cwd: self.cwd.clone(),
            workspace_root: self.repository_root.clone(),
        })?;
        self.warned.store(true, Ordering::Release);
        *self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(warning.clone());
        Some(warning)
    }

    pub fn take_pending(&self) -> Option<String> {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}
