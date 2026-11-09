//! Test-scope guard, ported from v2 `agent/testScopeGuard` (reference HEAD
//! `a0ccd487`).
//!
//! v2 subscribes to `toolExecutor.hooks.onDidExecuteTool`: when a `Bash` call
//! ran the workspace's whole test suite from the repository root, it notifies
//! the reminder service once per turn with the `test-scope-guard` variant. The
//! notice does not block the call; it asks the model to start from the tests
//! that cover the change and widen only when the change crosses packages.
//!
//! The Rust port keeps the detection rules verbatim ([`whole_suite_run_warning`])
//! and arms one pending reminder per turn, which [`TestScopeGuard::take_pending`]
//! hands to the injection registry. The environment kill switch
//! (`KIMI_CODE_TEST_SCOPE_GUARD`) matches v2: unset means enabled, and only the
//! falsey spellings disable it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use once_cell::sync::Lazy;
use regex::Regex;

/// Environment switch (v2 `TEST_SCOPE_GUARD_ENV`).
pub const TEST_SCOPE_GUARD_ENV: &str = "KIMI_CODE_TEST_SCOPE_GUARD";

/// Injection variant carrying the reminder (v2 `TEST_SCOPE_REMINDER_VARIANT`).
pub const TEST_SCOPE_REMINDER_VARIANT: &str = "test-scope-guard";

/// The narrowing example embedded in the reminder.
pub const TEST_SCOPE_NARROWING_HINT: &str =
    "cd packages/<package> && bun test --isolate <test-file…>";

static WHOLE_SUITE_COMMANDS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"^bun\s+test(?:\s|$)",
        r"^bun\s+(?:--bun\s+)?run\s+(?:test|test:heavy|verify)(?:\s|$)",
        r"^bun\s+(?:\./)?scripts/verify-packages\.mjs(?:\s|$)",
        r"^(?:npm|pnpm|yarn)\s+(?:run\s+)?test(?:\s|$)",
        r"^bun\s+vitest\s+(?:run|watch)(?:\s|$)",
        r"^(?:bunx\s+)?vitest\s+(?:run|watch)(?:\s|$)",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("test-scope pattern compiles"))
    .collect()
});

static NARROWING_FLAGS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:^|\s)(?:--project|--test-name-pattern|--filter|--shard|--dir)(?:[\s=]|$)")
        .expect("narrowing-flag pattern compiles")
});

static COMMAND_SEPARATOR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\s*(?:&&|\|\||[;&|\n])\s*").expect("separator pattern compiles"));

static ENV_ASSIGNMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^[A-Za-z_][A-Za-z0-9_]*=(?:"[^"]*"|'[^']*'|\S*)\s+"#)
        .expect("env-assignment pattern compiles")
});

static LEADING_CD: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*cd\s+([^\s;&|]+)\s*(?:&&|;)").expect("cd pattern compiles"));

static PATH_SEPARATOR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\\/]").expect("path-separator pattern compiles"));

static TEST_FILE_PATH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\.(?:test|spec)\.[cm]?[jt]sx?$").expect("test-file pattern compiles")
});

/// One observed shell command plus the directory context used to decide
/// whether it ran from the repository root (v2 `TestScopeInput`).
#[derive(Debug, Clone)]
pub struct TestScopeInput {
    pub command: String,
    pub cwd: String,
    pub workspace_root: String,
}

/// Normalize a directory for comparison: forward slashes, no trailing slash,
/// lowercased (v2 `normalizeDirectory`).
#[must_use]
pub fn normalize_directory(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// Walk `work_dir` upwards until a directory contains `.git`, returning that
/// directory; fall back to `work_dir` itself (v2 `repositoryRootOf` over
/// `findUpwardRoot`).
#[must_use]
pub fn repository_root_of(work_dir: &Path) -> String {
    let start = std::path::absolute(work_dir).unwrap_or_else(|_| PathBuf::from(work_dir));
    let mut current = start.clone();
    loop {
        if current.join(".git").exists() {
            return slashes(&current);
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return slashes(&start),
        }
    }
}

fn slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Whether the command runs from the workspace (repository) root, either
/// directly or after a leading `cd` (v2 `runsFromWorkspaceRoot`).
#[must_use]
pub fn runs_from_workspace_root(input: &TestScopeInput) -> bool {
    let base = normalize_directory(&input.cwd);
    let root = normalize_directory(&input.workspace_root);
    let stripped = strip_env_assignments(&input.command);
    let Some(captures) = LEADING_CD.captures(&stripped) else {
        return base == root;
    };
    let target = captures
        .get(1)
        .map_or("", |matched| matched.as_str())
        .trim();
    if target.is_empty() || target == "." || target == "./" {
        return base == root;
    }
    let bytes = target.as_bytes();
    let absolute = bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(1) == Some(&b':')
        || target.starts_with('/');
    absolute && normalize_directory(target) == root
}

/// Return the reminder when `input.command` runs the whole suite from the
/// repository root, unless it is narrowed to a package/file or carries a
/// narrowing flag (v2 `wholeSuiteRunWarning`).
#[must_use]
pub fn whole_suite_run_warning(input: &TestScopeInput) -> Option<String> {
    if !runs_from_workspace_root(input) {
        return None;
    }
    for part in COMMAND_SEPARATOR.split(&input.command) {
        let segment = strip_env_assignments(part);
        let Some(matched) = whole_suite_match_length(&segment) else {
            continue;
        };
        if NARROWING_FLAGS.is_match(&segment) {
            continue;
        }
        let first_token = segment
            .get(matched..)
            .unwrap_or_default()
            .trim()
            .split_whitespace()
            .next()
            .unwrap_or_default();
        if first_token.is_empty() || first_token.starts_with('-') {
            return Some(render_reminder(&input.command));
        }
        if PATH_SEPARATOR.is_match(first_token) || TEST_FILE_PATH.is_match(first_token) {
            continue;
        }
        return Some(render_reminder(&input.command));
    }
    None
}

fn whole_suite_match_length(segment: &str) -> Option<usize> {
    WHOLE_SUITE_COMMANDS
        .iter()
        .find_map(|pattern| pattern.find(segment).map(|matched| matched.end()))
}

fn strip_env_assignments(segment: &str) -> String {
    let mut rest = segment.trim().to_string();
    loop {
        let Some(matched) = ENV_ASSIGNMENT.find(&rest) else {
            return rest;
        };
        rest = rest[matched.end()..].to_string();
    }
}

/// The reminder body (v2 `renderReminder`).
#[must_use]
pub fn render_reminder(command: &str) -> String {
    format!(
        "`{}` ran the workspace's test suites from the repository root. All-suites is the CI bar, \
         not the default scope for a change: it costs minutes and tells you little that the \
         affected tests do not. Start from the tests that cover what you touched, then the owning \
         package's suite (`{}`), and widen only when the change crosses packages, alters a shared \
         contract, or the user asks for the full run. This note does not change the completion \
         bar — whatever you scoped to must still run and pass.",
        command.trim(),
        TEST_SCOPE_NARROWING_HINT
    )
}

/// Per-turn guard state: the resolved repository root, whether a warning was
/// already armed this turn, and the pending reminder text.
pub struct TestScopeGuard {
    enabled: bool,
    cwd: String,
    repository_root: String,
    warned: AtomicBool,
    pending: Mutex<Option<String>>,
}

impl TestScopeGuard {
    /// Build from the process working directory (the native tools resolve
    /// relative paths against it, matching v2 `sessionContext.cwd`).
    #[must_use]
    pub fn from_env() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(&cwd, std::env::var(TEST_SCOPE_GUARD_ENV).ok().as_deref())
    }

    /// Build for `cwd`, with an explicit environment value so tests can pin the
    /// switch.
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

    /// Observe one executed `Bash` command. Returns the armed reminder the
    /// first time a whole-suite run is seen this turn.
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

    /// Take the armed reminder, if any, for the injection registry.
    pub fn take_pending(&self) -> Option<String> {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }

    /// Whether the guard is enabled for this turn.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

fn parse_boolean_env(value: Option<&str>) -> Option<bool> {
    let normalized = value?.trim().to_lowercase();
    match normalized.as_str() {
        "" => None,
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(command: &str) -> TestScopeInput {
        TestScopeInput {
            command: command.to_string(),
            cwd: "G:\\repo".into(),
            workspace_root: "G:\\repo".into(),
        }
    }

    #[test]
    fn a_bare_suite_run_from_the_root_warns() {
        let warning = whole_suite_run_warning(&input("bun test")).expect("warning");
        assert!(warning.contains("`bun test`"));
        assert!(warning.contains(TEST_SCOPE_NARROWING_HINT));
    }

    #[test]
    fn package_and_file_scopes_do_not_warn() {
        assert!(whole_suite_run_warning(&input("bun test packages/kimi-agent")).is_none());
        assert!(whole_suite_run_warning(&input("bun test args.test.ts")).is_none());
        assert!(whole_suite_run_warning(&input("bun test packages/a/b.spec.tsx")).is_none());
    }

    #[test]
    fn narrowing_flags_do_not_warn() {
        assert!(whole_suite_run_warning(&input("bun test --filter agent")).is_none());
        assert!(whole_suite_run_warning(&input("bun test --project core")).is_none());
        assert!(whole_suite_run_warning(&input("vitest run --shard=1/2")).is_none());
    }

    #[test]
    fn the_other_whole_suite_spellings_warn() {
        for command in [
            "bun run test",
            "bun run test:heavy",
            "bun run verify",
            "bun scripts/verify-packages.mjs",
            "npm test",
            "pnpm run test",
            "yarn test",
            "bun vitest run",
            "bunx vitest run",
        ] {
            assert!(
                whole_suite_run_warning(&input(command)).is_some(),
                "{command} should warn"
            );
        }
    }

    #[test]
    fn a_leading_cd_changes_the_effective_directory() {
        assert!(whole_suite_run_warning(&input("cd packages/a && bun test")).is_none());
        assert!(whole_suite_run_warning(&input("cd . && bun test")).is_some());
        assert!(whole_suite_run_warning(&input("cd G:\\repo && bun test")).is_some());
        // A POSIX-absolute path is compared verbatim, so it is not the
        // Windows-style repository root recorded in `workspace_root`.
        assert!(whole_suite_run_warning(&input("cd /repo && bun test")).is_none());
    }

    #[test]
    fn env_assignments_are_stripped_before_matching() {
        assert!(whole_suite_run_warning(&input("KIMI_ENV=test bun test")).is_some());
        assert!(whole_suite_run_warning(&input("A=1 B='two words' bun test")).is_some());
    }

    #[test]
    fn a_command_chain_warns_on_the_suite_segment() {
        assert!(whole_suite_run_warning(&input("git fetch && bun test")).is_some());
        assert!(whole_suite_run_warning(&input("cd packages/a && bun test")).is_none());
    }

    #[test]
    fn a_different_working_directory_does_not_warn() {
        let mut scope = input("bun test");
        scope.cwd = "G:\\repo\\packages\\a".into();
        assert!(whole_suite_run_warning(&scope).is_none());
    }

    #[test]
    fn the_guard_warns_once_per_turn() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let guard = TestScopeGuard::new(&root, None);
        assert!(guard.observe("bun test").is_some());
        assert!(guard.observe("bun test").is_none());
        assert!(guard.take_pending().is_some());
        assert!(guard.take_pending().is_none());
    }

    #[test]
    fn the_switch_disables_the_guard() {
        let cwd = std::env::current_dir().unwrap();
        let guard = TestScopeGuard::new(&cwd, Some("off"));
        assert!(!guard.enabled());
        assert!(guard.observe("bun test").is_none());
    }

    #[test]
    fn normalize_directory_is_case_and_separator_insensitive() {
        assert_eq!(normalize_directory("G:\\Repo\\"), "g:/repo");
        assert_eq!(normalize_directory("g:/repo"), "g:/repo");
    }

    #[test]
    fn repository_root_walks_up_to_the_git_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let nested = root.join("packages").join("agent");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(
            normalize_directory(&repository_root_of(&nested)),
            normalize_directory(&slashes(&root))
        );
    }
}
