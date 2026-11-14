use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};

use crate::agent::environment_binding::agent_environment::IAgentEnvironmentService;
use crate::agent::permission_policy::types::{PermissionPolicy, PermissionPolicyResult};
use crate::agent::permission_rules::config_section::is_dangerous_command_guard_enabled;
use crate::agent::tool_executor::tool_hooks::ResolvedToolExecutionHookContext;
use crate::app::config::config::IConfigService;
use crate::app::git::work_tree::find_git_work_tree;
use crate::environment::environment::EnvironmentCapability;
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;
use crate::tool::path_access::PathClass;
use crate::wire::record::WireAsyncError;

use super::environment_lease::acquire_environment_lease;
use super::path_utils::{file_accesses, has_git_path_component, is_git_control_path};

#[derive(Clone)]
pub struct GitControlPathAccessAskPermissionPolicyService {
    environment: Arc<dyn IAgentEnvironmentService>,
    workspace: Arc<dyn ISessionWorkspaceContext>,
    config: Arc<dyn IConfigService>,
}

impl GitControlPathAccessAskPermissionPolicyService {
    pub fn new(
        environment: Arc<dyn IAgentEnvironmentService>,
        workspace: Arc<dyn ISessionWorkspaceContext>,
        config: Arc<dyn IConfigService>,
    ) -> Self {
        Self {
            environment,
            workspace,
            config,
        }
    }

    async fn evaluate_inner(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> Result<Option<PermissionPolicyResult>, WireAsyncError> {
        if !is_dangerous_command_guard_enabled(self.config.as_ref()) {
            return Ok(None);
        }
        let cwd = self.workspace.work_dir();
        if cwd.is_empty() {
            return Ok(None);
        }
        let Some(lease) = acquire_environment_lease(self.environment.as_ref(), &[])? else {
            return Ok(None);
        };
        let result = async {
            let Some(host) = lease.environment.host() else {
                return Ok(None);
            };
            let Some(path_class) = PathClass::from_str(&host.path_class) else {
                return Ok(None);
            };
            let accesses = file_accesses(&context);
            if accesses.is_empty() {
                return Ok(None);
            }
            if accesses
                .iter()
                .any(|access| has_git_path_component(&access.path, &cwd, path_class))
            {
                return Ok(Some(PermissionPolicyResult::Ask {
                    reason: None,
                    resolve_approval: None,
                    resolve_error: None,
                    resolve_non_interactive: None,
                }));
            }
            if !lease
                .environment
                .capabilities()
                .contains(&EnvironmentCapability::Fs)
            {
                return Ok(None);
            }
            let fs = lease.environment.fs();
            let Some(marker) = find_git_work_tree(fs.as_ref(), &cwd).await? else {
                return Ok(None);
            };
            if accesses
                .iter()
                .any(|access| is_git_control_path(&access.path, &marker, path_class))
            {
                return Ok(Some(PermissionPolicyResult::Ask {
                    reason: None,
                    resolve_approval: None,
                    resolve_error: None,
                    resolve_non_interactive: None,
                }));
            }
            Ok(None)
        }
        .await;
        lease.dispose();
        result
    }
}

impl PermissionPolicy for GitControlPathAccessAskPermissionPolicyService {
    fn name(&self) -> &str {
        "git-control-path-access-ask"
    }

    fn evaluate(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let this = self.clone();
        async move { this.evaluate_inner(context).await }.boxed()
    }
}

pub(crate) fn evaluate_legacy(
    context: &crate::permission::LegacyPolicyContext<'_>,
) -> Option<crate::permission::LocalPermissionVerdict> {
    if !context.snapshot.dangerous_command_guard {
        return None;
    }
    let path = context
        .file_access
        .iter()
        .find(|path| super::path_utils::is_legacy_git_control_path(path))?;
    Some(crate::permission::LocalPermissionVerdict {
        decision: crate::permission::VerdictDecision::Ask,
        policy_name: "GitControlPathAccessAsk".into(),
        session_approval_rule: None,
        reason: Some(
            crate::i18n::LocalizedText::with_params(
                "engine.permission.gitControlPathAccess",
                crate::i18n::i18n_params!["path" => path],
            )
            .render(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

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

        let verdict_gitignore = engine_manual.evaluate("Read", &json!({ "path": ".gitignore" }));
        assert_eq!(verdict_gitignore.decision, VerdictDecision::Allow);
        assert_eq!(verdict_gitignore.policy_name, "DefaultToolApprove");

        let verdict_workflow =
            engine_manual.evaluate("Read", &json!({ "path": ".github/workflows/ci.yml" }));
        assert_eq!(verdict_workflow.decision, VerdictDecision::Allow);
        assert_eq!(verdict_workflow.policy_name, "DefaultToolApprove");
    }
}
