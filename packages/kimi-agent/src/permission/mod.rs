use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::agent::guardian::guardian_service as guardian;
pub use crate::agent::permission_mode::permission_mode_service::{AgentPermissionModes, SharedAgentPermissionModes};
use crate::agent::permission_policy::policies::path_utils::legacy_file_accesses;
pub use crate::agent::permission_policy::types::PermissionMode;
use crate::agent::permission_rules::matches_rule::{
    RuleSubject, compile_rules, extract_rule_subject,
};
pub use crate::agent::permission_rules::matches_rule::{CompiledRule, InvalidPermissionRule};
use crate::tool::rule_match::rule_subject_variants;
use crate::turn_loop::types::LLMMessage;

pub use crate::agent::permission_policy::policies::git_cwd_write_approve::is_posix_path;
pub use crate::agent::permission_policy::policies::path_utils::is_legacy_git_control_path as is_git_control_path;
pub use crate::agent::permission_rules::matches_rule::{
    ParsedRule, RuleSubjectKind, parse_permission_pattern,
};
pub use crate::app::git::work_tree::find_local_git_work_tree as find_git_work_tree;
pub use crate::features::externalHooks::internal::types::{HOOK_EVENT_TYPES, HookDef};
pub use crate::tool::path_access::{
    is_within_directory_posix as is_within_directory,
    is_within_workspace_posix as is_within_workspace,
};


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
    #[serde(default)]
    pub tools_filter: Option<crate::tools::tool_policy::ToolsFilter>,
    #[serde(default)]
    pub pre_tool_hooks: Vec<HookDef>,
    #[serde(default)]
    pub rule_reasons: HashMap<String, String>,
    #[serde(default)]
    pub non_interactive: bool,
    #[serde(default = "default_true")]
    pub dangerous_command_guard: bool,
}

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
            rule_reasons: HashMap::new(),
            non_interactive: false,
            dangerous_command_guard: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPermissionVerdict {
    pub decision: VerdictDecision,
    pub policy_name: String,
    pub reason: Option<String>,
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

pub(crate) struct LegacyPolicyContext<'a> {
    pub tool_name: &'a str,
    pub tool_lower: &'a str,
    pub args: &'a Value,
    pub mode: PermissionMode,
    pub subject: Option<&'a RuleSubject>,
    pub rule_subjects: &'a [String],
    pub path_subject: Option<&'a str>,
    pub file_access: &'a [String],
    pub snapshot: &'a PolicySnapshot,
    pub workspace_dir: Option<&'a str>,
    pub additional_dirs: &'a [String],
    pub has_compaction_summary: bool,
}

pub type ConversationProvider = Arc<dyn Fn() -> Vec<LLMMessage> + Send + Sync>;

pub struct PermissionEngine {
    snapshot: PolicySnapshot,
    mode: RwLock<PermissionMode>,
    compiled_deny: Vec<CompiledRule>,
    compiled_ask: Vec<CompiledRule>,
    compiled_allow: Vec<CompiledRule>,
    compiled_session: Vec<CompiledRule>,
    session_records: RwLock<Vec<CompiledRule>>,
    invalid_rules: Vec<InvalidPermissionRule>,
    workspace_root: Option<String>,
    additional_dirs: Vec<String>,
    conversation: OnceLock<ConversationProvider>,
    guardian: OnceLock<Arc<guardian::GuardianService>>,
    agent_modes: OnceLock<SharedAgentPermissionModes>,
}

impl PermissionEngine {
    pub fn new(snapshot: PolicySnapshot) -> Self {
        Self::with_workspace(snapshot, None, Vec::new())
    }

    pub fn with_workspace(
        snapshot: PolicySnapshot,
        workspace_root: Option<String>,
        additional_dirs: Vec<String>,
    ) -> Self {
        let mut invalid_rules = Vec::new();
        let compiled_deny = compile_rules(&snapshot.deny_rules, "deny", &mut invalid_rules);
        let compiled_ask = compile_rules(&snapshot.ask_rules, "ask", &mut invalid_rules);
        let compiled_allow = compile_rules(&snapshot.allow_rules, "allow", &mut invalid_rules);
        let compiled_session =
            compile_rules(&snapshot.session_approvals, "session", &mut invalid_rules);
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

    pub fn invalid_rules(&self) -> &[InvalidPermissionRule] {
        &self.invalid_rules
    }

    pub fn remember_session_approval(&self, rule: &str) -> bool {
        let rule = rule.trim();
        if rule.is_empty()
            || self
                .compiled_session
                .iter()
                .any(|compiled| compiled.raw_rule == rule)
        {
            return false;
        }
        let Some(compiled) = CompiledRule::compile(rule) else {
            return false;
        };
        let mut records = self
            .session_records
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if records.iter().any(|record| record.raw_rule == rule) {
            return false;
        }
        records.push(compiled);
        true
    }

    pub fn session_approval_rules(&self) -> Vec<String> {
        self.session_records
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|record| record.raw_rule.clone())
            .collect()
    }

    pub fn mode(&self) -> PermissionMode {
        *self.mode.read().unwrap_or_else(|error| error.into_inner())
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.write().unwrap_or_else(|error| error.into_inner()) = mode;
    }

    pub fn set_agent_permission_modes(&self, modes: SharedAgentPermissionModes) {
        let _ = self.agent_modes.set(modes);
    }

    pub fn agent_permission_modes(&self) -> Option<&SharedAgentPermissionModes> {
        self.agent_modes.get()
    }

    pub fn set_conversation_provider(&self, provider: ConversationProvider) {
        let _ = self.conversation.set(provider);
    }

    pub fn conversation_snapshot(&self) -> Vec<LLMMessage> {
        self.conversation
            .get()
            .map(|provider| provider())
            .unwrap_or_default()
    }

    pub fn has_compaction_summary(&self) -> bool {
        self.conversation.get().is_some_and(|provider| {
            provider()
                .iter()
                .any(crate::compaction::is_compaction_summary_message)
        })
    }

    pub fn set_guardian_reviewer(&self, reviewer: Arc<guardian::GuardianService>) {
        let _ = self.guardian.set(reviewer);
    }

    pub fn guardian_reviewer(&self) -> Option<&Arc<guardian::GuardianService>> {
        self.guardian.get()
    }

    fn workspace_dir(&self) -> Option<&str> {
        self.workspace_root
            .as_deref()
            .or(self.snapshot.git_cwd.as_deref())
            .filter(|dir| !dir.is_empty())
    }

    pub fn evaluate(&self, tool_name: &str, args: &Value) -> LocalPermissionVerdict {
        self.evaluate_in_mode(tool_name, args, self.mode(), None)
    }

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
            Some(mode) => self.evaluate_in_mode(tool_name, args, mode, None),
            None => self.evaluate(tool_name, args),
        }
    }

    pub fn evaluate_for_agent_with_accesses(
        &self,
        agent_id: &str,
        tool_name: &str,
        args: &Value,
        accesses: &crate::turn_loop::types::ToolAccesses,
    ) -> LocalPermissionVerdict {
        match self
            .agent_modes
            .get()
            .and_then(|modes| modes.mode_for(agent_id))
        {
            Some(mode) => self.evaluate_in_mode(tool_name, args, mode, Some(accesses)),
            None => self.evaluate_in_mode(tool_name, args, self.mode(), Some(accesses)),
        }
    }

    fn evaluate_in_mode(
        &self,
        tool_name: &str,
        args: &Value,
        mode: PermissionMode,
        declared_accesses: Option<&crate::turn_loop::types::ToolAccesses>,
    ) -> LocalPermissionVerdict {
        let tool_lower = tool_name.to_ascii_lowercase();
        let subject = extract_rule_subject(&tool_lower, args);
        let subject_variants = rule_subject_variants(subject.as_ref(), self.workspace_dir());
        let rule_subjects = subject_variants.variants;
        let path_subject = subject_variants.resolved;
        let file_access = declared_accesses
            .map(|accesses| {
                accesses
                    .iter()
                    .filter_map(|access| match access {
                        crate::turn_loop::types::ToolResourceAccess::File(file) => {
                            Some(file.path.clone())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_else(|| legacy_file_accesses(&tool_lower, args));
        let context = LegacyPolicyContext {
            tool_name,
            tool_lower: &tool_lower,
            args,
            mode,
            subject: subject.as_ref(),
            rule_subjects: &rule_subjects,
            path_subject: path_subject.as_deref(),
            file_access: &file_access,
            snapshot: &self.snapshot,
            workspace_dir: self.workspace_dir(),
            additional_dirs: &self.additional_dirs,
            has_compaction_summary: self.has_compaction_summary(),
        };
        use crate::agent::permission_policy::policies::{
            auto_mode_approve, auto_mode_ask_user_question_deny, compaction_authorization_guard,
            dangerous_command_ask, default_tool_approve, fallback_ask, git_control_path_access_ask,
            git_cwd_write_approve, sensitive_file_access_ask, session_approval_history,
            user_configured_allow, user_configured_ask, user_configured_deny, yolo_mode_approve,
        };
        if let Some(verdict) = auto_mode_ask_user_question_deny::evaluate_legacy(&context) {
            return verdict;
        }
        if let Some(verdict) = user_configured_deny::evaluate_legacy(&context, &self.compiled_deny) {
            return verdict;
        }
        if let Some(verdict) = compaction_authorization_guard::evaluate_legacy(&context) {
            return verdict;
        }
        if let Some(verdict) = dangerous_command_ask::evaluate_legacy(&context) {
            return verdict;
        }
        if let Some(verdict) = auto_mode_approve::evaluate_legacy(&context) {
            return verdict;
        }
        let session_verdict = {
            let records = self
                .session_records
                .read()
                .unwrap_or_else(|error| error.into_inner());
            session_approval_history::evaluate_legacy(&context, &records, &self.compiled_session)
        };
        if let Some(verdict) = session_verdict {
            return verdict;
        }
        if let Some(verdict) = user_configured_ask::evaluate_legacy(&context, &self.compiled_ask) {
            return verdict;
        }
        if let Some(verdict) = user_configured_allow::evaluate_legacy(&context, &self.compiled_allow)
        {
            return verdict;
        }
        if let Some(verdict) = sensitive_file_access_ask::evaluate_legacy(&context) {
            return verdict;
        }
        if let Some(verdict) = git_control_path_access_ask::evaluate_legacy(&context) {
            return verdict;
        }
        if let Some(verdict) = yolo_mode_approve::evaluate_legacy(&context) {
            return verdict;
        }
        if let Some(verdict) = default_tool_approve::evaluate_legacy(&context) {
            return verdict;
        }
        if let Some(verdict) = git_cwd_write_approve::evaluate_legacy(&context) {
            return verdict;
        }
        fallback_ask::evaluate_legacy(&context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_policy_chain_precedence_hierarchy() {
        let engine_deny_vs_allow = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            deny_rules: vec!["Write(src/*)".into()],
            allow_rules: vec!["Write(src/*)".into()],
            ..Default::default()
        });
        let verdict = engine_deny_vs_allow.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");

        let engine_session_vs_ask = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            session_approvals: vec!["Write(src/*)".into()],
            ask_rules: vec!["Write(src/*)".into()],
            ..Default::default()
        });
        let verdict = engine_session_vs_ask.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "SessionApprovalHistory");

        let engine_ask_vs_allow = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ask_rules: vec!["Write(src/*)".into()],
            allow_rules: vec!["Write(src/*)".into()],
            ..Default::default()
        });
        let verdict = engine_ask_vs_allow.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "UserConfiguredAsk");

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
    fn test_serde_and_helpers() {
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

        let mode_json = serde_json::to_string(&PermissionMode::Auto).unwrap();
        assert_eq!(mode_json, "\"auto\"");
        let parsed_mode: PermissionMode = serde_json::from_str("\"yolo\"").unwrap();
        assert_eq!(parsed_mode, PermissionMode::Yolo);

        let verdict_json = serde_json::to_string(&allow_verdict).unwrap();
        let parsed_verdict: LocalPermissionVerdict = serde_json::from_str(&verdict_json).unwrap();
        assert_eq!(parsed_verdict, allow_verdict);

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

        let engine = PermissionEngine::new(serde_json::from_value(plan).expect("plan snapshot"));
        let verdict = engine.evaluate("Bash", &json!({ "command": "ls -la" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask, "unknown mode asks");
    }

    #[test]
    fn test_set_mode_switches_live_verdicts() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let before = engine.evaluate("Bash", &json!({ "command": "cargo check" }));
        assert_eq!(before.decision, VerdictDecision::Ask);
        assert_eq!(before.policy_name, "FallbackAsk");

        engine.set_mode(PermissionMode::Yolo);
        let after = engine.evaluate("Bash", &json!({ "command": "cargo check" }));
        assert_eq!(after.decision, VerdictDecision::Allow);
        assert_eq!(after.policy_name, "YoloModeApprove");
        assert_eq!(engine.mode(), PermissionMode::Yolo);

        engine.set_mode(PermissionMode::Manual);
        let again = engine.evaluate("Bash", &json!({ "command": "cargo check" }));
        assert_eq!(again.decision, VerdictDecision::Ask);

        engine.set_mode(PermissionMode::Auto);
        let auto = engine.evaluate("Bash", &json!({ "command": "sudo rm -rf /" }));
        assert_eq!(auto.decision, VerdictDecision::Allow);
        assert_eq!(auto.policy_name, "AutoModeApprove");
    }

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
