pub use super::test_scope_guard_service::AgentTestScopeGuardService;
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::{Path, PathBuf};

pub const TEST_SCOPE_GUARD_ENV: &str = "KIMI_CODE_TEST_SCOPE_GUARD";

pub const TEST_SCOPE_REMINDER_VARIANT: &str = "test-scope-guard";

pub const TEST_SCOPE_NARROWING_HINT: &str =
    "cd packages/<package> && bun test --isolate <test-file…>";

const JS_SPACE_CONTENT: &str =
    r"\t\n\x0B\f\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{9}'
            | '\u{A}'
            | '\u{B}'
            | '\u{C}'
            | '\u{D}'
            | '\u{20}'
            | '\u{A0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn js_regex(pattern: &str) -> Result<Regex, regex::Error> {
    let negated_target = format!("[^{JS_SPACE_CONTENT};&|]");
    let not_whitespace = format!("[^{JS_SPACE_CONTENT}]");
    let whitespace = format!("[{JS_SPACE_CONTENT}]");
    let pattern = pattern
        .replace(r"[^\s;&|]", &negated_target)
        .replace(r"\S", &not_whitespace)
        .replace(r"\s", &whitespace);
    Regex::new(&pattern)
}

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
    .map(|pattern| js_regex(pattern).expect("test-scope pattern compiles"))
    .collect()
});

static NARROWING_FLAGS: Lazy<Regex> = Lazy::new(|| {
    js_regex(r"(?:^|\s)(?:--project|--test-name-pattern|--filter|--shard|--dir)(?:[\s=]|$)")
        .expect("narrowing-flag pattern compiles")
});

static COMMAND_SEPARATOR: Lazy<Regex> =
    Lazy::new(|| js_regex(r"\s*(?:&&|\|\||[;&|\n])\s*").expect("separator pattern compiles"));

static ENV_ASSIGNMENT: Lazy<Regex> = Lazy::new(|| {
    js_regex(r#"^[A-Za-z_][A-Za-z0-9_]*=(?:"[^"]*"|'[^']*'|\S*)\s+"#)
        .expect("env-assignment pattern compiles")
});

static LEADING_CD: Lazy<Regex> =
    Lazy::new(|| js_regex(r"^\s*cd\s+([^\s;&|]+)\s*(?:&&|;)").expect("cd pattern compiles"));

static PATH_SEPARATOR: Lazy<Regex> =
    Lazy::new(|| js_regex(r"[\\/]").expect("path-separator pattern compiles"));

static TEST_FILE_PATH: Lazy<Regex> = Lazy::new(|| {
    js_regex(r"\.(?:test|spec)\.[cm]?[jt]sx?$").expect("test-file pattern compiles")
});

#[derive(Debug, Clone)]
pub struct TestScopeInput {
    pub command: String,
    pub cwd: String,
    pub workspace_root: String,
}

#[must_use]
pub fn normalize_directory(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

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

pub(super) fn slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[must_use]
pub fn runs_from_workspace_root(input: &TestScopeInput) -> bool {
    let base = normalize_directory(&input.cwd);
    let root = normalize_directory(&input.workspace_root);
    let stripped = strip_env_assignments(&input.command);
    let Some(captures) = LEADING_CD.captures(&stripped) else {
        return base == root;
    };
    let target = js_trim(captures.get(1).map_or("", |matched| matched.as_str()));
    if target.is_empty() || target == "." || target == "./" {
        return base == root;
    }
    let bytes = target.as_bytes();
    let absolute = bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(1) == Some(&b':')
        || target.starts_with('/');
    absolute && normalize_directory(target) == root
}

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
        let first_token = js_trim(segment.get(matched..).unwrap_or_default())
            .split(is_js_whitespace)
            .find(|token| !token.is_empty())
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
    let mut rest = js_trim(segment).to_string();
    loop {
        let Some(matched) = ENV_ASSIGNMENT.find(&rest) else {
            return rest;
        };
        rest = rest[matched.end()..].to_string();
    }
}

#[must_use]
pub fn render_reminder(command: &str) -> String {
    format!(
        "`{}` ran the workspace's test suites from the repository root. All-suites is the CI bar, \
         not the default scope for a change: it costs minutes and tells you little that the \
         affected tests do not. Start from the tests that cover what you touched, then the owning \
         package's suite (`{}`), and widen only when the change crosses packages, alters a shared \
         contract, or the user asks for the full run. This note does not change the completion \
         bar — whatever you scoped to must still run and pass.",
        js_trim(command),
        TEST_SCOPE_NARROWING_HINT
    )
}

pub(super) fn parse_boolean_env(value: Option<&str>) -> Option<bool> {
    let normalized = js_trim(value?).to_lowercase();
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
        let guard = AgentTestScopeGuardService::new(&root, None);
        assert!(guard.observe("bun test").is_some());
        assert!(guard.observe("bun test").is_none());
        assert!(guard.take_pending().is_some());
        assert!(guard.take_pending().is_none());
    }

    #[test]
    fn the_switch_disables_the_guard() {
        let cwd = std::env::current_dir().unwrap();
        let guard = AgentTestScopeGuardService::new(&cwd, Some("off"));
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
