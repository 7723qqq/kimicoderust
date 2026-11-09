//! Local Permission Engine for Kimi Agent (P26 批 3).
//!
//! Evaluates tool execution permissions locally in Rust based on a
//! `PolicySnapshot` injected from the host per turn.
//!
//! Mirrors the 15-policy chain in `agent-core-v2/src/agent/permissionPolicy/permissionPolicyService.ts`
//! including v2's DangerousCommandAsk mode gating (skipped under auto, and its
//! `[permission] dangerousCommandGuard` off-switch):
//!   1. AutoModeAskUserQuestionDeny
//!   2. UserConfiguredDeny
//!   3. CompactionAuthorizationGuard (irreversible Bash after a compaction)
//!   4. DangerousCommandAsk (skipped in auto; a *dangerous* command asks in
//!      manual and yolo alike, an *unanalyzable* one asks except in yolo;
//!      headless sessions and `dangerousCommandGuard: false` skip the policy)
//!   5. AutoModeApprove
//!   6. SessionApprovalHistory
//!   7. UserConfiguredAsk
//!   8. UserConfiguredAllow
//!   9. SensitiveFileAccessAsk
//!  10. GitControlPathAccessAsk
//!  11. GuardianReview (asynchronous/LLM-backed; in `callbacks::execute_tool`)
//!  12. YoloModeApprove
//!  13. DefaultToolApprove (Read-only tools)
//!  14. GitCwdWriteApprove
//!  15. FallbackAsk

use globset::{Glob, GlobBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use crate::i18n::{LocalizedText, i18n_params};
use crate::native::permission_engine::dangerous_command::{DangerousVerdict, analyze_bash_command};
use crate::turn_loop::types::LLMMessage;

pub mod guardian;

/// The tools that run without an approval prompt in every permission mode
/// (v2 `DEFAULT_APPROVE_TOOLS`, `default-tool-approve.ts`). Both spellings of
/// each multi-word name are listed because [`PermissionEngine::evaluate`]
/// lowercases without squashing underscores, so `ReadMediaFile` and
/// `read_media_file` are different keys. The fork's own read-only extras —
/// `Lsp`, the memory readers and the tower readers — are appended for the same reason
/// their absence prompted: a natively executed read must not fall through to
/// `FallbackAsk`.
const DEFAULT_APPROVE_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "readmediafile",
    "read_media_file",
    "settodolist",
    "set_todo_list",
    "todolist",
    "todo_list",
    "tasklist",
    "task_list",
    "taskoutput",
    "task_output",
    "waitfor",
    "wait_for",
    "cronlist",
    "cron_list",
    "websearch",
    "web_search",
    "fetchurl",
    "fetch_url",
    "agent",
    "agentswarm",
    "agent_swarm",
    "askuserquestion",
    "ask_user_question",
    "notifyuser",
    "notify_user",
    "skill",
    "exitplanmode",
    "exit_plan_mode",
    // v2 `default-tool-approve.ts` lists EnterSpecMode / ExitSpecMode next to
    // the plan pair: the tools only change mode state, and ExitSpecMode's own
    // review gate is the user confirmation.
    "enterspecmode",
    "enter_spec_mode",
    "exitspecmode",
    "exit_spec_mode",
    "creategoal",
    "create_goal",
    "getgoal",
    "get_goal",
    "setgoalbudget",
    "set_goal_budget",
    "updategoal",
    "update_goal",
    "select_tools",
    // Read-only tools v2's list cannot name (they do not exist upstream) but
    // which execute natively here. Leaving them out sent a pure read to
    // `FallbackAsk`, so a read-only call prompted in every non-Yolo mode —
    // the same drift the comment above already documents.
    "lsp",
    "towerstatus",
    "tower_status",
    "towerinbox",
    "tower_inbox",
];

/// v2 `IRREVERSIBLE_COMMAND`
/// (`policies/compaction-authorization-guard.ts:5-6`), verbatim.
static IRREVERSIBLE_COMMAND: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
            r"(?i)\b(git\s+push|git\s+merge|git\s+reset\s+--hard|gh\s+pr\s+merge|gh\s+api\b[\s\S]*?-X\s+(PUT|DELETE|PATCH)|branch\s+protection|ruleset|deploy|kubectl\s+delete|rm\s+-rf|DROP\s+(TABLE|DATABASE)|TRUNCATE\b|DELETE\s+FROM)\b",
        )
        .expect("the irreversible-command regex is a compile-time constant")
});

/// Permission mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    #[default]
    Manual,
    Auto,
    Yolo,
    /// Any mode the engine does not model (v2's `PermissionMode` is only
    /// `manual | yolo | auto`; a host that invents a fourth one — it once sent
    /// `plan` this way — lands here). Tolerant deserialization prevents an
    /// unknown mode from silently dropping the whole policy snapshot (hooks +
    /// rules) at the napi boundary: the permission chain still sees an explicit
    /// mode value, just one it treats as the manual default.
    #[serde(other)]
    Unknown,
}

/// Per-agent permission modes for subagent scopes (v2
/// `IAgentPermissionModeService` per agent plus
/// `IAgentLifecycleService.broadcastPermissionMode`). The session mode is the
/// default every agent inherits; a bound mode is the agent's own value and is
/// never overwritten by a session switch — that is what pins a tower worker
/// to `auto` while the user toggles the session between manual and yolo.
#[derive(Debug, Default)]
pub struct AgentPermissionModes {
    session: RwLock<Option<PermissionMode>>,
    bound: RwLock<HashMap<String, PermissionMode>>,
}

impl AgentPermissionModes {
    /// Record the session mode: the value a mode switch broadcasts to every
    /// agent that has no binding of its own (v2 `broadcastPermissionMode`).
    pub fn set_session_mode(&self, mode: PermissionMode) {
        *self.session.write().unwrap_or_else(|e| e.into_inner()) = Some(mode);
    }

    pub fn session_mode(&self) -> Option<PermissionMode> {
        *self.session.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Bind one agent to `mode` (v2 `IAgentPermissionModeService.setMode`).
    /// A bound agent is skipped by later broadcasts.
    pub fn bind(&self, agent_id: &str, mode: PermissionMode) {
        self.bound
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent_id.to_string(), mode);
    }

    pub fn is_bound(&self, agent_id: &str) -> bool {
        self.bound
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(agent_id)
    }

    /// The mode `agent_id` runs under: its own binding wins over the session
    /// default (v2's restored value taking precedence over the config default).
    pub fn mode_for(&self, agent_id: &str) -> Option<PermissionMode> {
        self.bound
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .copied()
            .or_else(|| self.session_mode())
    }
}

/// Shared handle installed on the permission engine and held by the subagent
/// manager.
pub type SharedAgentPermissionModes = std::sync::Arc<AgentPermissionModes>;

/// A user-configured external hook (v2 `HookDefSchema`): an event name, an
/// optional regex `matcher` (empty = match all), the command to run, an
/// optional timeout in seconds (1-600, default 30), an optional working
/// directory, and optional extra environment variables (merged over the
/// inherited environment).
///
/// Validated like v2's `HookDefSchema.strict()` (`configSection.ts:10-19`):
/// the event must be one of the 20 `HOOK_EVENT_TYPES`, `command`/`cwd` must be
/// non-empty, `timeout` is rejected outside 1-600 instead of clamped, and
/// unknown fields are refused. A bad entry therefore fails the whole `[hooks]`
/// section with a diagnostic instead of silently running a mistyped guardrail.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDef {
    #[serde(deserialize_with = "deserialize_hook_event")]
    pub event: String,
    #[serde(default)]
    pub matcher: String,
    #[serde(deserialize_with = "deserialize_hook_command")]
    pub command: String,
    #[serde(default, deserialize_with = "deserialize_hook_timeout")]
    pub timeout: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_hook_cwd")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<std::collections::HashMap<String, String>>,
}

/// v2 `HOOK_EVENT_TYPES` (`features/externalHooks/internal/types.ts:3-24`).
pub const HOOK_EVENT_TYPES: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "PermissionResult",
    "UserPromptSubmit",
    "UserPromptQueued",
    "TurnStarted",
    "Stop",
    "StopFailure",
    "Interrupt",
    "SessionStart",
    "SessionEnd",
    "SessionHeartbeat",
    "SubagentStart",
    "SubagentStop",
    "TaskStarted",
    "PreCompact",
    "PostCompact",
    "Notification",
];

fn deserialize_hook_event<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let event = String::deserialize(deserializer)?;
    if HOOK_EVENT_TYPES.contains(&event.as_str()) {
        Ok(event)
    } else {
        Err(serde::de::Error::custom(format!(
            "unknown hook event '{event}'; expected one of {}",
            HOOK_EVENT_TYPES.join(", ")
        )))
    }
}

fn deserialize_hook_command<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let command = String::deserialize(deserializer)?;
    if command.trim().is_empty() {
        return Err(serde::de::Error::custom("hook command must not be empty"));
    }
    Ok(command)
}

/// v2 `HookDefSchema` rejects rather than clamps an out-of-range timeout.
fn deserialize_hook_timeout<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let timeout = Option::<u64>::deserialize(deserializer)?;
    match timeout {
        Some(seconds) if !(1..=600).contains(&seconds) => Err(serde::de::Error::custom(format!(
            "hook timeout must be between 1 and 600 seconds, got {seconds}"
        ))),
        other => Ok(other),
    }
}

fn deserialize_hook_cwd<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cwd = Option::<String>::deserialize(deserializer)?;
    match cwd {
        Some(ref dir) if dir.trim().is_empty() => Err(serde::de::Error::custom(
            "hook cwd must not be empty when present",
        )),
        other => Ok(other),
    }
}

/// Snapshot of permission configuration passed from host at step boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicySnapshot {
    #[serde(default)]
    pub mode: PermissionMode,
    #[serde(default)]
    pub deny_rules: Vec<String>,
    #[serde(default)]
    pub ask_rules: Vec<String>,
    #[serde(default)]
    pub allow_rules: Vec<String>,
    #[serde(default)]
    pub session_approvals: Vec<String>,
    #[serde(default)]
    pub git_cwd: Option<String>,
    /// The user's global `[tools]` switch (v2 `tools.enabled` /
    /// `tools.disabled`), resolved by the host from `config.toml`. The engine
    /// intersects it with the advertised tool table and enforces it again
    /// before executing a native call.
    #[serde(default)]
    pub tools_filter: Option<crate::tools::tool_policy::ToolsFilter>,
    /// User-configured external hooks (v2 `[hooks]`). The engine executes
    /// the `PreToolUse` ones before native tool calls (G-6 #6), notifies
    /// the observe-only `PostToolUse` / `PostToolUseFailure` / `PreCompact`
    /// events fire-and-forget, lets `UserPromptSubmit` block or prepend
    /// context, and lets matching `Stop` hooks veto a clean text stop once per turn.
    #[serde(default)]
    pub pre_tool_hooks: Vec<HookDef>,
    /// Why each configured rule exists, keyed by its pattern (v2
    /// `permission.rules[].reason`): echoed in the denial so a refusal can
    /// explain itself. Absent entries fall back to the pattern alone.
    #[serde(default)]
    pub rule_reasons: std::collections::HashMap<String, String>,
    /// Headless session signal (upstream bootstrap `nonInteractive`, set by
    /// `kimi -p`): a run with no human to answer an `ask` verdict skips the
    /// `DangerousCommandAsk` policy (v2 `permissionPolicyService.ts` drops it
    /// from the chain), so the remaining policies decide the command.
    #[serde(default)]
    pub non_interactive: bool,
    /// `[permission] dangerousCommandGuard` (v2
    /// `isDangerousCommandGuardEnabled`, default true): `false` disables every
    /// built-in guard ask policy — DangerousCommandAsk, SensitiveFileAccessAsk
    /// and GitControlPathAccessAsk (v2 #4250) — letting the remaining policies
    /// decide. The hard path-access block is a separate layer and stays on.
    #[serde(default = "crate::permission::default_true")]
    pub dangerous_command_guard: bool,
}

/// `Default` for [`PolicySnapshot`]: the guard is on unless the config
/// explicitly turns it off — matching the serde default and v2's `?? true`.
/// Handwritten (not derived) because a derived `bool` default is `false`.
impl Default for PolicySnapshot {
    fn default() -> Self {
        Self {
            mode: PermissionMode::default(),
            deny_rules: Vec::new(),
            ask_rules: Vec::new(),
            allow_rules: Vec::new(),
            session_approvals: Vec::new(),
            git_cwd: None,
            tools_filter: None,
            pre_tool_hooks: Vec::new(),
            rule_reasons: std::collections::HashMap::new(),
            non_interactive: false,
            dangerous_command_guard: true,
        }
    }
}

/// Serde default for [`PolicySnapshot::dangerous_command_guard`] — the guard
/// is on unless the config explicitly turns it off (v2's `?? true`).
fn default_true() -> bool {
    true
}

/// Verdict returned by the local permission engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPermissionVerdict {
    pub decision: VerdictDecision,
    pub policy_name: String,
    pub reason: Option<String>,
    /// The user-configured ask rule that produced this verdict, when one did.
    ///
    /// This is the candidate for a session approval: approving it for the
    /// session remembers *this rule*, not the whole tool. v2 carries the same
    /// value as `sessionApprovalRule` on its approval record, and the rule is
    /// only a pattern the user already wrote in their own config — nothing is
    /// inferred here.
    pub session_approval_rule: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerdictDecision {
    Allow,
    Deny,
    Ask,
}

impl LocalPermissionVerdict {
    pub fn is_allow(&self) -> bool {
        self.decision == VerdictDecision::Allow
    }
}

/// Parsed permission rule pattern: `ToolName` or `ToolName(argPattern)`.
#[derive(Debug, Clone)]
pub struct ParsedRule {
    pub tool_name: String,
    pub arg_pattern: Option<String>,
}

/// Whether an argument pattern is negated — v2 `matchRuleSubjects` strips a
/// leading `!` and inverts the result (`Bash(!rm *)` matches every command
/// except the ones the inner pattern matches).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Negation {
    Positive,
    Negated,
}

pub fn parse_permission_pattern(pattern: &str) -> Option<ParsedRule> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return None;
    }

    let Some(open_idx) = trimmed.find('(') else {
        if trimmed.contains(')') {
            return None;
        }
        return Some(ParsedRule {
            tool_name: trimmed.to_string(),
            arg_pattern: None,
        });
    };

    if !trimmed.ends_with(')') {
        return None;
    }

    let tool_name = trimmed[..open_idx].trim().to_string();
    let arg = trimmed[open_idx + 1..trimmed.len() - 1].trim();

    if tool_name.is_empty() {
        return None;
    }

    Some(ParsedRule {
        tool_name,
        arg_pattern: if arg.is_empty() {
            None
        } else {
            Some(arg.to_string())
        },
    })
}

/// How a tool's own v2 `matchesRule` compares the rule's argument with the
/// call's subject (`tool/rule-match.ts`). The two strategies differ in more
/// than the value they pick: `matchesPathRuleSubject` folds case and resolves
/// the subject against the cwd (`pathGlobMatch` defaults
/// `caseInsensitivePaths: true`), while `matchesGlobRuleSubject` compares a
/// plain, case-sensitive glob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSubjectKind {
    Path,
    Glob,
}

/// The value a tool's `matchesRule` compares and the strategy it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RuleSubject {
    value: String,
    kind: RuleSubjectKind,
}

/// v2 `LEGACY_TOOL_NAME_ALIASES` (`permissionRules/matchesRule.ts:67-75`): the
/// fold of `ReadMediaFile` into `Read` kept the old spelling matchable, so a
/// rule a user wrote before the fold still constrains the surviving tool.
///
/// v2 only consults the table from the surviving name, because the legacy tool
/// no longer exists there. This engine still answers the legacy spelling (it is
/// in [`DEFAULT_APPROVE_TOOLS`]), so the same table is consulted from that side
/// too: a `Read(...)` deny rule must constrain a `ReadMediaFile` call, not only
/// the other way round. Both spellings of the legacy name are listed because
/// [`PermissionEngine::evaluate`] lowercases without squashing underscores.
const LEGACY_TOOL_NAME_ALIASES: &[(&str, &[&str])] =
    &[("read", &["readmediafile", "read_media_file"])];

/// Precompiled permission rule for zero-allocation fast-path evaluation.
///
/// Mirrors v2 `matchPermissionRule` (`permissionRules/matchesRule.ts`):
/// the tool name is a glob (`*` matches every tool, legacy spellings included),
/// and the argument pattern is compiled under both of v2's strategies so the
/// call site can pick the one its tool declares, plus `!` negation.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub raw_rule: String,
    tool_glob: Option<globset::GlobMatcher>,
    /// The argument pattern as `matchesPathRuleSubject` compares it.
    arg_glob_path: Option<globset::GlobMatcher>,
    /// The argument pattern as `matchesGlobRuleSubject` compares it.
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

    /// v2 `ruleMatchesToolName`: the pattern itself, or — for a tool that was
    /// folded into another — either spelling of the alias pair.
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

    #[inline]
    fn matches(&self, tool_name: &str, subject: Option<&str>, kind: RuleSubjectKind) -> bool {
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
            // An argument rule cannot be satisfied by a tool that declares no
            // subject: v2's `execution.matchesRule?.(...)` is `undefined`.
            (Some(_), None) => false,
        };
        match self.negation {
            Negation::Positive => hit,
            Negation::Negated => !hit,
        }
    }
}

/// v2 `stripLeadingDotSlash` (`tool/rule-match.ts`): `./src/lib.rs` and
/// `src/lib.rs` are the same target for rule matching.
fn strip_leading_dot_slash(value: &str) -> &str {
    value.strip_prefix("./").unwrap_or(value)
}

/// Compile a glob the way v2 compiles rule patterns. `pathGlobMatch` defaults
/// `caseInsensitivePaths` to true, so a path rule matches a differently-cased
/// subject on any platform; `globMatch` (the plain strategy) keeps case.
fn compile_subject_glob(pattern: &str, case_insensitive: bool) -> Option<globset::GlobMatcher> {
    // `*` stays cross-directory, like picomatch's default in v2, so `*.rs`
    // matches `src/main.rs` in a rule and `rm -rf *` matches `rm -rf /`.
    GlobBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .ok()
        .map(|glob| glob.compile_matcher())
}

/// Compile a tool-name glob (v2 `picomatch.isMatch(toolName, parsed.toolName)`).
fn compile_tool_glob(tool_name: &str) -> Option<globset::GlobMatcher> {
    Glob::new(&tool_name.to_ascii_lowercase())
        .ok()
        .map(|glob| glob.compile_matcher())
}

/// A permission rule that failed to compile and is therefore inert.
///
/// Fail-open is the deliberate choice: a single malformed deny rule must not
/// make every tool call fail. The rule is still surfaced through
/// [`PermissionEngine::invalid_rules`] and a `warn!` at construction, so the
/// degradation is visible rather than silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPermissionRule {
    /// Which rule list it came from: `deny`, `ask`, `allow` or `session`.
    pub kind: &'static str,
    /// The raw rule text as the snapshot carried it.
    pub rule: String,
}

/// Local permission engine evaluating tool calls against a `PolicySnapshot`.
///
/// The mode is interior-mutable ([`PermissionEngine::set_mode`]) so an
/// interactive entry can switch it live — the REPL's `/yolo`, which previously
/// only mutated config for the next restart (the M1a limitation). Everything
/// else about the snapshot (rules, hooks) stays fixed; hosts that need a
/// different rule set rebuild the engine instead.
pub struct PermissionEngine {
    snapshot: PolicySnapshot,
    mode: RwLock<PermissionMode>,
    compiled_deny: Vec<CompiledRule>,
    compiled_ask: Vec<CompiledRule>,
    compiled_allow: Vec<CompiledRule>,
    compiled_session: Vec<CompiledRule>,
    /// Rules the user approved "for this session" while this engine was
    /// running. The snapshot carries the ones a host restored; this list is
    /// what the engine records itself, so the approval survives a host that
    /// keeps no copy of its own (and works for a UI that has no host to ask).
    session_records: RwLock<Vec<CompiledRule>>,
    /// Rules the engine could not compile. They stay inert: dropping a
    /// malformed rule is fail-open by design (one bad pattern must not lock
    /// every tool), but the drop is recorded and warned about instead of
    /// vanishing — otherwise a user cannot tell why a rule "doesn't work".
    invalid_rules: Vec<InvalidPermissionRule>,
    /// The session workspace root (v2 `ISessionWorkspaceContext.workDir`) and
    /// the host-authorized `additionalDirs` — `GitCwdWriteApprove`'s
    /// containment gate. Empty for an engine built without them; that policy
    /// then falls back to the snapshot's `git_cwd` alone.
    workspace_root: Option<String>,
    additional_dirs: Vec<String>,
    /// The live session history (v2's `IAgentContextMemoryService`) that
    /// history-dependent policies read: `CompactionAuthorizationGuard` checks
    /// it for a compaction summary, and the guardian review takes its
    /// transcript from it. Installed once after construction by the entry
    /// point that owns the session; without it those policies never fire.
    conversation: OnceLock<ConversationProvider>,
    /// The guardian review service, installed by the pipeline builder once the
    /// LLM exists (v2's DI-injected `IAgentGuardianService`).
    guardian: OnceLock<std::sync::Arc<guardian::GuardianReviewer>>,
    /// Per-agent permission modes (v2 `IAgentPermissionModeService` per agent
    /// scope). Installed by the entry point that owns the session; without it
    /// every tool call evaluates under the engine's own mode.
    agent_modes: OnceLock<SharedAgentPermissionModes>,
}

/// Reads the session's current message history. Called per permission
/// evaluation, so a mid-turn compaction is visible to the next tool call
/// (v2 reads its context memory live in the same way).
pub type ConversationProvider = std::sync::Arc<dyn Fn() -> Vec<LLMMessage> + Send + Sync>;

impl PermissionEngine {
    pub fn new(snapshot: PolicySnapshot) -> Self {
        Self::with_workspace(snapshot, None, Vec::new())
    }

    /// Build with the host's workspace roots: `workspace_root` is the session
    /// workDir and `additional_dirs` the `/add-dir` list (`PipelineSpec`'s
    /// `workspace_root` / `extra_roots`). `GitCwdWriteApprove` is the only
    /// policy that reads them; without them it falls back to
    /// [`PolicySnapshot::git_cwd`], which the host sets to the same workDir.
    pub fn with_workspace(
        snapshot: PolicySnapshot,
        workspace_root: Option<String>,
        additional_dirs: Vec<String>,
    ) -> Self {
        let mut invalid_rules = Vec::new();
        let compiled_deny = Self::compile_rules(&snapshot.deny_rules, "deny", &mut invalid_rules);
        let compiled_ask = Self::compile_rules(&snapshot.ask_rules, "ask", &mut invalid_rules);
        let compiled_allow =
            Self::compile_rules(&snapshot.allow_rules, "allow", &mut invalid_rules);
        let compiled_session =
            Self::compile_rules(&snapshot.session_approvals, "session", &mut invalid_rules);

        let mode = snapshot.mode;
        Self {
            snapshot,
            mode: RwLock::new(mode),
            compiled_deny,
            compiled_ask,
            compiled_allow,
            compiled_session,
            session_records: RwLock::new(Vec::new()),
            invalid_rules,
            workspace_root,
            additional_dirs,
            conversation: OnceLock::new(),
            guardian: OnceLock::new(),
            agent_modes: OnceLock::new(),
        }
    }

    /// Compile one rule bucket, recording (and warning about) every rule whose
    /// pattern is malformed. A malformed rule is dropped — fail-open, so one
    /// typo cannot lock every tool — but never silently.
    fn compile_rules(
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

    /// Rules whose pattern did not compile. They are inert; this is the
    /// observable count a host or test can surface instead of the drop being
    /// invisible.
    pub fn invalid_rules(&self) -> &[InvalidPermissionRule] {
        &self.invalid_rules
    }

    /// Record `rule` as approved for the rest of this session.
    ///
    /// Returns whether the engine learned something: a rule the snapshot
    /// already carries, one this engine already recorded, or a pattern that
    /// does not compile changes nothing. The user's own rule is what is
    /// remembered — approving "Bash" for a session would grant every later
    /// Bash call, including the dangerous ones the rule was written to catch.
    pub fn remember_session_approval(&self, rule: &str) -> bool {
        let rule = rule.trim();
        if rule.is_empty() {
            return false;
        }
        if self.compiled_session.iter().any(|r| r.raw_rule == rule) {
            return false;
        }
        let Some(compiled) = CompiledRule::compile(rule) else {
            return false;
        };
        let mut records = self
            .session_records
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if records.iter().any(|r| r.raw_rule == rule) {
            return false;
        }
        records.push(compiled);
        true
    }

    /// The rules this engine recorded itself, in approval order.
    pub fn session_approval_rules(&self) -> Vec<String> {
        self.session_records
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|r| r.raw_rule.clone())
            .collect()
    }

    /// Evaluate permission for a tool call.
    /// The permission mode of the snapshot (G-6 #7: the goal-start review
    /// gate reads it to decide whether CreateGoal routes to the host).
    pub fn mode(&self) -> PermissionMode {
        *self.mode.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Switch the permission mode live. The REPL's `/yolo` and `/permission`
    /// use this instead of asking the user to restart; every subsequent
    /// [`PermissionEngine::evaluate`] sees the new mode at its own chain arm.
    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.write().unwrap_or_else(|e| e.into_inner()) = mode;
    }

    /// Install the per-agent permission mode registry (v2
    /// `IAgentPermissionModeService` per agent scope). Set once by the entry
    /// point that owns the session, alongside the subagent manager that shares
    /// the same handle.
    pub fn set_agent_permission_modes(&self, modes: SharedAgentPermissionModes) {
        let _ = self.agent_modes.set(modes);
    }

    pub fn agent_permission_modes(&self) -> Option<&SharedAgentPermissionModes> {
        self.agent_modes.get()
    }

    /// Install the live conversation reader. Set once: a session's history
    /// lives for the session's lifetime, so a second provider would be a
    /// mistake, not an update.
    pub fn set_conversation_provider(&self, provider: ConversationProvider) {
        let _ = self.conversation.set(provider);
    }

    /// The session history as of this call. Empty when no provider is
    /// installed (the engine then behaves as a context-free policy chain).
    pub fn conversation_snapshot(&self) -> Vec<LLMMessage> {
        self.conversation
            .get()
            .map(|provider| provider())
            .unwrap_or_default()
    }

    /// Whether the session currently carries a compaction summary (v2
    /// `CompactionAuthorizationGuard`: `message.origin?.kind ===
    /// 'compaction_summary'`).
    pub fn has_compaction_summary(&self) -> bool {
        self.conversation.get().is_some_and(|provider| {
            provider()
                .iter()
                .any(crate::compaction::is_compaction_summary_message)
        })
    }

    /// Install the guardian review service (v2 DI registration).
    pub fn set_guardian_reviewer(&self, reviewer: std::sync::Arc<guardian::GuardianReviewer>) {
        let _ = self.guardian.set(reviewer);
    }

    pub fn guardian_reviewer(&self) -> Option<&std::sync::Arc<guardian::GuardianReviewer>> {
        self.guardian.get()
    }

    /// The workspace cwd `GitCwdWriteApprove` resolves against — v2's
    /// `ISessionWorkspaceContext.workDir`, which both `git-cwd-write-approve.ts`
    /// and its `findWorkTree(cwd)` gate read. The host-set `workspace_root`
    /// wins; the stdio entry carries only the snapshot's `git_cwd`, which the
    /// host sets to the same directory. An empty value is no cwd at all (v2's
    /// `cwd.length === 0` early return).
    fn workspace_dir(&self) -> Option<&str> {
        self.workspace_root
            .as_deref()
            .or(self.snapshot.git_cwd.as_deref())
            .filter(|dir| !dir.is_empty())
    }

    pub fn evaluate(&self, tool_name: &str, args: &Value) -> LocalPermissionVerdict {
        self.evaluate_in_mode(tool_name, args, self.mode())
    }

    /// Evaluate as one agent runs: a bound mode (tower worker pinned to
    /// `auto`) or the inherited session mode wins over the engine's own mode
    /// (v2 `IAgentPermissionModeService` living per agent scope). Called by
    /// the tool-call layer with the executing `agent_id`.
    pub fn evaluate_for_agent(
        &self,
        agent_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> LocalPermissionVerdict {
        match self
            .agent_modes
            .get()
            .and_then(|modes| modes.mode_for(agent_id))
        {
            Some(mode) => self.evaluate_in_mode(tool_name, args, mode),
            None => self.evaluate(tool_name, args),
        }
    }

    fn evaluate_in_mode(
        &self,
        tool_name: &str,
        args: &Value,
        mode: PermissionMode,
    ) -> LocalPermissionVerdict {
        let tool_lower = tool_name.to_ascii_lowercase();
        let subject = extract_rule_subject(&tool_lower, args);
        // Rule matching sees the raw spelling and the lexically resolved path,
        // the same variants v2 `pathVariants` builds (`rule-match.ts:49-60`):
        // a deny rule for `/etc/passwd` must also match `/etc/../etc/passwd`.
        // A glob subject is compared as written. Symlinks are not resolved
        // here — v2 does that once per file-tool execution
        // (`tool/realpath-access.ts`), which this engine mirrors in
        // `tools/file_ops.rs::file_access_refusal`.
        let subject_variants = rule_subject_variants(subject.as_ref(), self.workspace_dir());
        let rule_subjects = subject_variants.variants;
        // Security checks below (`GitControlPathAccessAsk`, sensitive files,
        // workspace containment) use the lexically resolved path, never the raw
        // spelling and never "whichever sorts last".
        let path_subject = subject_variants.resolved;
        let file_access = file_accesses(&tool_lower, args);

        // 1. AutoModeAskUserQuestionDeny
        if mode == PermissionMode::Auto
            && matches!(tool_lower.as_str(), "askuserquestion" | "ask_user_question")
        {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Deny,
                policy_name: "AutoModeAskUserQuestionDeny".into(),
                session_approval_rule: None,
                reason: Some(LocalizedText::new("engine.permission.autoModeCannotAsk").render()),
            };
        }

        // 2. UserConfiguredDeny
        if let Some(rule) = Self::matches_any_rule(
            &self.compiled_deny,
            &tool_lower,
            subject.as_ref(),
            &rule_subjects,
        ) {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Deny,
                policy_name: "UserConfiguredDeny".into(),
                session_approval_rule: None,
                // A declared reason explains the refusal (v2 `reason`).
                reason: Some(
                    match self
                        .snapshot
                        .rule_reasons
                        .get(rule.as_str())
                        .map(String::as_str)
                        .filter(|r| !r.trim().is_empty())
                    {
                        Some(why) => LocalizedText::with_params(
                            "engine.permission.deniedByUserRule",
                            i18n_params!["rule" => rule, "why" => why],
                        )
                        .render(),
                        None => LocalizedText::with_params(
                            "engine.permission.deniedByUserRuleNoWhy",
                            i18n_params!["rule" => rule],
                        )
                        .render(),
                    },
                ),
            };
        }

        // 3. CompactionAuthorizationGuard (v2 `policies/
        //    compaction-authorization-guard.ts:19-24`). A session whose context
        //    was compacted has lost the verbatim record of what the user
        //    authorized; an irreversible Bash command then asks in every mode
        //    — yolo included — instead of riding the compaction summary as if
        //    it were an approval. Inert without a conversation provider.
        if self.has_compaction_summary()
            && tool_lower == "bash"
            && let Some(command) = args.get("command").and_then(Value::as_str)
            && IRREVERSIBLE_COMMAND.is_match(command)
        {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Ask,
                policy_name: "CompactionAuthorizationGuard".into(),
                session_approval_rule: None,
                reason: Some(
                    LocalizedText::new("engine.permission.compactionAuthorizationRequired")
                        .render(),
                ),
            };
        }

        // 4. DangerousCommandAsk (v2 `dangerous-command-ask.ts`, under
        //    `agent/permissionPolicy/policies/`). Mode gating
        //    mirrors v2 exactly, in v2's own order: the whole policy is skipped
        //    in auto (`if (mode === 'auto') return undefined` comes *before* the
        //    verdict), a **dangerous** command asks in every mode that reaches
        //    this point — yolo included — and an **unanalyzable** one asks
        //    except in yolo (v2 #3869's late `if (mode === 'yolo') return
        //    undefined`). Skipped for headless sessions (`non_interactive`,
        //    upstream `permissionPolicyService.ts` drops this ask-policy when
        //    the host cannot answer a prompt), and for
        //    `[permission] dangerousCommandGuard: false` (v2
        //    `isDangerousCommandGuardEnabled`).
        if !self.snapshot.non_interactive
            && self.snapshot.dangerous_command_guard
            && mode != PermissionMode::Auto
            && tool_lower == "bash"
            && let Some(command) = subject.as_ref().map(|subject| subject.value.as_str())
        {
            match analyze_bash_command(command) {
                // yolo is not an exemption for a *known* dangerous command:
                // v2 asks for it before the yolo early-return exists.
                DangerousVerdict::Dangerous(_) => {
                    return LocalPermissionVerdict {
                        decision: VerdictDecision::Ask,
                        policy_name: "DangerousCommandAsk".into(),
                        session_approval_rule: None,
                        reason: Some(
                            LocalizedText::new("engine.permission.highRiskShellCommand").render(),
                        ),
                    };
                }
                DangerousVerdict::Unanalyzable(_) if mode != PermissionMode::Yolo => {
                    return LocalPermissionVerdict {
                        decision: VerdictDecision::Ask,
                        policy_name: "DangerousCommandAsk".into(),
                        session_approval_rule: None,
                        reason: Some(
                            LocalizedText::new("engine.permission.shellCommandUnanalyzable")
                                .render(),
                        ),
                    };
                }
                _ => {}
            }
        }

        // 5. AutoModeApprove
        if mode == PermissionMode::Auto {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Allow,
                policy_name: "AutoModeApprove".into(),
                session_approval_rule: None,
                reason: None,
            };
        }

        // 6. SessionApprovalHistory — the rules a host restored in the snapshot
        //    plus the ones this engine recorded while answering approvals. The
        //    engine-side record is what makes the choice survive a host that
        //    keeps no list of its own.
        let session_rule = {
            let records = self
                .session_records
                .read()
                .unwrap_or_else(|e| e.into_inner());
            Self::matches_any_rule(&records, &tool_lower, subject.as_ref(), &rule_subjects).or_else(
                || {
                    Self::matches_any_rule(
                        &self.compiled_session,
                        &tool_lower,
                        subject.as_ref(),
                        &rule_subjects,
                    )
                },
            )
        };
        if let Some(rule) = session_rule {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Allow,
                policy_name: "SessionApprovalHistory".into(),
                session_approval_rule: None,
                reason: Some(
                    LocalizedText::with_params(
                        "engine.permission.approvedBySessionHistory",
                        i18n_params!["rule" => rule],
                    )
                    .render(),
                ),
            };
        }

        // 7. UserConfiguredAsk
        if let Some(rule) = Self::matches_any_rule(
            &self.compiled_ask,
            &tool_lower,
            subject.as_ref(),
            &rule_subjects,
        ) {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Ask,
                policy_name: "UserConfiguredAsk".into(),
                // The rule that matched is what a session approval would
                // remember, so carry it: without it the host can only offer
                // to allow the whole tool for the session, which grants far
                // more than the rule the user wrote.
                session_approval_rule: Some(rule.to_string()),
                reason: Some(
                    LocalizedText::with_params(
                        "engine.permission.approvalRequiredByUserRule",
                        i18n_params!["rule" => rule],
                    )
                    .render(),
                ),
            };
        }

        // 8. UserConfiguredAllow
        if let Some(rule) = Self::matches_any_rule(
            &self.compiled_allow,
            &tool_lower,
            subject.as_ref(),
            &rule_subjects,
        ) {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Allow,
                policy_name: "UserConfiguredAllow".into(),
                session_approval_rule: None,
                reason: Some(
                    LocalizedText::with_params(
                        "engine.permission.allowedByUserRule",
                        i18n_params!["rule" => rule],
                    )
                    .render(),
                ),
            };
        }

        // 9. SensitiveFileAccessAsk (v2 `sensitive-file-access-ask.ts`, gated
        //    by the same `[permission] dangerousCommandGuard` switch as
        //    DangerousCommandAsk since #4250).
        if self.snapshot.dangerous_command_guard
            && let Some(path) = file_access
                .iter()
                .find(|path| crate::native::file_type::is_sensitive_file(path))
        {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Ask,
                policy_name: "SensitiveFileAccessAsk".into(),
                session_approval_rule: None,
                reason: Some(
                    LocalizedText::with_params(
                        "engine.permission.sensitiveFileAccess",
                        i18n_params!["path" => path],
                    )
                    .render(),
                ),
            };
        }

        // 10. GitControlPathAccessAsk (v2 `git-control-path-access-ask.ts`, same
        //    guard switch).
        if self.snapshot.dangerous_command_guard
            && let Some(path) = file_access.iter().find(|path| is_git_control_path(path))
        {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Ask,
                policy_name: "GitControlPathAccessAsk".into(),
                session_approval_rule: None,
                reason: Some(
                    LocalizedText::with_params(
                        "engine.permission.gitControlPathAccess",
                        i18n_params!["path" => path],
                    )
                    .render(),
                ),
            };
        }

        // 11. GuardianReview (v2 `policies/guardian-review.ts`): asynchronous
        //     and LLM-backed, so it runs in the callback layer between this
        //     chain's `YoloModeApprove` and the approval gate rather than here
        //     (see `permission/guardian.rs` and `callbacks::execute_tool`).

        // 12. YoloModeApprove
        if mode == PermissionMode::Yolo {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Allow,
                policy_name: "YoloModeApprove".into(),
                session_approval_rule: None,
                reason: None,
            };
        }

        // 13. DefaultToolApprove (v2 `DEFAULT_APPROVE_TOOLS`,
        //     default-tool-approve.ts): the tools that run without an approval
        //     prompt in every permission mode. The fork's list had drifted to
        //     the read-only file tools alone, so TodoList, the plan tools, the
        //     goal tools, Agent / Skill and the task readers all fell through
        //     to `FallbackAsk` — a prompt v2 never shows. Both spellings are
        //     listed because `evaluate` lowercases without squashing
        //     underscores, so `ReadMediaFile` and `read_media_file` differ.
        if DEFAULT_APPROVE_TOOLS.contains(&tool_lower.as_str())
            || crate::tools::github::is_readonly_tool(tool_name)
        {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Allow,
                policy_name: "DefaultToolApprove".into(),
                session_approval_rule: None,
                reason: None,
            };
        }

        // 14. GitCwdWriteApprove (v2 `git-cwd-write-approve.ts`): a Write / Edit
        //     whose target lands inside the workspace runs without a prompt.
        //     v2's gates, all of them: the Write/Edit tool pair, `pathClass ===
        //     'posix'`, the target inside `{workspaceDir, additionalDirs}`, and
        //     the workspace cwd inside a git work tree. The engine has no
        //     path-class handshake, so the *workspace root's* flavour stands in
        //     for the runtime's class ([`is_posix_path`]): a win32 workspace — a
        //     local Windows session — never approves here, which is exactly what
        //     v2's `pathClass !== 'posix'` early return does, while a posix
        //     workspace (Linux/macOS, or a remote posix runtime) proceeds. The
        //     containment is component-wise ([`is_within_workspace`]), so
        //     `/repo2/x` is not inside `/repo` as the fork's prefix test read
        //     it, and the cwd must sit in a work tree ([`find_git_work_tree`],
        //     v2's `findWorkTree(cwd)`). The fork's earlier "any tool, any
        //     platform, any path under a prefix" breadth was a deviation and is
        //     gone.
        if matches!(tool_lower.as_str(), "write" | "edit")
            && let Some(path) = path_subject.as_deref()
            && let Some(dir) = self.workspace_dir()
            && is_posix_path(dir)
            && is_posix_path(path)
            && !is_git_control_path(path)
            && !crate::native::file_type::is_sensitive_file(path)
            && is_within_workspace(path, dir, &self.additional_dirs)
            && find_git_work_tree(dir).is_some()
        {
            return LocalPermissionVerdict {
                decision: VerdictDecision::Allow,
                policy_name: "GitCwdWriteApprove".into(),
                session_approval_rule: None,
                reason: None,
            };
        }

        // 15. FallbackAsk
        LocalPermissionVerdict {
            decision: VerdictDecision::Ask,
            policy_name: "FallbackAsk".into(),
            session_approval_rule: None,
            reason: Some(
                LocalizedText::with_params(
                    "engine.permission.toolExecutionRequiresApproval",
                    i18n_params!["tool_name" => tool_name],
                )
                .render(),
            ),
        }
    }

    #[inline]
    fn matches_any_rule(
        rules: &[CompiledRule],
        tool_lower: &str,
        subject: Option<&RuleSubject>,
        subject_variants: &[String],
    ) -> Option<String> {
        for rule in rules {
            // A tool-only rule (`Bash*`, with no argument pattern) matches even
            // when the call carries no subject; with a subject, any spelling of
            // the target can satisfy the argument pattern, compared the way the
            // tool's own `matchesRule` compares it.
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
}

/// Spellings of one subject: every variant a rule may match, plus the path the
/// security checks should use.
///
/// `resolved` is the lexically resolved path (v2 `canonicalizePath`,
/// `path-access.ts:137-164`), never the raw spelling and never "whichever
/// sorts last". It resolves no symlinks: v2's permission policies compare the
/// paths each tool declares, and the realpath gate runs in the tool layer.
struct RuleSubjectVariants {
    variants: Vec<String>,
    resolved: Option<String>,
}

impl RuleSubjectVariants {
    fn empty() -> Self {
        Self {
            variants: Vec::new(),
            resolved: None,
        }
    }
}

/// Warn once when a path subject cannot be resolved against a workspace cwd:
/// only the raw spelling stays available, so relative-path rules silently stop
/// matching. One line per process is enough to make the degradation visible.
static CWD_MISSING_WARNED: std::sync::Once = std::sync::Once::new();
/// Same, for a lexical resolution failure (unsupported spelling, invalid path).
static SUBJECT_RESOLVE_WARNED: std::sync::Once = std::sync::Once::new();

/// The subject spellings a rule can match: the raw argument and the lexically
/// resolved absolute path, mirroring v2 `pathVariants` (`rule-match.ts:49-60`).
/// A deny rule for `/etc/passwd` otherwise missed `/etc/../etc/passwd`.
/// Canonicalisation is lexical only — `fs::canonicalize` here would stat the
/// filesystem on every tool call already evaluated (`fs` on a cold UNC share
/// cost seconds), and v2 resolves real paths once per file-tool execution
/// rather than per rule subject.
fn rule_subject_variants(subject: Option<&RuleSubject>, cwd: Option<&str>) -> RuleSubjectVariants {
    let Some(subject) = subject else {
        return RuleSubjectVariants::empty();
    };
    let raw = subject.value.clone();
    let mut variants = vec![raw.clone()];
    if subject.kind != RuleSubjectKind::Path {
        return RuleSubjectVariants {
            variants,
            resolved: None,
        };
    }
    let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) else {
        CWD_MISSING_WARNED.call_once(|| {
            tracing::warn!(
                subject = %raw,
                "permission path subject has no workspace cwd; only the raw spelling is matched, so relative rules may not apply"
            );
        });
        return RuleSubjectVariants {
            variants,
            resolved: None,
        };
    };
    let path_class = if cwd.starts_with('/') {
        crate::native::path_access::PathClass::Posix
    } else {
        crate::native::path_access::PathClass::Win32
    };
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| home.to_string_lossy().to_string());
    let expanded = crate::native::path_access::expand_user_path(&raw, home.as_deref(), path_class);
    let Ok(lexical) = crate::native::path_access::canonicalize_path(&expanded, cwd, path_class)
    else {
        SUBJECT_RESOLVE_WARNED.call_once(|| {
            tracing::warn!(
                subject = %raw,
                "permission path subject could not be resolved; only the raw spelling is matched"
            );
        });
        return RuleSubjectVariants {
            variants,
            resolved: None,
        };
    };
    variants.push(lexical.clone());
    let resolved = lexical;
    // Rules are commonly written with `/` even on Windows; match both
    // spellings of the resolved path.
    let slash_variants: Vec<String> = variants
        .iter()
        .filter(|variant| variant.contains('\\'))
        .map(|variant| variant.replace('\\', "/"))
        .collect();
    variants.extend(slash_variants);
    variants.sort();
    variants.dedup();
    RuleSubjectVariants {
        variants,
        resolved: Some(resolved),
    }
}

/// The file paths a call touches — v2's `fileAccesses(context)`, the input
/// `SensitiveFileAccessAsk` (#8) and `GitControlPathAccessAsk` (#9) evaluate.
///
/// v2 derives them from each tool's *declared* accesses, so a tool that
/// declares none never reaches those two policies: `bashTool.ts` exposes only
/// `approvalRule` / `matchesRule` / `execute`, and WebSearch / FetchURL take a
/// query and a URL. Feeding a Bash *command* or a search *query* to those
/// policies instead made any command that merely mentioned `/x/.env` or
/// `/repo/.git/config` prompt — in yolo too, where #8/#9 run before
/// `YoloModeApprove`.
///
/// v2's declaration also carries the operation, but neither #8 nor #9 looks at
/// it — a read-only probe is an access like any other. So the split is recorded
/// where it belongs rather than collapsed into "write":
///   * read-only — `Read`, `Grep`, `Glob`, the legacy media read, and the
///     fork's `Lsp` code-intelligence probe (`core_tool_defs.rs:561-594`),
///     plus the `Memory` actions that only look (`read` / `search` / `list`);
///   * mutating — `Write`, `Edit`, and the `Memory` actions that create,
///     change or delete a file (`memory_action.rs:67-99`).
/// `Memory` only carries `path` on its file-bearing actions, so reading that
/// argument cannot drag a list/search into the policy.
fn file_accesses(tool_lower: &str, args: &Value) -> Vec<String> {
    if !matches!(
        tool_lower,
        "read"
            | "write"
            | "edit"
            | "grep"
            | "glob"
            | "lsp"
            | "readmediafile"
            | "read_media_file"
            | "memory"
    ) {
        return Vec::new();
    }

    match args.get("path") {
        Some(Value::String(path)) => vec![path.clone()],
        Some(Value::Array(paths)) => paths
            .iter()
            .filter_map(|path| path.as_str())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// The subject a tool's own v2 `matchesRule` compares, and the strategy it
/// uses. `None` is v2's `execution.matchesRule === undefined`: an argument rule
/// on such a tool never matches (`permissionRules/matchesRule.ts:100-102`).
fn extract_rule_subject(tool_lower: &str, args: &Value) -> Option<RuleSubject> {
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
        // v2 `readTool.ts:324-329` / `writeTool.ts:64` / `editTool.ts:63`:
        // `matchesPathRuleSubject(path, …)`.
        "read" | "write" | "edit" => string_arg("path").map(path),
        // The media branch of `Read` compares its path with the plain glob
        // strategy (`readTool.ts:402`). The legacy spelling is that branch
        // under its pre-fold name, so it is matched the same way.
        "readmediafile" | "read_media_file" => string_arg("path").map(glob),
        // v2 Grep / Glob rules match the *search pattern*, not the directory
        // (`grepTool.ts:119`, `globTool.ts:133`).
        "grep" | "glob" => string_arg("pattern").map(glob),
        "fetchurl" | "fetch_url" => string_arg("url").map(glob),
        "websearch" | "web_search" => string_arg("query").map(glob),
        // v2 `powershellTool.ts` `matchesRule` matches the rule's argument
        // against the script with the same glob semantics Bash uses for its
        // command, so `PowerShell(Get-Service)` is a usable session rule.
        "bash" | "powershell" => string_arg("command").map(glob),
        // v2 `skillTool.ts:50`: the skill name. `skill.rs:538-539` also accepts
        // the legacy `name` key, and a rule must bind the name the call used.
        "skill" => string_arg("skill").or_else(|| string_arg("name")).map(glob),
        // v2 `agentTool.ts:301-320` rules match the resolved profile name. A
        // `fork` call reuses the *caller's* profile, which this layer cannot
        // read — such a call declares no subject rather than a wrong one.
        "agent" => match string_arg("subagent_type") {
            Some(profile) => Some(glob(profile)),
            None if args.get("fork").and_then(Value::as_bool) == Some(true) => None,
            None => Some(glob(
                crate::tools::agent_tool::DEFAULT_PROFILE_NAME.to_string(),
            )),
        },
        // v2 `taskOutputTool.ts:59`, `taskStopTool.ts:29`, `taskWaitTool.ts:150`,
        // `taskListTool.ts:29-33`.
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
        // The fork's own path tools. `Lsp` names a workspace file, so its path
        // resolves like Read's; a `Memory` path is relative to the memory
        // store, not the cwd, so it is compared as written.
        "lsp" => string_arg("path").map(path),
        "memory" => string_arg("path").map(glob),
        _ if crate::tools::github::is_github_tool(tool_lower) => {
            crate::tools::github::rule_subject(tool_lower, args).map(glob)
        }
        _ => None,
    }
}

pub fn is_git_control_path(path_str: &str) -> bool {
    let normalized = path_str.replace('\\', "/").to_ascii_lowercase();
    normalized == ".git"
        || normalized.contains("/.git/")
        || normalized.ends_with("/.git")
        || normalized.starts_with(".git/")
}

/// Whether a path is written in posix form — the engine's stand-in for v2's
/// `pathClass === 'posix'` (`git-cwd-write-approve.ts`). v2 reads the class off
/// the *execution runtime's* environment, not the host OS, so deriving it from
/// the subject path keeps the policy honest in both directions: a Windows host
/// driving a posix runtime still gets the allow, and a win32-flavoured target is
/// never approved (v2's `pathClass !== 'posix'` early return).
pub fn is_posix_path(path_str: &str) -> bool {
    let normalized = path_str.replace('\\', "/");
    // `//…` is a win32 UNC root; `C:…` a drive-absolute or drive-relative path.
    if normalized.starts_with("//") {
        return false;
    }
    let mut chars = normalized.chars();
    !matches!(
        (chars.next(), chars.next()),
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic()
    )
}

/// v2 `isWithinWorkspace` (`tool/path-access.ts`): the target inside the
/// workspace dir or any host-authorized additional dir (`/add-dir`). A relative
/// target resolves against the workspace cwd first, which is what v2's
/// `canonicalizePath(path, cwd)` does before the containment test.
pub fn is_within_workspace(
    candidate: &str,
    workspace_dir: &str,
    additional_dirs: &[String],
) -> bool {
    let absolute = if candidate.starts_with('/') {
        candidate.to_string()
    } else {
        format!("{}/{}", workspace_dir.trim_end_matches('/'), candidate)
    };
    is_within_directory(&absolute, workspace_dir)
        || additional_dirs
            .iter()
            .any(|dir| is_within_directory(&absolute, dir))
}

/// v2 `isWithinDirectory`: containment on normalized path components. `.` and
/// `..` are resolved before the comparison and the base has to match on a
/// component boundary, so `/repo2/x` is *not* inside `/repo` — the fork's
/// `starts_with` prefix test approved it, and approved a `..` escape with it.
pub fn is_within_directory(candidate: &str, base: &str) -> bool {
    let candidate_parts = normalized_posix_parts(candidate);
    let base_parts = normalized_posix_parts(base);
    !base_parts.is_empty()
        && candidate_parts.len() >= base_parts.len()
        && candidate_parts[..base_parts.len()] == base_parts[..]
}

/// Split a posix-flavoured path into components, dropping `.` and resolving
/// `..`. An absolute path keeps a leading `/` component of its own, so a
/// relative path never compares equal to the absolute one it resolves to.
fn normalized_posix_parts(path: &str) -> Vec<String> {
    let unified = path.replace('\\', "/");
    let mut parts: Vec<String> = Vec::new();
    for segment in unified.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_string()),
        }
    }
    if unified.starts_with('/') {
        parts.insert(0, "/".to_string());
    }
    parts
}

/// v2 `findGitWorkTree` (`app/git/workTree.ts`) — the `findWorkTree(cwd)` gate
/// `GitCwdWriteApprove` runs last: walk up from `cwd` for `.git`, which is a
/// directory in a normal checkout and a file carrying a `gitdir:` pointer in a
/// linked worktree or a submodule. Returns the work-tree root; `None` when the
/// walk reaches the filesystem root without finding one, which is v2's
/// `findWorkTree(cwd) === null` and takes the policy out of the chain.
pub fn find_git_work_tree(cwd: &str) -> Option<std::path::PathBuf> {
    let start = std::path::Path::new(cwd);
    if cwd.is_empty() || !start.is_absolute() {
        return None;
    }
    let mut current = start;
    loop {
        let dot_git = current.join(".git");
        if let Ok(metadata) = std::fs::metadata(&dot_git) {
            if metadata.is_dir() {
                return Some(current.to_path_buf());
            }
            if metadata.is_file()
                && std::fs::read_to_string(&dot_git)
                    .ok()
                    .is_some_and(|content| has_git_dir_pointer(&content))
            {
                return Some(current.to_path_buf());
            }
        }
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent,
            _ => return None,
        }
    }
}

/// v2 `parseGitDirPointer` (`app/git/workTree.ts`): whether a `.git` file's
/// first line carries a non-empty `gitdir:` target, BOM-tolerant. The target
/// itself is not needed here — the policy gate only asks whether a work tree
/// exists (`findWorkTree(cwd) !== null`) — so it is detected, not resolved.
fn has_git_dir_pointer(content: &str) -> bool {
    let stripped = content.strip_prefix('\u{feff}').unwrap_or(content);
    stripped
        .lines()
        .next()
        .map(str::trim)
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .is_some_and(|target| !target.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// v2 `HookDefSchema.strict()` (`externalHooks/configSection.ts:10-19`):
    /// a mistyped event, an out-of-range timeout, an empty command and an
    /// unknown field are configuration errors, not silent no-ops.
    #[test]
    fn hook_def_is_validated_like_v2() {
        let valid: HookDef = serde_json::from_value(json!({
            "event": "PreToolUse",
            "command": "echo hi",
            "timeout": 30
        }))
        .unwrap();
        assert_eq!(valid.event, "PreToolUse");

        for (label, value) in [
            (
                "unknown event",
                json!({ "event": "NotAnEvent", "command": "echo hi" }),
            ),
            (
                "timeout below range",
                json!({ "event": "PreToolUse", "command": "echo hi", "timeout": 0 }),
            ),
            (
                "timeout above range",
                json!({ "event": "PreToolUse", "command": "echo hi", "timeout": 601 }),
            ),
            (
                "empty command",
                json!({ "event": "PreToolUse", "command": "  " }),
            ),
            (
                "unknown field",
                json!({ "event": "PreToolUse", "command": "echo hi", "extra": true }),
            ),
        ] {
            assert!(
                serde_json::from_value::<HookDef>(value).is_err(),
                "{label} must be rejected"
            );
        }
    }

    /// A malformed rule must not lock every tool (fail-open), but it must not
    /// vanish either: the engine records it so the drop is observable.
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

    /// v2 `CompactionAuthorizationGuard`: once the context holds a compaction
    /// summary, an irreversible Bash command asks even under yolo; other
    /// commands and uncompacted sessions keep the normal chain.
    #[test]
    fn compaction_authorization_guard_asks_only_for_irreversible_bash_after_compaction() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });
        assert_eq!(
            engine
                .evaluate("Bash", &json!({ "command": "git push origin main" }))
                .policy_name,
            "YoloModeApprove",
            "a session without a compaction summary is untouched"
        );

        let mut summary = LLMMessage::new("user", "the conversation was compacted");
        summary.origin = Some(json!({ "kind": "compaction_summary" }));
        engine.set_conversation_provider(std::sync::Arc::new(move || vec![summary.clone()]));

        let verdict = engine.evaluate("Bash", &json!({ "command": "git push origin main" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "CompactionAuthorizationGuard");
        assert!(verdict.reason.is_some());

        for command in [
            "git reset --hard HEAD~1",
            "rm -rf build",
            "gh pr merge 12",
            "kubectl delete pod x",
        ] {
            assert_eq!(
                engine
                    .evaluate("Bash", &json!({ "command": command }))
                    .policy_name,
                "CompactionAuthorizationGuard",
                "{command} is irreversible"
            );
        }

        assert_eq!(
            engine
                .evaluate("Bash", &json!({ "command": "cargo test" }))
                .policy_name,
            "YoloModeApprove",
            "an ordinary command is not affected"
        );
        assert_eq!(
            engine
                .evaluate("Read", &json!({ "path": "src/main.rs" }))
                .policy_name,
            "YoloModeApprove",
            "the guard is Bash-only"
        );
    }

    #[test]
    fn test_yolo_mode_approves_mutating_tools() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        // Write
        let verdict = engine.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());

        // Edit
        let verdict = engine.evaluate("Edit", &json!({ "path": "README.md" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());

        // Bash
        let verdict = engine.evaluate("Bash", &json!({ "command": "cargo check" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());

        // Mutating GitHub tool
        let verdict = engine.evaluate(
            "GitHubCreateIssue",
            &json!({ "owner": "octocat", "repo": "hello-world", "title": "test" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());
    }

    /// v2's Bash tool declares **no** file accesses (`bashTool.ts` exposes only
    /// `approvalRule` / `matchesRule` / `execute`), so in v2 Bash never reaches
    /// `SensitiveFileAccessAsk` or `GitControlPathAccessAsk` — both read
    /// `fileAccesses(context)` and bail out on an empty list. A command that
    /// merely *mentions* a sensitive or `.git` path must therefore keep the
    /// normal mode verdict instead of asking.
    #[test]
    fn test_bash_command_text_is_not_a_file_access() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        let verdict = engine.evaluate(
            "Bash",
            &json!({ "command": "cat /workspace/project/.git/config" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");

        let verdict = engine.evaluate("Bash", &json!({ "command": "cat /home/u/project/.env" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");

        let verdict = engine.evaluate(
            "Bash",
            &json!({ "command": "grep -n x /workspace/project/credentials" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");

        let verdict = engine.evaluate(
            "Bash",
            &json!({ "command": "git commit -F .git/CMSG9.txt" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");

        // A path tool on the same target still asks — only Bash changed.
        let verdict = engine.evaluate("Read", &json!({ "path": "/workspace/project/.git/config" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "GitControlPathAccessAsk");
    }

    /// v2's WebSearch and FetchURL declare no file access either, so a query or
    /// URL that looks like a sensitive path must not reach #8/#9.
    #[test]
    fn test_query_and_url_subjects_are_not_file_accesses() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        let verdict = engine.evaluate("WebSearch", &json!({ "query": "how to read .env" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");

        let verdict = engine.evaluate("FetchURL", &json!({ "url": "https://example.com/.env" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
    }

    /// Every path-shaped tool contributes a file access, the fork's own ones
    /// included: `Lsp` and the probing `Memory` actions read the target, the
    /// rest create or change it, and #8/#9 treat both alike.
    #[test]
    fn test_path_shaped_tools_declare_file_accesses() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        let sensitive = "/workspace/project/.env";
        for tool in [
            "Read",
            "Write",
            "Edit",
            "Grep",
            "Glob",
            "lsp",
            "ReadMediaFile",
            "Memory",
        ] {
            let verdict = engine.evaluate(tool, &json!({ "path": sensitive }));
            assert_eq!(
                verdict.decision,
                VerdictDecision::Ask,
                "{tool} should declare a file access"
            );
            assert_eq!(verdict.policy_name, "SensitiveFileAccessAsk");
            // A policy ask carries no session-approval candidate: there is no
            // user-written rule to remember. Offering to remember "this" would
            // have to invent a pattern, and an invented one is how a single
            // approval turns into a tool-wide grant.
            assert_eq!(verdict.session_approval_rule, None);
        }
    }

    /// v2 #4250: `[permission] dangerousCommandGuard = false` disables every
    /// built-in guard ask policy, not just DangerousCommandAsk. The hard
    /// path-access block is a separate layer and is unaffected.
    #[test]
    fn test_guard_off_disables_sensitive_and_git_path_asks() {
        let sensitive = "/workspace/project/.env";
        let git_path = "/workspace/project/.git/config";

        let guarded = PermissionEngine::new(PolicySnapshot::default());
        let verdict = guarded.evaluate("Read", &json!({ "path": sensitive }));
        assert_eq!(verdict.policy_name, "SensitiveFileAccessAsk");
        let verdict = guarded.evaluate("Read", &json!({ "path": git_path }));
        assert_eq!(verdict.policy_name, "GitControlPathAccessAsk");

        let unguarded = PermissionEngine::new(PolicySnapshot {
            dangerous_command_guard: false,
            ..Default::default()
        });
        for path in [sensitive, git_path] {
            let verdict = unguarded.evaluate("Read", &json!({ "path": path }));
            assert_eq!(
                verdict.decision,
                VerdictDecision::Allow,
                "guards off must let the default-approve policy decide {path}"
            );
            assert_eq!(verdict.policy_name, "DefaultToolApprove");
        }

        // The switch skips the guard policy; it is not a blanket allow. A
        // Bash call still falls through to the mode's own decision.
        let verdict = unguarded.evaluate("Bash", &json!({ "command": "rm -rf /" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");
    }

    #[test]
    fn test_yolo_mode_still_intercepts_sensitive_files() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        // Read .env
        let verdict = engine.evaluate("Read", &json!({ "path": ".env" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "SensitiveFileAccessAsk");
        assert_eq!(
            verdict.reason,
            Some("Access to sensitive file requires approval: .env".into())
        );
        assert!(!verdict.is_allow());

        // Grep .env.local
        let verdict = engine.evaluate("Grep", &json!({ "path": "config/.env.local" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "SensitiveFileAccessAsk");
        assert_eq!(
            verdict.reason,
            Some("Access to sensitive file requires approval: config/.env.local".into())
        );
        assert!(!verdict.is_allow());

        // Write to private SSH key
        let verdict = engine.evaluate("Write", &json!({ "path": "~/.ssh/id_rsa" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "SensitiveFileAccessAsk");
        assert_eq!(
            verdict.reason,
            Some("Access to sensitive file requires approval: ~/.ssh/id_rsa".into())
        );
        assert!(!verdict.is_allow());

        // Edit ssl\server.key — standalone `.key` is NOT sensitive in v2
        // (only the dot-variants of `id_rsa` / `id_ed25519` / `id_ecdsa` /
        // `credentials` are). The path must not trigger SensitiveFileAccessAsk.
        let verdict = engine.evaluate("Edit", &json!({ "path": "ssl\\server.key" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
    }

    #[test]
    fn test_user_configured_deny_overrides_all_modes_and_policies() {
        // 1. Deny with argument glob overrides YOLO mode
        let engine_yolo = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            deny_rules: vec!["Bash(rm -rf *)".into()],
            ..Default::default()
        });
        let verdict = engine_yolo.evaluate("Bash", &json!({ "command": "rm -rf /" }));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");
        assert_eq!(
            verdict.reason,
            Some("Denied by user rule: Bash(rm -rf *)".into())
        );
        assert!(!verdict.is_allow());

        // Non-matching bash command in YOLO mode is allowed
        let verdict_safe = engine_yolo.evaluate("Bash", &json!({ "command": "echo hello" }));
        assert_eq!(verdict_safe.decision, VerdictDecision::Allow);
        assert_eq!(verdict_safe.policy_name, "YoloModeApprove");
        assert_eq!(verdict_safe.reason, None);

        // 2. Tool-wide deny overrides Auto mode
        let engine_auto = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            deny_rules: vec!["Write".into()],
            ..Default::default()
        });
        let verdict_auto = engine_auto.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict_auto.decision, VerdictDecision::Deny);
        assert_eq!(verdict_auto.policy_name, "UserConfiguredDeny");
        assert_eq!(
            verdict_auto.reason,
            Some("Denied by user rule: Write".into())
        );
        assert!(!verdict_auto.is_allow());

        // 3. Wildcard tool rule `*(*.secret)` denies any matching tool call
        let engine_wildcard = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            deny_rules: vec!["*(*.secret)".into()],
            ..Default::default()
        });
        let verdict_read = engine_wildcard.evaluate("Read", &json!({ "path": "app.secret" }));
        assert_eq!(verdict_read.decision, VerdictDecision::Deny);
        assert_eq!(verdict_read.policy_name, "UserConfiguredDeny");
        assert_eq!(
            verdict_read.reason,
            Some("Denied by user rule: *(*.secret)".into())
        );

        let verdict_edit = engine_wildcard.evaluate("Edit", &json!({ "path": "app.secret" }));
        assert_eq!(verdict_edit.decision, VerdictDecision::Deny);
        assert_eq!(verdict_edit.policy_name, "UserConfiguredDeny");
        assert_eq!(
            verdict_edit.reason,
            Some("Denied by user rule: *(*.secret)".into())
        );

        // 4. Deny overrides session approval history
        let engine_session = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            deny_rules: vec!["Bash(dropdb *)".into()],
            session_approvals: vec!["Bash".into()],
            ..Default::default()
        });
        let verdict_denied = engine_session.evaluate("Bash", &json!({ "command": "dropdb prod" }));
        assert_eq!(verdict_denied.decision, VerdictDecision::Deny);
        assert_eq!(verdict_denied.policy_name, "UserConfiguredDeny");
        assert_eq!(
            verdict_denied.reason,
            Some("Denied by user rule: Bash(dropdb *)".into())
        );
    }

    #[test]
    fn test_default_tool_approve_for_all_readonly_tools() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });

        let cases = [
            ("read", json!({ "path": "package.json" })),
            ("READ", json!({ "path": "src/lib.rs" })),
            ("grep", json!({ "path": "src", "pattern": "fn" })),
            ("Grep", json!({ "path": "src", "pattern": "struct" })),
            ("glob", json!({ "path": ".", "pattern": "*.ts" })),
            ("GLOB", json!({ "path": ".", "pattern": "*.rs" })),
            ("fetchurl", json!({ "url": "https://example.com" })),
            ("fetch_url", json!({ "url": "https://example.com/api" })),
            ("Fetch_Url", json!({ "url": "https://example.com/docs" })),
            ("websearch", json!({ "query": "rust async" })),
            ("web_search", json!({ "query": "tokio tutorial" })),
            ("WebSearch", json!({ "query": "actix web" })),
            // Fork-original read-only tools: they execute natively, so without
            // an entry here they fell through to `FallbackAsk` and prompted in
            // every non-Yolo mode — a prompt v2's read-only policy never shows.
            ("lsp", json!({ "action": "hover", "path": "src/lib.rs" })),
            ("tower_status", json!({})),
            ("towerstatus", json!({})),
            ("tower_inbox", json!({ "limit": 10 })),
            ("towerinbox", json!({})),
        ];

        for (tool, args) in cases {
            let verdict = engine.evaluate(tool, &args);
            assert_eq!(
                verdict.decision,
                VerdictDecision::Allow,
                "Tool '{tool}' should be allowed by DefaultToolApprove"
            );
            assert_eq!(
                verdict.policy_name, "DefaultToolApprove",
                "Tool '{tool}' should trigger DefaultToolApprove"
            );
            assert_eq!(verdict.reason, None);
            assert!(verdict.is_allow());
        }
    }

    #[test]
    fn test_manual_mode_fallback_ask_for_write() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });

        // Write without git_cwd falls back to Ask
        let verdict = engine.evaluate("Write", &json!({ "path": "package.json" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");
        assert_eq!(
            verdict.reason,
            Some("Tool execution requires approval: Write".into())
        );
        assert!(!verdict.is_allow());

        // Edit without git_cwd falls back to Ask
        let verdict = engine.evaluate("Edit", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");
        assert_eq!(
            verdict.reason,
            Some("Tool execution requires approval: Edit".into())
        );

        // Unknown custom tool falls back to Ask
        let verdict = engine.evaluate("DeployTool", &json!({ "env": "staging" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");
        assert_eq!(
            verdict.reason,
            Some("Tool execution requires approval: DeployTool".into())
        );
    }

    #[test]
    fn test_readonly_github_tools_approved_by_default() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });

        // Read-only GitHub tools are approved by DefaultToolApprove
        for tool in [
            "GitHubGetRepo",
            "GitHubGetPRDiff",
            "GitHubSearchCode",
            "GitHubGetMe",
        ] {
            let verdict =
                engine.evaluate(tool, &json!({ "owner": "octocat", "repo": "hello-world" }));
            assert_eq!(
                verdict.decision,
                VerdictDecision::Allow,
                "{tool} should be allowed"
            );
            assert_eq!(verdict.policy_name, "DefaultToolApprove");
            assert_eq!(verdict.reason, None);
            assert!(verdict.is_allow());
        }
    }

    #[test]
    fn test_mutating_github_tools_fall_back_to_ask() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });

        // Mutating GitHub tools fall back to FallbackAsk
        for tool in ["GitHubCreateIssue", "GitHubMergePR", "GitHubUpdateRef"] {
            let verdict = engine.evaluate(
                tool,
                &json!({ "owner": "octocat", "repo": "hello-world", "title": "t" }),
            );
            assert_eq!(
                verdict.decision,
                VerdictDecision::Ask,
                "{tool} should require approval"
            );
            assert_eq!(verdict.policy_name, "FallbackAsk");
            assert_eq!(
                verdict.reason,
                Some(format!("Tool execution requires approval: {tool}"))
            );
            assert!(!verdict.is_allow());
        }
    }

    #[test]
    fn test_github_subject_rules_match() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            deny_rules: vec!["GitHubCreateIssue(octocat/hello-world)".into()],
            ..Default::default()
        });

        // Matching subject is denied
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

        // Different repo subject falls back to FallbackAsk
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
    fn test_auto_mode_ask_user_question_denied() {
        let engine_auto = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            ..Default::default()
        });

        // Exact PascalCase AskUserQuestion
        let verdict = engine_auto.evaluate(
            "AskUserQuestion",
            &json!({ "question": "Should I proceed with delete?" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "AutoModeAskUserQuestionDeny");
        assert_eq!(
            verdict.reason,
            Some("Auto mode cannot ask interactive questions".into())
        );
        assert!(!verdict.is_allow());

        // snake_case ask_user_question
        let verdict_snake = engine_auto.evaluate(
            "ask_user_question",
            &json!({ "question": "Which option do you prefer?" }),
        );
        assert_eq!(verdict_snake.decision, VerdictDecision::Deny);
        assert_eq!(verdict_snake.policy_name, "AutoModeAskUserQuestionDeny");
        assert_eq!(
            verdict_snake.reason,
            Some("Auto mode cannot ask interactive questions".into())
        );

        // In Manual mode, AskUserQuestion is approved by DefaultToolApprove —
        // v2 lists it in `DEFAULT_APPROVE_TOOLS`, so asking the user a
        // question never itself needs approval. Only Auto mode denies it
        // (AutoModeAskUserQuestionDeny runs first).
        let engine_manual = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let verdict_manual = engine_manual.evaluate("AskUserQuestion", &json!({}));
        assert_eq!(verdict_manual.decision, VerdictDecision::Allow);
        assert_eq!(verdict_manual.policy_name, "DefaultToolApprove");
        assert_eq!(verdict_manual.reason, None);

        // In Yolo mode, AskUserQuestion is allowed by YoloModeApprove
        let engine_yolo = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });
        let verdict_yolo = engine_yolo.evaluate("AskUserQuestion", &json!({}));
        assert_eq!(verdict_yolo.decision, VerdictDecision::Allow);
        assert_eq!(verdict_yolo.policy_name, "YoloModeApprove");
        assert_eq!(verdict_yolo.reason, None);
    }

    #[test]
    fn test_auto_mode_approves_safe_and_mutating() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            ..Default::default()
        });

        // Mutating tools like Write are approved in Auto mode
        let verdict = engine.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());

        // Edit is approved in Auto mode
        let verdict = engine.evaluate("Edit", &json!({ "path": "Cargo.toml" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");
        assert_eq!(verdict.reason, None);

        // Mutating GitHub tool is approved in Auto mode
        let verdict = engine.evaluate(
            "GitHubCreateIssue",
            &json!({ "owner": "octocat", "repo": "hello-world", "title": "t" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");
        assert_eq!(verdict.reason, None);
    }

    #[test]
    fn an_engine_recorded_approval_stops_the_next_ask() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        assert_eq!(
            engine
                .evaluate("Write", &json!({ "path": "notes.txt" }))
                .decision,
            VerdictDecision::Ask
        );

        assert!(engine.remember_session_approval("Write(notes.txt)"));

        let verdict = engine.evaluate("Write", &json!({ "path": "notes.txt" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "SessionApprovalHistory");
        assert_eq!(
            engine.session_approval_rules(),
            vec!["Write(notes.txt)".to_string()]
        );
    }

    #[test]
    fn a_recorded_rule_covers_its_own_pattern_and_nothing_else() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        assert!(engine.remember_session_approval("Write(src/*.rs)"));

        assert!(
            engine
                .evaluate("Write", &json!({ "path": "src/lib.rs" }))
                .is_allow(),
            "the approved pattern is allowed"
        );
        assert_eq!(
            engine
                .evaluate("Write", &json!({ "path": "notes.txt" }))
                .decision,
            VerdictDecision::Ask,
            "approving one path must not approve the tool"
        );
        assert_eq!(
            engine
                .evaluate("Read", &json!({ "path": "src/lib.rs" }))
                .decision,
            VerdictDecision::Allow,
            "Read is allowed by the default policy, not by the Write rule"
        );
    }

    #[test]
    fn a_rule_is_recorded_once_and_only_when_it_can_mean_something() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            session_approvals: vec!["Bash(cargo test)".into()],
            ..Default::default()
        });

        assert!(
            !engine.remember_session_approval("   "),
            "an empty rule grants nothing"
        );
        assert!(
            !engine.remember_session_approval("Bash(cargo test)"),
            "a rule the snapshot already carries is not re-recorded"
        );
        assert!(engine.remember_session_approval("Bash(cargo build)"));
        assert!(
            !engine.remember_session_approval("Bash(cargo build)"),
            "recording twice is a no-op"
        );
        assert_eq!(
            engine.session_approval_rules(),
            vec!["Bash(cargo build)".to_string()]
        );
    }

    #[test]
    fn test_session_approval_history() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            session_approvals: vec!["Write(src/*.rs)".into(), "Bash(cargo test)".into()],
            ..Default::default()
        });

        // Matches session rule for Write(src/*.rs)
        let verdict = engine.evaluate("Write", &json!({ "path": "src/lib.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "SessionApprovalHistory");
        assert_eq!(
            verdict.reason,
            Some("Approved by session history rule: Write(src/*.rs)".into())
        );
        assert!(verdict.is_allow());

        // Matches session rule for Bash(cargo test)
        let verdict = engine.evaluate("Bash", &json!({ "command": "cargo test" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "SessionApprovalHistory");
        assert_eq!(
            verdict.reason,
            Some("Approved by session history rule: Bash(cargo test)".into())
        );

        // Unmatched path falls through to FallbackAsk
        let verdict_miss = engine.evaluate("Write", &json!({ "path": "tests/test.rs" }));
        assert_eq!(verdict_miss.decision, VerdictDecision::Ask);
        assert_eq!(verdict_miss.policy_name, "FallbackAsk");
        assert_eq!(
            verdict_miss.reason,
            Some("Tool execution requires approval: Write".into())
        );
    }

    #[test]
    fn test_user_configured_ask() {
        // Even in YOLO mode, an explicit user ask rule requires confirmation
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ask_rules: vec!["Write(config/*)".into(), "Bash(deploy *)".into()],
            ..Default::default()
        });

        let verdict = engine.evaluate("Write", &json!({ "path": "config/prod.json" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "UserConfiguredAsk");
        assert_eq!(
            verdict.reason,
            Some("Approval required by user rule: Write(config/*)".into())
        );
        assert!(!verdict.is_allow());
        // The rule also rides as a field, which is what the approval request
        // carries: a session approval remembers *this pattern*, so the host must
        // be told it. Reading it back out of the rendered reason would couple
        // the approval path to a locale-dependent string.
        assert_eq!(
            verdict.session_approval_rule.as_deref(),
            Some("Write(config/*)")
        );

        let verdict_bash = engine.evaluate("Bash", &json!({ "command": "deploy prod" }));
        assert_eq!(verdict_bash.decision, VerdictDecision::Ask);
        assert_eq!(verdict_bash.policy_name, "UserConfiguredAsk");
        assert_eq!(
            verdict_bash.reason,
            Some("Approval required by user rule: Bash(deploy *)".into())
        );

        // Non-matching call in YOLO mode is approved
        let verdict_pass = engine.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict_pass.decision, VerdictDecision::Allow);
        assert_eq!(verdict_pass.policy_name, "YoloModeApprove");
        assert_eq!(verdict_pass.reason, None);
    }

    #[test]
    fn test_user_configured_allow() {
        // In Manual mode, explicit allow rules permit normally restricted operations
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            allow_rules: vec!["Write(tmp/*)".into(), "Bash(npm run lint)".into()],
            ..Default::default()
        });

        let verdict = engine.evaluate("Write", &json!({ "path": "tmp/cache.json" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "UserConfiguredAllow");
        assert_eq!(
            verdict.reason,
            Some("Allowed by user rule: Write(tmp/*)".into())
        );
        assert!(verdict.is_allow());

        let verdict_bash = engine.evaluate("Bash", &json!({ "command": "npm run lint" }));
        assert_eq!(verdict_bash.decision, VerdictDecision::Allow);
        assert_eq!(verdict_bash.policy_name, "UserConfiguredAllow");
        assert_eq!(
            verdict_bash.reason,
            Some("Allowed by user rule: Bash(npm run lint)".into())
        );

        // Unmatched path requires approval
        let verdict_other = engine.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict_other.decision, VerdictDecision::Ask);
        assert_eq!(verdict_other.policy_name, "FallbackAsk");
        assert_eq!(
            verdict_other.reason,
            Some("Tool execution requires approval: Write".into())
        );
    }

    #[test]
    fn test_git_control_path_access_ask() {
        let engine_manual = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let engine_yolo = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        let test_paths = [
            ".git/config",
            "repo/.git/HEAD",
            "packages/app/.git/refs/heads/main",
            "sub\\.git\\hooks\\pre-commit",
            ".git",
            "my_repo/.git",
        ];

        for path in test_paths {
            // Manual mode asks
            let verdict = engine_manual.evaluate("Write", &json!({ "path": path }));
            assert_eq!(
                verdict.decision,
                VerdictDecision::Ask,
                "Path '{path}' should require approval in Manual mode"
            );
            assert_eq!(verdict.policy_name, "GitControlPathAccessAsk");
            assert_eq!(
                verdict.reason,
                Some(format!(
                    "Access to git control path requires approval: {path}"
                ))
            );
            assert!(!verdict.is_allow());

            // YOLO mode also asks (cannot bypass git control path)
            let verdict_yolo = engine_yolo.evaluate("Read", &json!({ "path": path }));
            assert_eq!(
                verdict_yolo.decision,
                VerdictDecision::Ask,
                "Path '{path}' should require approval in YOLO mode"
            );
            assert_eq!(verdict_yolo.policy_name, "GitControlPathAccessAsk");
            assert_eq!(
                verdict_yolo.reason,
                Some(format!(
                    "Access to git control path requires approval: {path}"
                ))
            );
        }

        // Non-git control files should not be flagged as git control paths
        let verdict_gitignore = engine_manual.evaluate("Read", &json!({ "path": ".gitignore" }));
        assert_eq!(verdict_gitignore.decision, VerdictDecision::Allow);
        assert_eq!(verdict_gitignore.policy_name, "DefaultToolApprove");

        let verdict_workflow =
            engine_manual.evaluate("Read", &json!({ "path": ".github/workflows/ci.yml" }));
        assert_eq!(verdict_workflow.decision, VerdictDecision::Allow);
        assert_eq!(verdict_workflow.policy_name, "DefaultToolApprove");
    }

    /// v2 `git-cwd-write-approve.ts` end to end, against a **real** work tree:
    /// the policy needs a posix workspace root that exists on disk carrying a
    /// `.git`, so this matrix runs where temp dirs are posix paths. The gates
    /// that need no work tree are covered by
    /// `test_git_cwd_write_approve_requires_a_posix_work_tree`.
    #[cfg(unix)]
    #[test]
    fn test_git_cwd_write_approve() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let engine = PermissionEngine::with_workspace(
            PolicySnapshot {
                mode: PermissionMode::Manual,
                git_cwd: Some(root.clone()),
                ..Default::default()
            },
            Some(root.clone()),
            Vec::new(),
        );

        // 1. Write inside the work tree is approved by GitCwdWriteApprove
        let verdict = engine.evaluate("Write", &json!({ "path": format!("{root}/src/lib.rs") }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "GitCwdWriteApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());

        // 2. Edit inside the work tree is approved by GitCwdWriteApprove
        let verdict_edit =
            engine.evaluate("Edit", &json!({ "path": format!("{root}/Cargo.toml") }));
        assert_eq!(verdict_edit.decision, VerdictDecision::Allow);
        assert_eq!(verdict_edit.policy_name, "GitCwdWriteApprove");
        assert_eq!(verdict_edit.reason, None);

        // 3. Write outside git_cwd falls back to FallbackAsk
        let verdict_outside =
            engine.evaluate("Write", &json!({ "path": "/other/location/file.txt" }));
        assert_eq!(verdict_outside.decision, VerdictDecision::Ask);
        assert_eq!(verdict_outside.policy_name, "FallbackAsk");
        assert_eq!(
            verdict_outside.reason,
            Some("Tool execution requires approval: Write".into())
        );

        // 4. A sensitive target inside the work tree hits SensitiveFileAccessAsk
        let env_path = format!("{root}/.env");
        let verdict_sensitive = engine.evaluate("Write", &json!({ "path": env_path }));
        assert_eq!(verdict_sensitive.decision, VerdictDecision::Ask);
        assert_eq!(verdict_sensitive.policy_name, "SensitiveFileAccessAsk");
        assert_eq!(
            verdict_sensitive.reason,
            Some(format!(
                "Access to sensitive file requires approval: {env_path}"
            ))
        );

        // 5. A git control path inside the work tree hits GitControlPathAccessAsk
        let git_config = format!("{root}/.git/config");
        let verdict_git = engine.evaluate("Write", &json!({ "path": git_config }));
        assert_eq!(verdict_git.decision, VerdictDecision::Ask);
        assert_eq!(verdict_git.policy_name, "GitControlPathAccessAsk");
        assert_eq!(
            verdict_git.reason,
            Some(format!(
                "Access to git control path requires approval: {git_config}"
            ))
        );

        // 6. v2's containment gate is component-wise, so a sibling directory
        // that merely shares the prefix is *not* inside the workspace — the
        // fork's `starts_with` approved it — and neither is a `..` escape that
        // lands outside after normalization. A relative target, by contrast,
        // resolves against the workspace cwd (v2 `canonicalizePath`) and is.
        let sibling = engine.evaluate("Write", &json!({ "path": format!("{root}2/src/lib.rs") }));
        assert_eq!(sibling.decision, VerdictDecision::Ask, "prefix sibling");
        assert_eq!(sibling.policy_name, "FallbackAsk");
        let escape = engine.evaluate(
            "Write",
            &json!({ "path": format!("{root}/../elsewhere/a.rs") }),
        );
        assert_eq!(escape.decision, VerdictDecision::Ask, "`..` escape");
        assert_eq!(escape.policy_name, "FallbackAsk");
        let relative = engine.evaluate("Write", &json!({ "path": "src/relative.rs" }));
        assert_eq!(relative.decision, VerdictDecision::Allow, "relative target");
        assert_eq!(relative.policy_name, "GitCwdWriteApprove");

        // 7. An `/add-dir` root is part of the boundary (v2
        //    `{workspaceDir, additionalDirs}`): without it the target is
        //    outside and asks, with it the same target is approved.
        let elsewhere = tempfile::tempdir().unwrap();
        let other = elsewhere.path().to_string_lossy().to_string();
        let other_file = format!("{other}/notes.md");
        let narrow = engine.evaluate("Write", &json!({ "path": other_file }));
        assert_eq!(narrow.decision, VerdictDecision::Ask);
        assert_eq!(narrow.policy_name, "FallbackAsk");
        let widened = PermissionEngine::with_workspace(
            PolicySnapshot {
                mode: PermissionMode::Manual,
                git_cwd: Some(root.clone()),
                ..Default::default()
            },
            Some(root.clone()),
            vec![other.clone()],
        );
        let granted = widened.evaluate("Write", &json!({ "path": other_file }));
        assert_eq!(granted.decision, VerdictDecision::Allow);
        assert_eq!(granted.policy_name, "GitCwdWriteApprove");

        // 8. v2's tool gate: only the Write/Edit pair is eligible. Another tool
        // whose subject happens to sit inside the workspace is *not* approved
        // here — Bash carries its command as the subject, so a command that
        // looks like a path inside the workspace must not ride this policy.
        let verdict_bash = engine.evaluate(
            "Bash",
            &json!({ "command": format!("{root}/scripts/build.sh") }),
        );
        assert_eq!(verdict_bash.decision, VerdictDecision::Ask);
        assert_eq!(verdict_bash.policy_name, "FallbackAsk");

        assert!(is_posix_path("/workspace/project/src/lib.rs"));
        assert!(!is_posix_path(r"D:\repo\src\main.rs"));
        assert!(!is_posix_path("D:/repo/src/main.rs"));
        assert!(!is_posix_path(r"\\server\share\repo\a.rs"));
        assert!(!is_posix_path("C:relative.rs"));
    }

    /// The two #12 gates that need no existing posix work tree: the
    /// path-class stand-in (a win32 workspace root never approves, which is
    /// v2's `pathClass !== 'posix'` early return for a local Windows runtime)
    /// and the `findWorkTree(cwd)` requirement (a posix workspace without a
    /// `.git` anywhere up the tree is not approved either).
    #[test]
    fn test_git_cwd_write_approve_requires_a_posix_work_tree() {
        let win_engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            git_cwd: Some("D:/repo".into()),
            ..Default::default()
        });
        for path in [
            r"D:\repo\src\main.rs",
            "D:/repo/src/main.rs",
            r"\\server\share\repo\a.rs",
        ] {
            let verdict = win_engine.evaluate("Write", &json!({ "path": path }));
            assert_eq!(verdict.decision, VerdictDecision::Ask, "path: {path}");
            assert_eq!(verdict.policy_name, "FallbackAsk", "path: {path}");
        }
        // A posix-flavoured target does not rescue a win32 workspace: the
        // runtime's class is what v2 reads, not the target's flavour.
        let outside = win_engine.evaluate("Write", &json!({ "path": "/workspace/project/a.rs" }));
        assert_eq!(outside.decision, VerdictDecision::Ask);
        assert_eq!(outside.policy_name, "FallbackAsk");

        // A posix workspace root with no `.git` up the tree fails
        // `findWorkTree(cwd)` and takes the policy out of the chain.
        let no_tree = tempfile::tempdir().unwrap();
        let root = no_tree.path().to_string_lossy().to_string();
        let engine = PermissionEngine::with_workspace(
            PolicySnapshot {
                mode: PermissionMode::Manual,
                git_cwd: Some(root.clone()),
                ..Default::default()
            },
            Some(root.clone()),
            Vec::new(),
        );
        let verdict = engine.evaluate("Write", &json!({ "path": format!("{root}/a.rs") }));
        assert_eq!(verdict.decision, VerdictDecision::Ask, "no work tree");
        assert_eq!(verdict.policy_name, "FallbackAsk");

        // No workspace cwd at all (v2's `cwd.length === 0`) is no approval
        // either, whatever the target looks like.
        let bare = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let verdict = bare.evaluate("Write", &json!({ "path": "/workspace/project/a.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");
    }

    /// v2 `findGitWorkTree` (`app/git/workTree.ts`): the walk finds a `.git`
    /// directory or a `.git` *file* carrying a `gitdir:` pointer (a linked
    /// worktree / submodule), and reports nothing for a plain file or a tree
    /// without one.
    #[test]
    fn test_find_git_work_tree() {
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(
            find_git_work_tree(&plain.path().to_string_lossy()),
            None,
            "a stray temp dir is not a work tree"
        );

        let checkout = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(checkout.path().join(".git")).unwrap();
        let nested = checkout.path().join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            find_git_work_tree(&nested.to_string_lossy()),
            Some(checkout.path().to_path_buf()),
            "the walk starts at the cwd and finds the checkout root above it"
        );

        let linked = tempfile::tempdir().unwrap();
        std::fs::write(
            linked.path().join(".git"),
            "gitdir: /elsewhere/.git/worktrees/linked\n",
        )
        .unwrap();
        assert_eq!(
            find_git_work_tree(&linked.path().to_string_lossy()),
            Some(linked.path().to_path_buf()),
            "a `gitdir:` pointer file is a work tree too"
        );

        let broken = tempfile::tempdir().unwrap();
        std::fs::write(broken.path().join(".git"), "not a pointer\n").unwrap();
        assert_eq!(find_git_work_tree(&broken.path().to_string_lossy()), None);

        assert_eq!(find_git_work_tree(""), None, "no cwd, no work tree");
        assert_eq!(
            find_git_work_tree("relative/dir"),
            None,
            "a cwd that is not absolute has nowhere to walk up from"
        );
    }

    /// v2 `isWithinDirectory` / `isWithinWorkspace`: component-wise containment
    /// with `.` / `..` resolved, plus the `additionalDirs` union.
    #[test]
    fn test_workspace_containment_is_component_wise() {
        assert!(is_within_directory("/repo/src/lib.rs", "/repo"));
        assert!(is_within_directory("/repo", "/repo"));
        assert!(!is_within_directory("/repo2/src/lib.rs", "/repo"));
        assert!(!is_within_directory("/other/repo/x", "/repo"));
        assert!(is_within_directory("/repo/a/../b/x.rs", "/repo/b"));
        assert!(!is_within_directory("/repo/../elsewhere/x.rs", "/repo"));
        assert!(
            !is_within_directory("/repo/x", "repo"),
            "the leading `/` is its own component, so an absolute path is never \
             inside the relative base of the same name"
        );
        assert!(
            !is_within_directory("repo/src/lib.rs", "/repo"),
            "a relative candidate is not the absolute path it resolves to"
        );
        assert!(
            !is_within_directory("/repo/x", ""),
            "an empty base has no components, so nothing is inside it"
        );

        let extras = vec!["/granted".to_string()];
        assert!(is_within_workspace("/repo/x.rs", "/repo", &extras));
        assert!(
            is_within_workspace("/granted/x.rs", "/repo", &extras),
            "an /add-dir root is inside the boundary"
        );
        assert!(is_within_workspace("/granted/sub/x.rs", "/repo", &extras));
        assert!(!is_within_workspace("/granted2/x.rs", "/repo", &extras));
        assert!(
            is_within_workspace("src/lib.rs", "/repo", &extras),
            "a relative target resolves against the workspace cwd"
        );
        assert!(!is_within_workspace("/elsewhere/x.rs", "/repo", &extras));
    }

    /// Vectors the component-wise comparison must refuse. `starts_with` accepted
    /// every one of these, which is how a write escaped the workspace.
    #[test]
    fn test_workspace_containment_refuses_escape_shapes() {
        // `~` is not expanded here, so it is a literal component — a home-
        // relative path must never read as inside a workspace root.
        assert!(!is_within_directory("~/.ssh/config", "/"));
        assert!(!is_within_directory("/repo/~/x", "/repo/real"));
        // A trailing separator is a separator, not a new component: `/repo/`
        // and `/repo` are the same root, so a sibling that merely shares the
        // prefix is still outside.
        assert!(is_within_directory("/repo/x.rs", "/repo/"));
        assert!(is_within_directory("/repo/", "/repo"));
        assert!(!is_within_directory("/repo-evil/x.rs", "/repo/"));
        // Repeated separators collapse rather than creating empty components
        // that could line up with a base segment.
        assert!(is_within_directory("/repo//src//x.rs", "/repo"));
        // `..` that climbs past the root is clamped, not allowed to wrap.
        assert!(!is_within_directory("/../etc/passwd", "/repo"));
        // Backslashes are unified first, so a Windows-flavoured subject cannot
        // smuggle a separator past the component split.
        assert!(is_within_directory("\\repo\\src\\x.rs", "/repo"));
        assert!(!is_within_directory("\\repo2\\x.rs", "/repo"));
    }

    /// v2 `matchPermissionRule` (`permissionRules/matchesRule.ts`): the tool
    /// name is a **picomatch pattern**, not a literal — `Bash*` / `*` / `?`
    /// match, and `*` short-circuits before the pattern is consulted.
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

        // `?` is a single character in picomatch, so `Read?le` does not match.
        let engine_single = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            allow_rules: vec!["Read?le".into()],
            ..Default::default()
        });
        let verdict = engine_single.evaluate("ReadFile", &json!({ "path": "src/lib.rs" }));
        assert_ne!(verdict.policy_name, "UserConfiguredAllow");

        // A tool outside both patterns still asks.
        let verdict = engine.evaluate("Write", &json!({ "path": "src/lib.rs" }));
        assert_ne!(verdict.policy_name, "UserConfiguredAllow");
    }

    /// v2 `pathGlobMatch` defaults `caseInsensitivePaths` to **true**, so a
    /// path rule matches a differently-cased path on any platform.
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

    /// A deny rule must match the resolved target, not only the raw spelling:
    /// `/sub/../secret` reaches the same file as `/secret`, and matching the raw
    /// path let the traversal bypass the rule.
    #[test]
    fn test_rule_subject_resolves_dot_dot_traversal() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
        let root = std::fs::canonicalize(ws.path()).unwrap();
        let secret = std::fs::canonicalize(root.join("secret.txt")).unwrap();
        let rule_path = secret.to_string_lossy().replace('\\', "/");
        let engine = PermissionEngine::with_workspace(
            PolicySnapshot {
                mode: PermissionMode::Manual,
                deny_rules: vec![format!("Read({rule_path})")],
                ..Default::default()
            },
            Some(root.to_string_lossy().to_string()),
            Vec::new(),
        );
        let traversal = root.join("sub").join("..").join("secret.txt");
        let verdict = engine.evaluate("Read", &json!({ "path": traversal.to_string_lossy() }));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");
    }

    /// v2 `LEGACY_TOOL_NAME_ALIASES` (`permissionRules/matchesRule.ts:67-75`):
    /// the fold of `ReadMediaFile` into `Read` kept the old spelling matchable,
    /// so neither spelling of a `Read` rule can be side-stepped by calling the
    /// other tool.
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
        // The alias widens the tool name, not the argument pattern.
        let verdict = engine.evaluate("ReadMediaFile", &json!({ "path": "public.txt" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");

        // v2's own direction: a rule written under the legacy name still
        // constrains the surviving tool.
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            deny_rules: vec!["ReadMediaFile(secret.txt)".into()],
            ..Default::default()
        });
        let verdict = engine.evaluate("Read", &json!({ "path": "secret.txt" }));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");
    }

    /// v2 requires an argument rule to match the subject the **tool** declares
    /// (`permissionRules/matchesRule.ts:100`): Grep / Glob match the search
    /// pattern, the task tools the id / list scope, Skill the skill name and
    /// Agent the resolved profile. The fork's own path tools declare one too.
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

        // Grep / Glob rules bind the pattern, not the directory.
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
        // v2 `taskWaitTool.ts:150` uses `task_id ?? 'any'`.
        assert_eq!(
            denied("WaitFor(any)", "WaitFor", json!({})),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );
        // v2 `taskListTool.ts:29`: `active_only ?? true` selects the scope.
        assert_eq!(
            denied("TaskList(all)", "TaskList", json!({ "active_only": false })),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );
        assert_eq!(
            denied("Agent(coder)", "Agent", json!({ "prompt": "x" })),
            (VerdictDecision::Deny, "UserConfiguredDeny".to_string())
        );

        // The fork's own path tools are no longer exempt from path rules.
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

    /// The two ways an argument rule can miss are different and both must stay
    /// silent: the subject exists but the pattern does not match it, or the tool
    /// declares no subject at all — v2's `execution.matchesRule?.(...)` is
    /// `undefined`, so the rule can never fire.
    #[test]
    fn test_unmatched_argument_rule_and_inapplicable_rule_both_miss() {
        let engine = |rule: &str| {
            PermissionEngine::new(PolicySnapshot {
                mode: PermissionMode::Auto,
                deny_rules: vec![rule.into()],
                ..Default::default()
            })
        };

        // Argument carried, pattern unmatched.
        let verdict = engine("Skill(commit)").evaluate("Skill", &json!({ "skill": "review" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");

        // No matcher declared: the tool's arguments cannot satisfy the rule.
        let verdict = engine("TodoList(done)").evaluate("TodoList", &json!({ "items": ["x"] }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");

        // The same rule without arguments is a plain tool-name rule.
        let verdict = engine("TodoList").evaluate("TodoList", &json!({}));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");
    }

    /// v2 `matchRuleSubjects`: a leading `!` negates the argument pattern, so
    /// `Bash(!rm *)` denies every command except the ones starting with `rm`.
    /// The engine used to refuse such patterns outright, which silently
    /// dropped the rule.
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
    fn test_policy_chain_precedence_hierarchy() {
        // A. Deny beats Allow
        let engine_deny_vs_allow = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            deny_rules: vec!["Write(src/*)".into()],
            allow_rules: vec!["Write(src/*)".into()],
            ..Default::default()
        });
        let verdict = engine_deny_vs_allow.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");

        // B. Session history beats UserConfiguredAsk
        let engine_session_vs_ask = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            session_approvals: vec!["Write(src/*)".into()],
            ask_rules: vec!["Write(src/*)".into()],
            ..Default::default()
        });
        let verdict = engine_session_vs_ask.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "SessionApprovalHistory");

        // C. UserConfiguredAsk beats UserConfiguredAllow
        let engine_ask_vs_allow = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ask_rules: vec!["Write(src/*)".into()],
            allow_rules: vec!["Write(src/*)".into()],
            ..Default::default()
        });
        let verdict = engine_ask_vs_allow.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "UserConfiguredAsk");

        // D. UserConfiguredAllow beats SensitiveFileAccessAsk
        let engine_allow_sensitive = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            allow_rules: vec!["Read(.env)".into()],
            ..Default::default()
        });
        let verdict = engine_allow_sensitive.evaluate("Read", &json!({ "path": ".env" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "UserConfiguredAllow");
        assert_eq!(
            verdict.reason,
            Some("Allowed by user rule: Read(.env)".into())
        );

        // E. UserConfiguredAllow beats GitControlPathAccessAsk
        let engine_allow_git = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            allow_rules: vec!["Write(.git/config)".into()],
            ..Default::default()
        });
        let verdict = engine_allow_git.evaluate("Write", &json!({ "path": ".git/config" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "UserConfiguredAllow");
        assert_eq!(
            verdict.reason,
            Some("Allowed by user rule: Write(.git/config)".into())
        );
    }

    #[test]
    fn test_parse_permission_pattern() {
        // Valid patterns
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
        assert_eq!(rule.tool_name, "Bash");
        assert_eq!(rule.arg_pattern, Some("rm -rf *".into()));

        let rule = parse_permission_pattern("Write()").unwrap();
        assert_eq!(rule.tool_name, "Write");
        assert_eq!(rule.arg_pattern, None);

        let rule = parse_permission_pattern("Write(   )").unwrap();
        assert_eq!(rule.tool_name, "Write");
        assert_eq!(rule.arg_pattern, None);

        // Invalid patterns
        assert!(parse_permission_pattern("").is_none());
        assert!(parse_permission_pattern("   ").is_none());
        assert!(parse_permission_pattern("Write(").is_none());
        assert!(parse_permission_pattern(")").is_none());
        assert!(parse_permission_pattern("(arg)").is_none());
        assert!(parse_permission_pattern("  (arg)  ").is_none());
        assert!(parse_permission_pattern("Write(arg)trailing").is_none());
    }

    #[test]
    fn test_compiled_rule_semantics() {
        let path = RuleSubjectKind::Path;

        // Tool-only rule
        let rule = CompiledRule::compile("Write").unwrap();
        assert!(rule.matches("write", Some("src/main.rs"), path));
        assert!(rule.matches("write", None, path));
        assert!(rule.matches("WRITE", Some("anything"), path));
        assert!(!rule.matches("read", Some("src/main.rs"), path));

        // Rule with glob pattern
        let rule = CompiledRule::compile("Write(src/*.rs)").unwrap();
        assert!(rule.matches("write", Some("src/main.rs"), path));
        assert!(!rule.matches("write", Some("tests/test.rs"), path));
        assert!(!rule.matches("write", None, path));
        assert!(!rule.matches("read", Some("src/main.rs"), path));

        // A `./`-prefixed target matches a rule written without it (v2
        // `stripLeadingDotSlash`).
        assert!(rule.matches("write", Some("./src/main.rs"), path));

        // Wildcard tool rule `*(*.rs)`
        let rule = CompiledRule::compile("*(*.rs)").unwrap();
        assert!(rule.matches("write", Some("src/main.rs"), path));
        assert!(rule.matches("read", Some("src/main.rs"), path));
        assert!(rule.matches("edit", Some("src/lib.rs"), path));
        assert!(!rule.matches("write", Some("src/main.py"), path));

        // `**` crosses directory boundaries; both match inside `src`.
        let rule = CompiledRule::compile("Write(src/**)").unwrap();
        assert!(rule.matches("write", Some("src/deep/nested/main.rs"), path));
        assert!(!rule.matches("write", Some("tests/test.rs"), path));

        // The plain strategy keeps case (v2 `globMatch` has no `nocase`).
        let rule = CompiledRule::compile("Bash(cargo *)").unwrap();
        assert!(rule.matches("bash", Some("cargo build"), RuleSubjectKind::Glob));
        assert!(!rule.matches("bash", Some("Cargo build"), RuleSubjectKind::Glob));

        // Invalid glob pattern should fail compilation fast (not silently match everything)
        assert!(CompiledRule::compile("Write([unclosed").is_none());

        // Empty rule fails
        assert!(CompiledRule::compile("").is_none());
    }

    #[test]
    fn test_parse_permission_pattern_keeps_negation() {
        let parsed = parse_permission_pattern("Bash(!rm *)").unwrap();
        assert_eq!(parsed.tool_name, "Bash");
        assert_eq!(parsed.arg_pattern.as_deref(), Some("!rm *"));
    }

    #[test]
    fn test_sensitive_file_rules_match_v2() {
        // Positive cases: .env variants (v2 `ENV_PREFIX` + basenames)
        assert!(crate::native::file_type::is_sensitive_file(".env"));
        assert!(crate::native::file_type::is_sensitive_file(".env.local"));
        assert!(crate::native::file_type::is_sensitive_file(
            ".env.production"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            ".env.development.local"
        ));
        assert!(crate::native::file_type::is_sensitive_file(".ENV"));
        assert!(crate::native::file_type::is_sensitive_file(".Env.Test"));
        assert!(crate::native::file_type::is_sensitive_file("config/.env"));
        assert!(crate::native::file_type::is_sensitive_file(
            "backend/.env.production"
        ));

        // Positive cases: SSH private keys (v2 `SENSITIVE_BASENAMES`)
        assert!(crate::native::file_type::is_sensitive_file("id_rsa"));
        assert!(crate::native::file_type::is_sensitive_file("id_ed25519"));
        assert!(crate::native::file_type::is_sensitive_file("id_ecdsa"));
        assert!(crate::native::file_type::is_sensitive_file("~/.ssh/id_rsa"));
        assert!(crate::native::file_type::is_sensitive_file(
            "/root/.ssh/id_ed25519"
        ));
        assert!(crate::native::file_type::is_sensitive_file("ID_RSA"));
        // v2 prefix+separator and prefix+dot-variant: id_rsa.bak, id_rsa-prod,
        // credentials_backup, credentials.old, etc.
        assert!(crate::native::file_type::is_sensitive_file("id_rsa.bak"));
        assert!(crate::native::file_type::is_sensitive_file("id_rsa-prod"));
        assert!(crate::native::file_type::is_sensitive_file(
            "id_ed25519.old"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "credentials.bak"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "credentials_backup"
        ));

        // Positive cases: credentials basenames + path-suffix components
        assert!(crate::native::file_type::is_sensitive_file("credentials"));
        assert!(crate::native::file_type::is_sensitive_file(
            "~/.aws/credentials"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "/root/.gcp/credentials"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "path/to/.aws/credentials"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "path/to/.aws/credentials/extra"
        ));

        // Positive cases: Windows paths (backslashes normalised)
        assert!(crate::native::file_type::is_sensitive_file(
            "C:\\Users\\admin\\.ssh\\id_rsa"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "app\\config\\.env.local"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "C:\\path\\.aws\\credentials"
        ));

        // Negative cases: exemptions (v2 `ENV_EXEMPTIONS` + `PUBLIC_KEY_BASENAMES`)
        assert!(!crate::native::file_type::is_sensitive_file(".env.example"));
        assert!(!crate::native::file_type::is_sensitive_file(".env.sample"));
        assert!(!crate::native::file_type::is_sensitive_file(
            ".env.template"
        ));
        assert!(!crate::native::file_type::is_sensitive_file("id_rsa.pub"));
        assert!(crate::native::file_type::is_sensitive_file("id_rsa"));
        assert!(!crate::native::file_type::is_sensitive_file(
            "id_ed25519.pub"
        ));
        assert!(!crate::native::file_type::is_sensitive_file("id_ecdsa.pub"));
        assert!(!crate::native::file_type::is_sensitive_file(".ENV.example"));

        // Negative cases: safe non-sensitive files. v2 does NOT flag
        // arbitrary `.pem` / `.key` / `.pfx` — only the dot-variants of
        // `id_rsa` / `id_ed25519` / `id_ecdsa` / `credentials`.
        assert!(!crate::native::file_type::is_sensitive_file(
            "environment.ts"
        ));
        assert!(!crate::native::file_type::is_sensitive_file("dotenv.js"));
        assert!(!crate::native::file_type::is_sensitive_file(
            "environment.json"
        ));
        assert!(!crate::native::file_type::is_sensitive_file("key.txt"));
        assert!(!crate::native::file_type::is_sensitive_file("keyboard.rs"));
        assert!(!crate::native::file_type::is_sensitive_file("README.md"));
        assert!(!crate::native::file_type::is_sensitive_file("src/main.rs"));
        assert!(!crate::native::file_type::is_sensitive_file("server.key"));
        assert!(!crate::native::file_type::is_sensitive_file("cert.pem"));
        assert!(!crate::native::file_type::is_sensitive_file("identity.pfx"));
        assert!(!crate::native::file_type::is_sensitive_file("certs/ca.pem"));
        assert!(!crate::native::file_type::is_sensitive_file(
            "keys/secret.KEY"
        ));
    }

    #[test]
    fn test_is_git_control_path() {
        // Positive cases
        assert!(is_git_control_path(".git"));
        assert!(is_git_control_path(".GIT"));
        assert!(is_git_control_path(".git/config"));
        assert!(is_git_control_path(".git/HEAD"));
        assert!(is_git_control_path(".git/index"));
        assert!(is_git_control_path(".git/refs/heads/main"));
        assert!(is_git_control_path(".git/hooks/pre-commit"));
        assert!(is_git_control_path("repo/.git"));
        assert!(is_git_control_path("repo/.git/config"));
        assert!(is_git_control_path("packages/app/.git/HEAD"));
        assert!(is_git_control_path("a/b/c/.git/objects"));
        assert!(is_git_control_path("repo\\.git"));
        assert!(is_git_control_path("repo\\.git\\config"));
        assert!(is_git_control_path("C:\\repo\\.git\\HEAD"));

        // Negative cases
        assert!(!is_git_control_path(".gitignore"));
        assert!(!is_git_control_path(".gitattributes"));
        assert!(!is_git_control_path(".gitmodules"));
        assert!(!is_git_control_path(".github/workflows/ci.yml"));
        assert!(!is_git_control_path(".github/CODEOWNERS"));
        assert!(!is_git_control_path("git.rs"));
        assert!(!is_git_control_path("git_controller.ts"));
        assert!(!is_git_control_path("src/git/mod.rs"));
    }

    #[test]
    fn test_extract_rule_subject() {
        let subject = |tool: &str, args: Value| {
            extract_rule_subject(tool, &args).map(|subject| (subject.value, subject.kind))
        };
        let glob = |value: &str| Some((value.to_string(), RuleSubjectKind::Glob));
        let path = |value: &str| Some((value.to_string(), RuleSubjectKind::Path));

        // v2 path-strategy tools
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

        // v2 `grepTool.ts:119` / `globTool.ts:133` bind the search pattern.
        assert_eq!(
            subject("grep", json!({ "pattern": "fn main", "path": "src/lib" })),
            glob("fn main")
        );
        assert_eq!(
            subject("glob", json!({ "pattern": "*.rs", "path": "tests" })),
            glob("*.rs")
        );

        // URL-bearing tools
        assert_eq!(
            subject("fetchurl", json!({ "url": "https://api.test/data" })),
            glob("https://api.test/data")
        );
        assert_eq!(
            subject("fetch_url", json!({ "url": "https://api.test/data" })),
            glob("https://api.test/data")
        );

        // Query-bearing tools
        assert_eq!(
            subject("websearch", json!({ "query": "rust docs" })),
            glob("rust docs")
        );
        assert_eq!(
            subject("web_search", json!({ "query": "rust docs" })),
            glob("rust docs")
        );

        // Command-bearing tools
        assert_eq!(
            subject("bash", json!({ "command": "cargo test" })),
            glob("cargo test")
        );
        // v2 `powershellTool.ts` `matchesRule` uses the same subject: the
        // script text, matched by the rule's glob.
        assert_eq!(
            subject("powershell", json!({ "command": "Get-Service" })),
            glob("Get-Service")
        );

        // Skill / task subjects
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

        // The fork's own path tools
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

        // Missing field or non-string field returns None
        assert_eq!(extract_rule_subject("read", &json!({})), None);
        assert_eq!(extract_rule_subject("read", &json!({ "path": 123 })), None);
        assert_eq!(extract_rule_subject("read", &json!({ "path": true })), None);

        // Unknown tool returns None
        assert_eq!(
            extract_rule_subject("unknown_tool", &json!({ "path": "foo" })),
            None
        );
    }

    #[test]
    fn test_serde_and_helpers() {
        // PermissionEngine::mode()
        let engine_manual = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        assert_eq!(engine_manual.mode(), PermissionMode::Manual);

        let engine_auto = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            ..Default::default()
        });
        assert_eq!(engine_auto.mode(), PermissionMode::Auto);

        let engine_yolo = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });
        assert_eq!(engine_yolo.mode(), PermissionMode::Yolo);

        // LocalPermissionVerdict::is_allow()
        let allow_verdict = LocalPermissionVerdict {
            decision: VerdictDecision::Allow,
            policy_name: "TestPolicy".into(),
            session_approval_rule: None,
            reason: None,
        };
        assert!(allow_verdict.is_allow());

        let deny_verdict = LocalPermissionVerdict {
            decision: VerdictDecision::Deny,
            policy_name: "TestPolicy".into(),
            session_approval_rule: None,
            reason: Some("reason".into()),
        };
        assert!(!deny_verdict.is_allow());

        let ask_verdict = LocalPermissionVerdict {
            decision: VerdictDecision::Ask,
            policy_name: "TestPolicy".into(),
            session_approval_rule: None,
            reason: Some("reason".into()),
        };
        assert!(!ask_verdict.is_allow());

        // Serde roundtrips
        let mode_json = serde_json::to_string(&PermissionMode::Auto).unwrap();
        assert_eq!(mode_json, "\"auto\"");
        let parsed_mode: PermissionMode = serde_json::from_str("\"yolo\"").unwrap();
        assert_eq!(parsed_mode, PermissionMode::Yolo);

        let verdict_json = serde_json::to_string(&allow_verdict).unwrap();
        let parsed_verdict: LocalPermissionVerdict = serde_json::from_str(&verdict_json).unwrap();
        assert_eq!(parsed_verdict, allow_verdict);

        // PolicySnapshot with HookDef deserialization
        let snapshot_json = json!({
            "mode": "auto",
            "deny_rules": ["Bash(rm *)"],
            "pre_tool_hooks": [
                {
                    "event": "PreToolUse",
                    "matcher": "Bash",
                    "command": "echo check",
                    "timeout": 15
                }
            ]
        });
        let snapshot: PolicySnapshot = serde_json::from_value(snapshot_json).unwrap();
        assert_eq!(snapshot.mode, PermissionMode::Auto);
        assert_eq!(snapshot.deny_rules, vec!["Bash(rm *)"]);
        assert_eq!(snapshot.pre_tool_hooks.len(), 1);
        assert_eq!(snapshot.pre_tool_hooks[0].event, "PreToolUse");
        assert_eq!(snapshot.pre_tool_hooks[0].matcher, "Bash");
        assert_eq!(snapshot.pre_tool_hooks[0].command, "echo check");
        assert_eq!(snapshot.pre_tool_hooks[0].timeout, Some(15));
    }

    /// v2 "does not let session approval history exempt dangerous commands"
    /// (permissionPolicyService.test.ts): the dangerous guard runs before the
    /// session-approval policy, so an earlier "approve for session" on `Bash`
    /// must not silence it for a destructive command.
    #[test]
    fn a_session_approval_does_not_exempt_a_dangerous_command() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            session_approvals: vec!["Bash".into()],
            ..Default::default()
        });

        let dangerous = engine.evaluate("bash", &json!({ "command": "rm -rf /" }));
        assert_eq!(dangerous.decision, VerdictDecision::Ask);
        assert_eq!(dangerous.policy_name, "DangerousCommandAsk");

        // The same approval still covers a benign command: the guard bypasses
        // destructive commands, it does not override the history wholesale.
        let benign = engine.evaluate("bash", &json!({ "command": "git status" }));
        assert_eq!(benign.decision, VerdictDecision::Allow);
        assert_eq!(benign.policy_name, "SessionApprovalHistory");
    }

    /// v2 `powershellTool.ts`: the tool carries an approval rule subject
    /// (`command`) but is absent from `DEFAULT_APPROVE_TOOLS` and from
    /// `dangerous-command-ask.ts` (that guard matches the literal name
    /// `Bash`). A `PowerShell(...)` session rule therefore covers exactly the
    /// scripts it names, an unapproved script asks, and a script that *reads*
    /// like a destructive bash command is not analyzed as one.
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

        // The bash analyzer never runs on a PowerShell script: v2's guard is
        // keyed to the tool name, so this is an ordinary prompt, not
        // `DangerousCommandAsk`.
        let looks_destructive = engine.evaluate("PowerShell", &json!({ "command": "rm -rf /" }));
        assert_eq!(looks_destructive.decision, VerdictDecision::Ask);
        assert_eq!(looks_destructive.policy_name, "FallbackAsk");
    }

    /// v2 "does not use git-cwd approval in auto mode"
    /// (permissionPolicyService.test.ts): auto approves before the git-cwd
    /// policy runs, so a write outside the work tree is not judged by it.
    #[test]
    fn auto_mode_does_not_consume_the_git_cwd_approval() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            git_cwd: Some("/tmp/repo".into()),
            ..Default::default()
        });
        let verdict = engine.evaluate("Write", &json!({ "path": "/tmp/elsewhere/file.txt" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");
    }

    /// v2 mode gating (dangerous-command-ask.ts, under
    /// `agent/permissionPolicy/policies/`): a dangerous command asks in
    /// manual and yolo alike; auto skips the policy entirely, so AutoModeApprove
    /// decides. Benign commands are approved in auto and yolo; manual falls
    /// through to FallbackAsk.
    #[test]
    fn test_dangerous_bash_command_asks_in_manual_and_yolo() {
        let manual = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let auto = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            ..Default::default()
        });
        let yolo = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        for cmd in ["sudo reboot", "shutdown -h now", "rm -rf /", "format C: /q"] {
            // v2 asks for a *dangerous* command in manual and yolo alike: only
            // `mode === 'auto'` returns before the verdict.
            for engine in [&manual, &yolo] {
                let verdict = engine.evaluate("bash", &json!({ "command": cmd }));
                assert_eq!(verdict.decision, VerdictDecision::Ask, "cmd: {cmd}");
                assert_eq!(verdict.policy_name, "DangerousCommandAsk");
                assert_eq!(
                    verdict.reason.as_deref(),
                    Some("High-risk shell command requires approval")
                );
            }
            // Auto skipped the policy before the verdict, so AutoModeApprove
            // owns the call (v2 `if (mode === 'auto') return undefined`).
            let verdict = auto.evaluate("bash", &json!({ "command": cmd }));
            assert_eq!(verdict.decision, VerdictDecision::Allow, "cmd: {cmd}");
            assert_eq!(verdict.policy_name, "AutoModeApprove");
        }

        // A benign command is untouched by the policy: Auto/Yolo approve it
        // outright, manual falls through the rest of the chain to FallbackAsk
        // (v2 behavior: a non-dangerous, non-allow-listed Bash call asks in
        // manual).
        for engine in [&auto, &yolo] {
            let benign = engine.evaluate("bash", &json!({ "command": "git status" }));
            assert_eq!(benign.decision, VerdictDecision::Allow, "benign command");
        }
        let benign = manual.evaluate("bash", &json!({ "command": "git status" }));
        assert_eq!(benign.decision, VerdictDecision::Ask);
        assert_eq!(benign.policy_name, "FallbackAsk");
    }

    /// v2 `isDangerousCommandGuardEnabled`: `[permission]
    /// dangerousCommandGuard = false` skips the whole policy — the remaining
    /// policies decide every Bash call.
    #[test]
    fn test_dangerous_command_guard_off_skips_the_policy() {
        let guard_on = PermissionEngine::new(PolicySnapshot {
            dangerous_command_guard: true,
            ..Default::default()
        });
        let guard_off = PermissionEngine::new(PolicySnapshot {
            dangerous_command_guard: false,
            ..Default::default()
        });

        let verdict = guard_on.evaluate("bash", &json!({ "command": "sudo reboot" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "DangerousCommandAsk");

        // Guard off: the ask falls through to FallbackAsk (manual mode).
        let verdict = guard_off.evaluate("bash", &json!({ "command": "sudo reboot" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");

        // Default is on: a snapshot without the field keeps the guard.
        let default = PermissionEngine::new(PolicySnapshot::default());
        let verdict = default.evaluate("bash", &json!({ "command": "sudo reboot" }));
        assert_eq!(verdict.policy_name, "DangerousCommandAsk");
    }

    /// Upstream #3869: a command the analyzer cannot read is approved in auto and
    /// yolo, and asks in manual — v2 returns undefined for auto *before* the
    /// verdict and for yolo *after* it, asking with `unanalyzable_command`
    /// otherwise.
    #[test]
    fn test_unanalyzable_bash_command_asks_except_in_auto_and_yolo() {
        let manual = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let auto = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            ..Default::default()
        });
        let yolo = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        for cmd in ["echo \"unterminated", "$CMD --force"] {
            let verdict = manual.evaluate("bash", &json!({ "command": cmd }));
            assert_eq!(verdict.decision, VerdictDecision::Ask, "cmd: {cmd}");
            assert_eq!(verdict.policy_name, "DangerousCommandAsk");
            assert!(
                verdict
                    .reason
                    .as_deref()
                    .is_some_and(|r| r.contains("could not be statically analyzed")),
                "reason: {:?}",
                verdict.reason
            );
        }

        // Auto and yolo fall through to their own approve policies.
        let verdict = auto.evaluate("bash", &json!({ "command": "echo \"unterminated" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");
        let verdict = yolo.evaluate("bash", &json!({ "command": "echo \"unterminated" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
    }

    // Headless sessions (`kimi -p`, upstream bootstrap `nonInteractive`) skip
    // the DangerousCommandAsk policy: there is no human to answer the prompt,
    // so the remaining policies decide — Auto approves, Manual falls through
    // to the host chain (which for a headless run cannot answer, and the
    // command is denied there).
    #[test]
    fn test_non_interactive_session_skips_dangerous_command_ask() {
        let auto = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            non_interactive: true,
            ..Default::default()
        });
        let verdict = auto.evaluate("bash", &json!({ "command": "sudo reboot" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");
    }

    /// A benign command with a quoted, non-ASCII path is allowed in auto and
    /// yolo and only asks in manual.
    ///
    /// This pins the chain's per-mode verdict for the shape behind the report
    /// "switched to yolo and still got an approval prompt". If the engine starts
    /// asking here, the regression is in the chain; while this stays green and
    /// the CLI prompts anyway, the mode never reached the engine — which is what
    /// the host-side `'plan'` mode used to do (see the snapshot test below).
    #[test]
    fn test_benign_command_verdict_tracks_permission_mode() {
        let cmd = r#"ls "D:\work\示例 项目\src\machines" 2>&1 | head -50"#;
        for (mode, policy, expected) in [
            (PermissionMode::Manual, "FallbackAsk", VerdictDecision::Ask),
            (
                PermissionMode::Auto,
                "AutoModeApprove",
                VerdictDecision::Allow,
            ),
            (
                PermissionMode::Yolo,
                "YoloModeApprove",
                VerdictDecision::Allow,
            ),
        ] {
            let engine = PermissionEngine::new(PolicySnapshot {
                mode,
                ..Default::default()
            });
            // Both spellings reach the chain: the host advertises `Bash`, while
            // the policy DSL also matches the lowercase tool name.
            for tool in ["Bash", "bash"] {
                let verdict = engine.evaluate(tool, &json!({ "command": cmd }));
                assert_eq!(verdict.decision, expected, "{mode:?} / {tool} decision");
                assert_eq!(verdict.policy_name, policy, "{mode:?} / {tool} policy");
            }
        }
    }

    /// Every shape the host's `buildPolicySnapshot` emits must deserialize.
    ///
    /// The napi boundary keeps only the parsed snapshot, so a single rejected
    /// field used to drop the rules *and* the hooks with it and leave the engine
    /// without a local chain — every tool call then round-tripped to the host's
    /// `check_permission`, which prompts in every mode.
    #[test]
    fn test_host_policy_snapshot_shapes_deserialize() {
        let full = json!({
            "mode": "yolo",
            "deny_rules": [],
            "ask_rules": ["Bash(rm *)"],
            "allow_rules": [],
            "session_approvals": [],
            "git_cwd": "/workspace",
            "tools_filter": { "enabled": [], "disabled": [] },
            "pre_tool_hooks": [
                { "event": "PreToolUse", "matcher": "", "command": "echo hi",
                  "timeout": 30, "cwd": null, "env": { "A": "1" } }
            ]
        });
        let minimal = json!({ "mode": "yolo", "deny_rules": [], "ask_rules": [],
                              "allow_rules": [], "session_approvals": [],
                              "git_cwd": null, "pre_tool_hooks": [] });
        // A mode this engine does not model must degrade to `Unknown` rather
        // than failing the whole snapshot, or a host that invents one would
        // silently cost the user their rules and hooks. `plan` is the shape
        // that used to arrive this way (the host folded plan mode into the
        // permission mode); it is not one of v2's `manual | yolo | auto` and no
        // longer sent, but the tolerance stays — an unknown mode gets the
        // manual default, never yolo's auto-approval.
        let plan = json!({ "mode": "plan", "deny_rules": [], "ask_rules": [],
                           "allow_rules": [], "session_approvals": [],
                           "git_cwd": null, "pre_tool_hooks": [] });

        for (label, value, expected) in [
            ("full", &full, PermissionMode::Yolo),
            ("minimal", &minimal, PermissionMode::Yolo),
            ("plan", &plan, PermissionMode::Unknown),
        ] {
            let snapshot: PolicySnapshot = serde_json::from_value(value.clone())
                .unwrap_or_else(|error| panic!("{label} snapshot rejected: {error}"));
            assert_eq!(snapshot.mode, expected, "{label} mode");
        }

        // Nothing else rides on the mode: rules and hooks survive the round
        // trip, so a snapshot can never cost the user their configured policy.
        let snapshot: PolicySnapshot = serde_json::from_value(full).expect("full snapshot");
        assert_eq!(snapshot.ask_rules, vec!["Bash(rm *)".to_string()]);
        assert_eq!(snapshot.pre_tool_hooks.len(), 1);
        assert_eq!(snapshot.pre_tool_hooks[0].timeout, Some(30));
        assert_eq!(
            snapshot.pre_tool_hooks[0]
                .env
                .as_ref()
                .and_then(|env| env.get("A"))
                .map(String::as_str),
            Some("1")
        );

        // An unmodelled mode must not inherit yolo's auto-approval.
        let engine = PermissionEngine::new(serde_json::from_value(plan).expect("plan snapshot"));
        let verdict = engine.evaluate("Bash", &json!({ "command": "ls -la" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask, "unknown mode asks");
    }

    #[test]
    fn test_set_mode_switches_live_verdicts() {
        // Start manual: an ordinary Bash call falls through to FallbackAsk.
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let before = engine.evaluate("Bash", &json!({ "command": "cargo check" }));
        assert_eq!(before.decision, VerdictDecision::Ask);
        assert_eq!(before.policy_name, "FallbackAsk");

        // Live switch to yolo — the same call is now approved without a
        // pipeline rebuild, so a mode change reaches the turn already running.
        engine.set_mode(PermissionMode::Yolo);
        let after = engine.evaluate("Bash", &json!({ "command": "cargo check" }));
        assert_eq!(after.decision, VerdictDecision::Allow);
        assert_eq!(after.policy_name, "YoloModeApprove");
        assert_eq!(engine.mode(), PermissionMode::Yolo);

        // Switching back to manual restores the ask verdict.
        engine.set_mode(PermissionMode::Manual);
        let again = engine.evaluate("Bash", &json!({ "command": "cargo check" }));
        assert_eq!(again.decision, VerdictDecision::Ask);

        // Auto approves outright; a dangerous command still passes (the
        // dangerous-ask policy is skipped before it runs).
        engine.set_mode(PermissionMode::Auto);
        let auto = engine.evaluate("Bash", &json!({ "command": "sudo rm -rf /" }));
        assert_eq!(auto.decision, VerdictDecision::Allow);
        assert_eq!(auto.policy_name, "AutoModeApprove");
    }

    /// v2 `broadcastPermissionMode` skips agents pinned to their own mode: a
    /// session switch reaches unbound agents, a tower worker stays on `auto`.
    #[test]
    fn broadcast_skips_pinned_agent_permission_modes() {
        let modes = std::sync::Arc::new(AgentPermissionModes::default());
        modes.set_session_mode(PermissionMode::Manual);
        modes.bind("agent-7", PermissionMode::Auto);

        assert_eq!(modes.mode_for("agent-7"), Some(PermissionMode::Auto));
        assert_eq!(modes.mode_for("agent-8"), Some(PermissionMode::Manual));

        // The user toggles the session to yolo: the unbound agent follows, the
        // pinned one does not.
        modes.set_session_mode(PermissionMode::Yolo);
        assert_eq!(modes.mode_for("agent-7"), Some(PermissionMode::Auto));
        assert_eq!(modes.mode_for("agent-8"), Some(PermissionMode::Yolo));
    }

    /// The engine evaluates each call under the executing agent's mode, even
    /// when its own session mode differs.
    #[test]
    fn evaluate_for_agent_uses_the_agents_own_mode() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let modes = std::sync::Arc::new(AgentPermissionModes::default());
        modes.set_session_mode(PermissionMode::Manual);
        modes.bind("tower-worker-1", PermissionMode::Auto);
        engine.set_agent_permission_modes(modes);

        let write = json!({ "path": "src/main.rs" });
        assert_eq!(
            engine.evaluate_for_agent("main", "Write", &write).decision,
            VerdictDecision::Ask,
            "the main agent keeps the session mode"
        );
        let pinned = engine.evaluate_for_agent("tower-worker-1", "Write", &write);
        assert_eq!(pinned.decision, VerdictDecision::Allow);
        assert_eq!(pinned.policy_name, "AutoModeApprove");
    }
}
