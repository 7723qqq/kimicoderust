use futures_util::future::{BoxFuture, FutureExt};

use crate::agent::permission_policy::types::{
    PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult,
};
use crate::wire::record::WireAsyncError;

pub(crate) const DEFAULT_APPROVE_TOOLS: &[&str] = &[
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
    "lsp",
    "towerstatus",
    "tower_status",
    "towerinbox",
    "tower_inbox",
];

#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultToolApprovePermissionPolicyService;

impl PermissionPolicy for DefaultToolApprovePermissionPolicyService {
    fn name(&self) -> &str {
        "default-tool-approve"
    }

    fn evaluate(
        &self,
        context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let tool_name = context.base.tool_call.name;
        let result = is_v2_default_approve_tool(&tool_name).then_some(
            PermissionPolicyResult::Approve {
                reason: None,
                execution_metadata: None,
            },
        );
        async move { Ok(result) }.boxed()
    }
}

fn is_v2_default_approve_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Read"
            | "Grep"
            | "Glob"
            | "RepoMap"
            | "SetTodoList"
            | "TodoList"
            | "TaskList"
            | "TaskOutput"
            | "WaitFor"
            | "CronList"
            | "WebSearch"
            | "FetchURL"
            | "Agent"
            | "AgentSwarm"
            | "AskUserQuestion"
            | "NotifyUser"
            | "Skill"
            | "ExitPlanMode"
            | "EnterSpecMode"
            | "ExitSpecMode"
            | "CreateGoal"
            | "GetGoal"
            | "SetGoalBudget"
            | "UpdateGoal"
            | "select_tools"
    ) || is_v2_github_readonly_tool(tool_name)
}

fn is_v2_github_readonly_tool(tool_name: &str) -> bool {
    crate::agent::tools::github::github_legacy::github_tool_defs()
        .iter()
        .any(|tool| tool.name == tool_name)
        && !crate::agent::tools::github::github_legacy::is_mutating_tool(tool_name)
}

pub(crate) fn is_default_approve_tool(tool_name: &str) -> bool {
    DEFAULT_APPROVE_TOOLS.contains(&tool_name.to_ascii_lowercase().as_str())
        || crate::agent::tools::github::github_legacy::is_readonly_tool(tool_name)
}

pub(crate) fn evaluate_legacy(
    context: &crate::permission::LegacyPolicyContext<'_>,
) -> Option<crate::permission::LocalPermissionVerdict> {
    is_default_approve_tool(context.tool_name).then_some(crate::permission::LocalPermissionVerdict {
        decision: crate::permission::VerdictDecision::Allow,
        policy_name: "DefaultToolApprove".into(),
        session_approval_rule: None,
        reason: None,
    })
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_APPROVE_TOOLS;
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

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
    fn test_readonly_github_tools_approved_by_default() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });

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
}
