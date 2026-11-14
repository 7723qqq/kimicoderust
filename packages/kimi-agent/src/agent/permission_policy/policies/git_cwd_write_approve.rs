use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};

use crate::agent::environment_binding::agent_environment::IAgentEnvironmentService;
use crate::agent::permission_policy::types::{
    PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult,
};
use crate::app::git::work_tree::{find_git_work_tree as find_runtime_git_work_tree, find_local_git_work_tree};
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;
use crate::tool::path_access::{
    PathClass, is_within_workspace as is_within_runtime_workspace, is_within_workspace_posix,
};
use crate::wire::record::WireAsyncError;

use super::environment_lease::acquire_environment_lease;
use super::path_utils::write_file_accesses;

#[derive(Clone)]
pub struct GitCwdWriteApprovePermissionPolicyService {
    environment: Arc<dyn IAgentEnvironmentService>,
    workspace: Arc<dyn ISessionWorkspaceContext>,
}

impl GitCwdWriteApprovePermissionPolicyService {
    pub fn new(
        environment: Arc<dyn IAgentEnvironmentService>,
        workspace: Arc<dyn ISessionWorkspaceContext>,
    ) -> Self {
        Self {
            environment,
            workspace,
        }
    }

    async fn evaluate_inner(
        &self,
        context: PermissionPolicyContext,
    ) -> Result<Option<PermissionPolicyResult>, WireAsyncError> {
        if !matches!(context.base.tool_call.name.as_str(), "Write" | "Edit") {
            return Ok(None);
        }
        let Some(lease) = acquire_environment_lease(self.environment.as_ref(), &[])? else {
            return Ok(None);
        };
        let result = async {
            if lease.environment.host().map(|host| host.path_class.as_str()) != Some("posix") {
                return Ok(None);
            }
            if !lease
                .environment
                .capabilities()
                .contains(&crate::environment::environment::EnvironmentCapability::Fs)
            {
                return Ok(None);
            }
            let cwd = self.workspace.work_dir();
            if cwd.is_empty() {
                return Ok(None);
            }
            let accesses = write_file_accesses(&context);
            if accesses.is_empty() {
                return Ok(None);
            }
            let mut roots = vec![cwd.clone()];
            roots.extend(self.workspace.additional_dirs());
            if !accesses.iter().all(|access| {
                is_within_runtime_workspace(&access.path, &roots, PathClass::Posix)
            }) {
                return Ok(None);
            }
            let marker = find_runtime_git_work_tree(lease.environment.fs().as_ref(), &cwd).await?;
            Ok((marker.is_some()).then_some(PermissionPolicyResult::Approve {
                reason: None,
                execution_metadata: None,
            }))
        }
        .await;
        lease.dispose();
        result
    }
}

impl PermissionPolicy for GitCwdWriteApprovePermissionPolicyService {
    fn name(&self) -> &str {
        "git-cwd-write-approve"
    }

    fn evaluate(
        &self,
        context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let this = self.clone();
        async move { this.evaluate_inner(context).await }.boxed()
    }
}

pub fn is_posix_path(path_str: &str) -> bool {
    let normalized = path_str.replace('\\', "/");
    if normalized.starts_with("//") {
        return false;
    }
    let mut chars = normalized.chars();
    !matches!(
        (chars.next(), chars.next()),
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic()
    )
}

pub(crate) fn evaluate_legacy(
    context: &crate::permission::LegacyPolicyContext<'_>,
) -> Option<crate::permission::LocalPermissionVerdict> {
    if !matches!(context.tool_lower, "write" | "edit") {
        return None;
    }
    let path = context.path_subject?;
    let dir = context.workspace_dir?;
    if !is_posix_path(dir)
        || !is_posix_path(path)
        || super::path_utils::is_legacy_git_control_path(path)
        || crate::native::file_type::is_sensitive_file(path)
        || !is_within_workspace_posix(path, dir, context.additional_dirs)
        || find_local_git_work_tree(dir).is_none()
    {
        return None;
    }
    Some(crate::permission::LocalPermissionVerdict {
        decision: crate::permission::VerdictDecision::Allow,
        policy_name: "GitCwdWriteApprove".into(),
        session_approval_rule: None,
        reason: None,
    })
}

#[cfg(test)]
mod tests {
    use super::is_posix_path;
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

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

        let verdict = engine.evaluate("Write", &json!({ "path": format!("{root}/src/lib.rs") }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "GitCwdWriteApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());

        let verdict_edit =
            engine.evaluate("Edit", &json!({ "path": format!("{root}/Cargo.toml") }));
        assert_eq!(verdict_edit.decision, VerdictDecision::Allow);
        assert_eq!(verdict_edit.policy_name, "GitCwdWriteApprove");
        assert_eq!(verdict_edit.reason, None);

        let verdict_outside =
            engine.evaluate("Write", &json!({ "path": "/other/location/file.txt" }));
        assert_eq!(verdict_outside.decision, VerdictDecision::Ask);
        assert_eq!(verdict_outside.policy_name, "FallbackAsk");
        assert_eq!(
            verdict_outside.reason,
            Some("Tool execution requires approval: Write".into())
        );

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
        let outside = win_engine.evaluate("Write", &json!({ "path": "/workspace/project/a.rs" }));
        assert_eq!(outside.decision, VerdictDecision::Ask);
        assert_eq!(outside.policy_name, "FallbackAsk");

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

        let bare = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let verdict = bare.evaluate("Write", &json!({ "path": "/workspace/project/a.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");
    }

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
}
