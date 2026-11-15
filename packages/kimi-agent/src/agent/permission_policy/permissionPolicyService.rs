use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};

use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::environment_binding::agent_environment::IAgentEnvironmentService;
use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::permission_policy::permission_policy::{
    IAgentPermissionPolicyService, PermissionPolicyEvaluation,
};
use crate::agent::permission_policy::policies::auto_mode_approve::AutoModeApprovePermissionPolicyService;
use crate::agent::permission_policy::policies::auto_mode_ask_user_question_deny::AutoModeAskUserQuestionDenyPermissionPolicyService;
use crate::agent::permission_policy::policies::compaction_authorization_guard::CompactionAuthorizationGuardPermissionPolicyService;
use crate::agent::permission_policy::policies::dangerous_command_ask::DangerousCommandAskPermissionPolicyService;
use crate::agent::permission_policy::policies::default_tool_approve::DefaultToolApprovePermissionPolicyService;
use crate::agent::permission_policy::policies::fallback_ask::FallbackAskPermissionPolicyService;
use crate::agent::permission_policy::policies::git_control_path_access_ask::GitControlPathAccessAskPermissionPolicyService;
use crate::agent::permission_policy::policies::git_cwd_write_approve::GitCwdWriteApprovePermissionPolicyService;
use crate::agent::permission_policy::policies::guardian_review::GuardianReviewPermissionPolicyService;
use crate::agent::permission_policy::policies::sensitive_file_access_ask::SensitiveFileAccessAskPermissionPolicyService;
use crate::agent::permission_policy::policies::session_approval_history::SessionApprovalHistoryPermissionPolicyService;
use crate::agent::permission_policy::policies::user_configured_allow::UserConfiguredAllowPermissionPolicyService;
use crate::agent::permission_policy::policies::user_configured_ask::UserConfiguredAskPermissionPolicyService;
use crate::agent::permission_policy::policies::user_configured_deny::UserConfiguredDenyPermissionPolicyService;
use crate::agent::permission_policy::policies::yolo_mode_approve::YoloModeApprovePermissionPolicyService;
use crate::agent::permission_policy::types::PermissionPolicy;
use crate::agent::permission_rules::permission_rules::IAgentPermissionRulesService;
use crate::agent::tool_executor::tool_hooks::ResolvedToolExecutionHookContext;
use crate::app::bash_parser::bash_parser::IBashParserService;
use crate::app::config::config::IConfigService;
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;
use crate::wire::record::WireAsyncError;

pub struct AgentPermissionPolicyDependencies {
    pub bash_parser: Arc<dyn IBashParserService>,
    pub mode_service: Arc<dyn IAgentPermissionModeService>,
    pub rules_service: Arc<dyn IAgentPermissionRulesService>,
    pub context_memory: Arc<dyn IAgentContextMemoryService>,
    pub environment: Arc<dyn IAgentEnvironmentService>,
    pub workspace: Arc<dyn ISessionWorkspaceContext>,
    pub config: Arc<dyn IConfigService>,
    pub guardian_review: Arc<GuardianReviewPermissionPolicyService>,
    pub non_interactive: bool,
}

pub struct AgentPermissionPolicyService {
    policies: Vec<Arc<dyn PermissionPolicy>>,
}

impl AgentPermissionPolicyService {
    pub fn new(dependencies: AgentPermissionPolicyDependencies) -> Self {
        let mut policies: Vec<Arc<dyn PermissionPolicy>> = vec![
            Arc::new(AutoModeAskUserQuestionDenyPermissionPolicyService::new(
                dependencies.mode_service.clone(),
            )),
            Arc::new(UserConfiguredDenyPermissionPolicyService::new(
                dependencies.rules_service.clone(),
            )),
            Arc::new(CompactionAuthorizationGuardPermissionPolicyService::new(
                dependencies.context_memory,
            )),
        ];
        if !dependencies.non_interactive {
            policies.push(Arc::new(DangerousCommandAskPermissionPolicyService::new(
                dependencies.bash_parser.clone(),
                dependencies.mode_service.clone(),
                dependencies.config.clone(),
            )));
        }
        policies.extend([
            Arc::new(AutoModeApprovePermissionPolicyService::new(
                dependencies.mode_service.clone(),
            )) as Arc<dyn PermissionPolicy>,
            Arc::new(SessionApprovalHistoryPermissionPolicyService::new(
                dependencies.rules_service.clone(),
            )),
            Arc::new(UserConfiguredAskPermissionPolicyService::new(
                dependencies.rules_service.clone(),
            )),
            Arc::new(UserConfiguredAllowPermissionPolicyService::new(
                dependencies.rules_service.clone(),
            )),
            Arc::new(SensitiveFileAccessAskPermissionPolicyService::new(
                dependencies.config.clone(),
            )),
            Arc::new(GitControlPathAccessAskPermissionPolicyService::new(
                dependencies.environment.clone(),
                dependencies.workspace.clone(),
                dependencies.config,
            )),
            dependencies.guardian_review,
            Arc::new(YoloModeApprovePermissionPolicyService::new(
                dependencies.mode_service,
            )),
            Arc::new(DefaultToolApprovePermissionPolicyService),
            Arc::new(GitCwdWriteApprovePermissionPolicyService::new(
                dependencies.environment,
                dependencies.workspace,
            )),
            Arc::new(FallbackAskPermissionPolicyService),
        ]);
        Self { policies }
    }
}

impl IAgentPermissionPolicyService for AgentPermissionPolicyService {
    fn evaluate(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyEvaluation>, WireAsyncError>> {
        let policies = self.policies.clone();
        async move {
            for policy in policies {
                let result = policy.evaluate(context.clone()).await?;
                if let Some(result) = result {
                    return Ok(Some(PermissionPolicyEvaluation {
                        policy_name: policy.name().to_string(),
                        result,
                    }));
                }
            }
            Ok(None)
        }
        .boxed()
    }
}
