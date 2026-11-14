use globset::{Glob, GlobBuilder};
use serde_json::Value;

use crate::agent::permission_rules::permission_rules::PermissionRule;
use crate::agent::tool_executor::tool_executor::RunnableToolExecution;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRule {
    pub tool_name: String,
    pub arg_pattern: Option<String>,
}

pub type ParsedPattern = ParsedRule;
pub type ParsedPermissionPattern = ParsedRule;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionRuleMatchStrategy {
    ToolNameOnly,
    MatchesRule,
}

#[derive(Debug, Clone)]
pub struct PermissionRuleMatch {
    pub rule: PermissionRule,
    pub strategy: PermissionRuleMatchStrategy,
    pub has_rule_args: bool,
}

pub struct PermissionRuleMatchInput<'a> {
    pub rule: PermissionRule,
    pub tool_name: &'a str,
    pub execution: &'a RunnableToolExecution,
}

pub fn parse_permission_pattern(pattern: &str) -> Option<ParsedRule> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Some(open_idx) = trimmed.find('(') else {
        return Some(ParsedRule {
            tool_name: trimmed.to_string(),
            arg_pattern: None,
        });
    };
    if !trimmed.ends_with(')') {
        return None;
    }
    let tool_name = trimmed[..open_idx].to_string();
    let arg = trimmed[open_idx + 1..trimmed.len() - 1].to_string();
    if tool_name.is_empty() {
        return None;
    }
    Some(ParsedRule {
        tool_name,
        arg_pattern: if arg.is_empty() { None } else { Some(arg) },
    })
}

pub fn parse_pattern(pattern: &str) -> Option<ParsedPattern> {
    parse_permission_pattern(pattern)
}

pub const LEGACY_TOOL_NAME_ALIASES: &[(&str, &[&str])] =
    &[("read", &["readmediafile", "read_media_file"])];

pub fn match_permission_rule(
    input: PermissionRuleMatchInput<'_>,
) -> Option<PermissionRuleMatch> {
    let parsed = parse_permission_pattern(&input.rule.pattern)?;
    if !rule_matches_tool_name(&parsed.tool_name, input.tool_name) {
        return None;
    }
    let Some(arg_pattern) = parsed.arg_pattern else {
        return Some(PermissionRuleMatch {
            rule: input.rule,
            strategy: PermissionRuleMatchStrategy::ToolNameOnly,
            has_rule_args: false,
        });
    };
    let matches = input
        .execution
        .matches_rule
        .as_ref()
        .is_some_and(|matches_rule| matches_rule(&arg_pattern));
    matches.then_some(PermissionRuleMatch {
        rule: input.rule,
        strategy: PermissionRuleMatchStrategy::MatchesRule,
        has_rule_args: true,
    })
}

fn rule_matches_tool_name(parsed_tool_name: &str, tool_name: &str) -> bool {
    if parsed_tool_name == "*" {
        return true;
    }
    let Some(matcher) = Glob::new(parsed_tool_name)
        .ok()
        .map(|glob| glob.compile_matcher())
    else {
        return false;
    };
    if matcher.is_match(tool_name) {
        return true;
    }
    if tool_name == "Read" {
        return matcher.is_match("ReadMediaFile");
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSubjectKind {
    Path,
    Glob,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSubject {
    pub value: String,
    pub kind: RuleSubjectKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Negation {
    Positive,
    Negated,
}

#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub raw_rule: String,
    tool_glob: Option<globset::GlobMatcher>,
    arg_glob_path: Option<globset::GlobMatcher>,
    arg_glob_plain: Option<globset::GlobMatcher>,
    negation: Negation,
}

impl CompiledRule {
    pub fn compile(raw_rule: &str) -> Option<Self> {
        let parsed = parse_permission_pattern(raw_rule)?;
        let tool_glob = if parsed.tool_name == "*" {
            None
        } else {
            Some(compile_tool_glob(&parsed.tool_name)?)
        };
        let (arg_glob_path, arg_glob_plain, negation) = match parsed.arg_pattern {
            None => (None, None, Negation::Positive),
            Some(ref pat) => {
                let (pattern, negation) = match pat.strip_prefix('!') {
                    Some(rest) => (rest, Negation::Negated),
                    None => (pat.as_str(), Negation::Positive),
                };
                (
                    Some(compile_subject_glob(pattern, true)?),
                    Some(compile_subject_glob(pattern, false)?),
                    negation,
                )
            }
        };
        Some(Self {
            raw_rule: raw_rule.to_string(),
            tool_glob,
            arg_glob_path,
            arg_glob_plain,
            negation,
        })
    }

    fn tool_name_matches(&self, tool_lower: &str) -> bool {
        let Some(matcher) = &self.tool_glob else {
            return true;
        };
        if matcher.is_match(tool_lower) {
            return true;
        }
        LEGACY_TOOL_NAME_ALIASES.iter().any(|(surviving, legacy)| {
            (*surviving == tool_lower && legacy.iter().any(|name| matcher.is_match(*name)))
                || (legacy.contains(&tool_lower) && matcher.is_match(*surviving))
        })
    }

    pub(crate) fn matches(
        &self,
        tool_name: &str,
        subject: Option<&str>,
        kind: RuleSubjectKind,
    ) -> bool {
        let tool_lower = tool_name.to_ascii_lowercase();
        if !self.tool_name_matches(&tool_lower) {
            return false;
        }
        let arg_glob = match kind {
            RuleSubjectKind::Path => &self.arg_glob_path,
            RuleSubjectKind::Glob => &self.arg_glob_plain,
        };
        let hit = match (arg_glob, subject) {
            (None, _) => true,
            (Some(matcher), Some(subj)) => {
                matcher.is_match(subj) || matcher.is_match(strip_leading_dot_slash(subj))
            }
            (Some(_), None) => false,
        };
        match self.negation {
            Negation::Positive => hit,
            Negation::Negated => !hit,
        }
    }
}

fn strip_leading_dot_slash(value: &str) -> &str {
    value.strip_prefix("./").unwrap_or(value)
}

fn compile_subject_glob(pattern: &str, case_insensitive: bool) -> Option<globset::GlobMatcher> {
    GlobBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .ok()
        .map(|glob| glob.compile_matcher())
}

fn compile_tool_glob(tool_name: &str) -> Option<globset::GlobMatcher> {
    Glob::new(&tool_name.to_ascii_lowercase())
        .ok()
        .map(|glob| glob.compile_matcher())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPermissionRule {
    pub kind: &'static str,
    pub rule: String,
}

pub fn compile_rules(
    rules: &[String],
    kind: &'static str,
    invalid: &mut Vec<InvalidPermissionRule>,
) -> Vec<CompiledRule> {
    rules
        .iter()
        .filter_map(|rule| match CompiledRule::compile(rule) {
            Some(compiled) => Some(compiled),
            None => {
                tracing::warn!(
                    kind,
                    rule = %rule,
                    "permission rule failed to compile; it is inert (fail-open)"
                );
                invalid.push(InvalidPermissionRule {
                    kind,
                    rule: rule.clone(),
                });
                None
            }
        })
        .collect()
}

pub fn matches_any_rule(
    rules: &[CompiledRule],
    tool_lower: &str,
    subject: Option<&RuleSubject>,
    subject_variants: &[String],
) -> Option<String> {
    for rule in rules {
        let matched = match subject {
            Some(subject) => subject_variants
                .iter()
                .any(|variant| rule.matches(tool_lower, Some(variant.as_str()), subject.kind)),
            None => rule.matches(tool_lower, None, RuleSubjectKind::Glob),
        };
        if matched {
            return Some(rule.raw_rule.clone());
        }
    }
    None
}

pub fn extract_rule_subject(tool_lower: &str, args: &Value) -> Option<RuleSubject> {
    let string_arg = |name: &str| args.get(name).and_then(Value::as_str).map(str::to_string);
    let glob = |value: String| RuleSubject {
        value,
        kind: RuleSubjectKind::Glob,
    };
    let path = |value: String| RuleSubject {
        value,
        kind: RuleSubjectKind::Path,
    };
    match tool_lower {
        "read" | "write" | "edit" => string_arg("path").map(path),
        "readmediafile" | "read_media_file" => string_arg("path").map(glob),
        "grep" | "glob" => string_arg("pattern").map(glob),
        "fetchurl" | "fetch_url" => string_arg("url").map(glob),
        "websearch" | "web_search" => string_arg("query").map(glob),
        "bash" | "powershell" => string_arg("command").map(glob),
        "skill" => string_arg("skill").or_else(|| string_arg("name")).map(glob),
        "agent" => match string_arg("subagent_type") {
            Some(profile) => Some(glob(profile)),
            None if args.get("fork").and_then(Value::as_bool) == Some(true) => None,
            None => Some(glob(
                crate::tools::agent_tool::DEFAULT_PROFILE_NAME.to_string(),
            )),
        },
        "taskoutput" | "task_output" | "taskstop" | "task_stop" => string_arg("task_id").map(glob),
        "taskwait" | "task_wait" | "waitfor" | "wait_for" => Some(glob(
            string_arg("task_id").unwrap_or_else(|| "any".to_string()),
        )),
        "tasklist" | "task_list" => Some(glob(
            match args.get("active_only").and_then(Value::as_bool) {
                Some(false) => "all",
                _ => "active",
            }
            .to_string(),
        )),
        "lsp" => string_arg("path").map(path),
        "memory" => string_arg("path").map(glob),
        _ if crate::agent::tools::github::github_legacy::is_github_tool(tool_lower) => {
            crate::agent::tools::github::github_legacy::rule_subject(tool_lower, args).map(glob)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

    #[test]
    fn test_parse_permission_pattern() {
        let rule = parse_permission_pattern("Write").unwrap();
        assert_eq!(rule.tool_name, "Write");
        assert_eq!(rule.arg_pattern, None);
        let rule = parse_permission_pattern("  Bash  ").unwrap();
        assert_eq!(rule.tool_name, "Bash");
        assert_eq!(rule.arg_pattern, None);
        let rule = parse_permission_pattern("Write(src/*.rs)").unwrap();
        assert_eq!(rule.tool_name, "Write");
        assert_eq!(rule.arg_pattern, Some("src/*.rs".into()));
        let rule = parse_permission_pattern("  Bash ( rm -rf * )  ").unwrap();
        assert_eq!(rule.tool_name, "Bash ");
        assert_eq!(rule.arg_pattern, Some(" rm -rf * ".into()));
        let rule = parse_permission_pattern("Write()").unwrap();
        assert_eq!(rule.tool_name, "Write");
        assert_eq!(rule.arg_pattern, None);
        let rule = parse_permission_pattern("Write(   )").unwrap();
        assert_eq!(rule.tool_name, "Write");
        assert_eq!(rule.arg_pattern, Some("   ".into()));
        assert!(parse_permission_pattern("").is_none());
        assert!(parse_permission_pattern("   ").is_none());
        assert!(parse_permission_pattern("Write(").is_none());
        assert_eq!(parse_permission_pattern(")").unwrap().tool_name, ")");
        assert!(parse_permission_pattern("(arg)").is_none());
        assert!(parse_permission_pattern("  (arg)  ").is_none());
        assert!(parse_permission_pattern("Write(arg)trailing").is_none());
    }

    #[test]
    fn test_compiled_rule_semantics() {
        let path = RuleSubjectKind::Path;
        let rule = CompiledRule::compile("Write").unwrap();
        assert!(rule.matches("write", Some("src/main.rs"), path));
        assert!(rule.matches("write", None, path));
        assert!(rule.matches("WRITE", Some("anything"), path));
        assert!(!rule.matches("read", Some("src/main.rs"), path));
        let rule = CompiledRule::compile("Write(src/*.rs)").unwrap();
        assert!(rule.matches("write", Some("src/main.rs"), path));
        assert!(!rule.matches("write", Some("tests/test.rs"), path));
        assert!(!rule.matches("write", None, path));
        assert!(!rule.matches("read", Some("src/main.rs"), path));
        assert!(rule.matches("write", Some("./src/main.rs"), path));
        let spaced = CompiledRule::compile("Bash (rm -rf *)").unwrap();
        assert!(!spaced.matches("bash", Some("rm -rf *"), RuleSubjectKind::Glob));
        let tight = CompiledRule::compile("Bash(rm -rf *)").unwrap();
        assert!(tight.matches("bash", Some("rm -rf *"), RuleSubjectKind::Glob));
        let rule = CompiledRule::compile("*(*.rs)").unwrap();
        assert!(rule.matches("write", Some("src/main.rs"), path));
        assert!(rule.matches("read", Some("src/main.rs"), path));
        assert!(rule.matches("edit", Some("src/lib.rs"), path));
        assert!(!rule.matches("write", Some("src/main.py"), path));
        let rule = CompiledRule::compile("Write(src/**)").unwrap();
        assert!(rule.matches("write", Some("src/deep/nested/main.rs"), path));
        assert!(!rule.matches("write", Some("tests/test.rs"), path));
        let rule = CompiledRule::compile("Bash(cargo *)").unwrap();
        assert!(rule.matches("bash", Some("cargo build"), RuleSubjectKind::Glob));
        assert!(!rule.matches("bash", Some("Cargo build"), RuleSubjectKind::Glob));
        assert!(CompiledRule::compile("Write([unclosed").is_none());
        assert!(CompiledRule::compile("").is_none());
    }

    #[test]
    fn test_parse_permission_pattern_keeps_negation() {
        let parsed = parse_permission_pattern("Bash(!rm *)").unwrap();
        assert_eq!(parsed.tool_name, "Bash");
        assert_eq!(parsed.arg_pattern.as_deref(), Some("!rm *"));
    }

#[test]
    fn uncompilable_rules_are_recorded_instead_of_dropped_silently() {
        let mut snapshot = PolicySnapshot::default();
        snapshot.deny_rules = vec!["Bash(".to_string(), "Bash".to_string()];
        snapshot.allow_rules = vec!["[".to_string()];
        let engine = PermissionEngine::new(snapshot);

        assert_eq!(
            engine.invalid_rules(),
            &[
                InvalidPermissionRule {
                    kind: "deny",
                    rule: "Bash(".to_string(),
                },
                InvalidPermissionRule {
                    kind: "allow",
                    rule: "[".to_string(),
                },
            ],
            "the valid `Bash` deny rule still compiles; only the two malformed \
             rules are listed"
        );
    }

#[test]
    fn test_github_subject_rules_match() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            deny_rules: vec!["GitHubCreateIssue(octocat/hello-world)".into()],
            ..Default::default()
        });

        let verdict = engine.evaluate(
            "GitHubCreateIssue",
            &json!({ "owner": "octocat", "repo": "hello-world", "title": "t" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");
        assert_eq!(
            verdict.reason,
            Some("Denied by user rule: GitHubCreateIssue(octocat/hello-world)".into())
        );

        let verdict_other = engine.evaluate(
            "GitHubCreateIssue",
            &json!({ "owner": "other", "repo": "repo", "title": "t" }),
        );
        assert_eq!(verdict_other.decision, VerdictDecision::Ask);
        assert_eq!(verdict_other.policy_name, "FallbackAsk");
        assert_eq!(
            verdict_other.reason,
            Some("Tool execution requires approval: GitHubCreateIssue".into())
        );
    }

#[test]
    fn test_rule_tool_name_is_a_glob_pattern() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            allow_rules: vec!["Bash*".into(), "Read*le".into()],
            ..Default::default()
        });

        let verdict = engine.evaluate("Bash", &json!({ "command": "cargo check" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "UserConfiguredAllow");

        let verdict = engine.evaluate("BashOutput", &json!({ "command": "cargo check" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "UserConfiguredAllow");

        let verdict = engine.evaluate("ReadFile", &json!({ "path": "src/lib.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "UserConfiguredAllow");

        let engine_single = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            allow_rules: vec!["Read?le".into()],
            ..Default::default()
        });
        let verdict = engine_single.evaluate("ReadFile", &json!({ "path": "src/lib.rs" }));
        assert_ne!(verdict.policy_name, "UserConfiguredAllow");

        let verdict = engine.evaluate("Write", &json!({ "path": "src/lib.rs" }));
        assert_ne!(verdict.policy_name, "UserConfiguredAllow");
    }

#[test]
    fn test_path_rules_match_case_insensitively() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            allow_rules: vec!["Write(src/**)".into()],
            ..Default::default()
        });

        let verdict = engine.evaluate("Write", &json!({ "path": "SRC/Main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "UserConfiguredAllow");
    }

#[test]
    fn test_read_and_read_media_file_are_legacy_aliases() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            deny_rules: vec!["Read(secret.txt)".into()],
            ..Default::default()
        });
        let verdict = engine.evaluate("ReadMediaFile", &json!({ "path": "secret.txt" }));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");
        let verdict = engine.evaluate("ReadMediaFile", &json!({ "path": "public.txt" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");

        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            deny_rules: vec!["ReadMediaFile(secret.txt)".into()],
            ..Default::default()
        });
        let verdict = engine.evaluate("Read", &json!({ "path": "secret.txt" }));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");
    }

#[test]
    fn test_argument_rules_match_the_tool_declared_subject() {
        let engine = |rule: &str| {
            PermissionEngine::new(PolicySnapshot {
                mode: PermissionMode::Auto,
                deny_rules: vec![rule.into()],
                ..Default::default()
            })
        };
        let denied = |rule: &str, tool: &str, args: Value| {
            let verdict = engine(rule).evaluate(tool, &args);
            (verdict.decision, verdict.policy_name)
        };

        assert_eq!(
            denied("Grep(src/**)", "Grep", json!({ "pattern": "src/**" })),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );
        assert_eq!(
            denied(
                "Glob(*.rs)",
                "Glob",
                json!({ "pattern": "*.rs", "path": "src" })
            ),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );

        assert_eq!(
            denied("Skill(commit)", "Skill", json!({ "skill": "commit" })),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );
        assert_eq!(
            denied(
                "TaskOutput(task-1)",
                "TaskOutput",
                json!({ "task_id": "task-1" })
            ),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );
        assert_eq!(
            denied("WaitFor(any)", "WaitFor", json!({})),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );
        assert_eq!(
            denied("TaskList(all)", "TaskList", json!({ "active_only": false })),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );
        assert_eq!(
            denied("Agent(coder)", "Agent", json!({ "prompt": "x" })),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );

        assert_eq!(
            denied(
                "Lsp(src/lib.rs)",
                "Lsp",
                json!({ "action": "hover", "path": "src/lib.rs" })
            ),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );
        assert_eq!(
            denied(
                "Memory(global/prefs.md)",
                "Memory",
                json!({ "action": "read", "path": "global/prefs.md" })
            ),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );
    }

#[test]
    fn test_unmatched_argument_rule_and_inapplicable_rule_both_miss() {
        let engine = |rule: &str| {
            PermissionEngine::new(PolicySnapshot {
                mode: PermissionMode::Auto,
                deny_rules: vec![rule.into()],
                ..Default::default()
            })
        };

        let verdict = engine("Skill(commit)").evaluate("Skill", &json!({ "skill": "review" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");

        let verdict = engine("TodoList(done)").evaluate("TodoList", &json!({ "items": ["x"] }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");

        let verdict = engine("TodoList").evaluate("TodoList", &json!({}));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");
    }

#[test]
    fn test_negated_argument_pattern() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            deny_rules: vec!["Bash(!rm *)".into()],
            ..Default::default()
        });

        let verdict = engine.evaluate("Bash", &json!({ "command": "cargo check" }));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");

        let verdict = engine.evaluate("Bash", &json!({ "command": "rm -rf target" }));
        assert_ne!(verdict.policy_name, "UserConfiguredDeny");
    }

#[test]
    fn test_extract_rule_subject() {
        let subject = |tool: &str, args: Value| {
            extract_rule_subject(tool, &args).map(|subject| (subject.value, subject.kind))
        };
        let glob = |value: &str| Some((value.to_string(), RuleSubjectKind::Glob));
        let path = |value: &str| Some((value.to_string(), RuleSubjectKind::Path));

        assert_eq!(
            subject("read", json!({ "path": "src/main.rs" })),
            path("src/main.rs")
        );
        assert_eq!(
            subject("write", json!({ "path": "src/main.rs" })),
            path("src/main.rs")
        );
        assert_eq!(
            subject("edit", json!({ "path": "src/main.rs" })),
            path("src/main.rs")
        );

        assert_eq!(
            subject("grep", json!({ "pattern": "fn main", "path": "src/lib" })),
            glob("fn main")
        );
        assert_eq!(
            subject("glob", json!({ "pattern": "*.rs", "path": "tests" })),
            glob("*.rs")
        );

        assert_eq!(
            subject("fetchurl", json!({ "url": "https://api.test/data" })),
            glob("https://api.test/data")
        );
        assert_eq!(
            subject("fetch_url", json!({ "url": "https://api.test/data" })),
            glob("https://api.test/data")
        );

        assert_eq!(
            subject("websearch", json!({ "query": "rust docs" })),
            glob("rust docs")
        );
        assert_eq!(
            subject("web_search", json!({ "query": "rust docs" })),
            glob("rust docs")
        );

        assert_eq!(
            subject("bash", json!({ "command": "cargo test" })),
            glob("cargo test")
        );
        assert_eq!(
            subject("powershell", json!({ "command": "Get-Service" })),
            glob("Get-Service")
        );

        assert_eq!(
            subject("skill", json!({ "skill": "commit" })),
            glob("commit")
        );
        assert_eq!(
            subject("skill", json!({ "name": "legacy" })),
            glob("legacy")
        );
        assert_eq!(
            subject("taskoutput", json!({ "task_id": "t-1" })),
            glob("t-1")
        );
        assert_eq!(subject("waitfor", json!({})), glob("any"));
        assert_eq!(subject("waitfor", json!({ "task_id": "t-1" })), glob("t-1"));
        assert_eq!(subject("tasklist", json!({})), glob("active"));
        assert_eq!(
            subject("tasklist", json!({ "active_only": false })),
            glob("all")
        );

        assert_eq!(
            subject("lsp", json!({ "path": "src/lib.rs" })),
            path("src/lib.rs")
        );
        assert_eq!(
            subject("memory", json!({ "path": "global/prefs.md" })),
            glob("global/prefs.md")
        );
        assert_eq!(
            subject("readmediafile", json!({ "path": "a.png" })),
            glob("a.png")
        );

        assert_eq!(extract_rule_subject("read", &json!({})), None);
        assert_eq!(extract_rule_subject("read", &json!({ "path": 123 })), None);
        assert_eq!(extract_rule_subject("read", &json!({ "path": true })), None);

        assert_eq!(
            extract_rule_subject("unknown_tool", &json!({ "path": "foo" })),
            None
        );
    }

#[test]
    fn powershell_rules_cover_scripts_without_the_bash_guard() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            session_approvals: vec!["PowerShell(Get-Service)".into()],
            ..Default::default()
        });

        let approved = engine.evaluate("PowerShell", &json!({ "command": "Get-Service" }));
        assert_eq!(approved.decision, VerdictDecision::Allow);
        assert_eq!(approved.policy_name, "SessionApprovalHistory");

        let unapproved = engine.evaluate("PowerShell", &json!({ "command": "Get-Process" }));
        assert_eq!(unapproved.decision, VerdictDecision::Ask);
        assert_eq!(unapproved.policy_name, "FallbackAsk");

        let looks_destructive = engine.evaluate("PowerShell", &json!({ "command": "rm -rf /" }));
        assert_eq!(looks_destructive.decision, VerdictDecision::Ask);
        assert_eq!(looks_destructive.policy_name, "FallbackAsk");
    }
}