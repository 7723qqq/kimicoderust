use std::sync::Arc;

use futures_util::future::BoxFuture;

use crate::_base::di::instantiation::LiveRef;
use crate::_base::log::log::ILogService;
use crate::app::agent_identity::agent_identity::IAgentIdentity;
use crate::app::agent_profile_catalog::agent_profile_registry::IAgentProfileRegistry;
use crate::app::agent_profile_catalog::builtin_agent_profile_loader::IBuiltinAgentProfileLoader;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::git::git::IGitService;
use crate::app::mcp_config::config_store::IMcpConfigStore;
use crate::app::plugin::plugin::IPluginService;
use crate::app::session_manager::session_manager::ISessionManager;
use crate::app::state::app_state::IAppStateService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::app::workspace::workspace::IWorkspaceService;
use crate::app::workspace::workspace_persistence::IWorkspacePersistence;
use crate::features::skill::catalog::builtin_skill_source::IBuiltinSkillSource;
use crate::features::skill::catalog::user_file_skill_source::IUserFileSkillSource;
use crate::mcp_core::oauth::service::McpOAuthService;
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::wire::record::WireAsyncError;
use crate::workspace::session_lifecycle::session_lifecycle_service::SessionLifecycleService;
use crate::workspace::workspace_agent_profile_loader::explicit_agent_profile_loader::IExplicitAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::extra_agent_profile_loader::IExtraAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::plugin_agent_profile_loader::IPluginAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::user_agent_profile_loader::IUserAgentProfileLoader;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;
use crate::workspace::workspace_mcp::workspace_mcp::IWorkspaceMcpService;
use crate::workspace::workspace_trust::workspace_trust::IWorkspaceTrust;
use crate::workspace::workspace_trust::workspace_trust_store::IWorkspaceTrustStore;

use super::program_workdirs::ProgramWorkdir;

pub struct ProgramSessionControllerInput {
    pub context: Arc<dyn IWorkspaceContext>,
    pub extra_agent_profiles: Arc<dyn IExtraAgentProfileLoader>,
    pub explicit_agent_profiles: Arc<dyn IExplicitAgentProfileLoader>,
    pub user_agent_profiles: Arc<dyn IUserAgentProfileLoader>,
    pub plugin_agent_profiles: Arc<dyn IPluginAgentProfileLoader>,
    pub mcp: Arc<dyn IWorkspaceMcpService>,
    pub trust: Arc<dyn IWorkspaceTrust>,
    pub acquire_workdir: Arc<
        dyn Fn(String, String, bool) -> BoxFuture<'static, Result<ProgramWorkdir, WireAsyncError>>
            + Send
            + Sync,
    >,
    pub on_dispose: Option<Box<dyn FnOnce() + Send>>,
}

pub type CreateSessionController = Arc<
    dyn Fn(ProgramSessionControllerInput) -> Arc<SessionLifecycleService> + Send + Sync,
>;

pub struct ProgramDependencies {
    pub app_state: Arc<dyn IAppStateService>,
    pub docs: Arc<dyn IAtomicDocumentStore>,
    pub workspaces: Arc<dyn IWorkspaceService>,
    pub workspace_store: Arc<dyn IWorkspacePersistence>,
    pub bootstrap: Arc<dyn IBootstrapService>,
    pub config: Arc<dyn IConfigService>,
    pub git: LiveRef<Arc<dyn IGitService>>,
    pub identity: Arc<dyn IAgentIdentity>,
    pub log: Arc<dyn ILogService>,
    pub oauth: Arc<McpOAuthService>,
    pub config_store: Arc<dyn IMcpConfigStore>,
    pub plugins: Arc<dyn IPluginService>,
    pub session_manager: LiveRef<Arc<dyn ISessionManager>>,
    pub agent_profiles: Arc<dyn IAgentProfileRegistry>,
    pub builtin_agent_profiles: Arc<dyn IBuiltinAgentProfileLoader>,
    pub builtin_skills: Arc<dyn IBuiltinSkillSource>,
    pub user_skills: Arc<dyn IUserFileSkillSource>,
    pub telemetry: Arc<dyn ITelemetryService>,
    pub trust_store: Arc<dyn IWorkspaceTrustStore>,
    pub create_session_controller: CreateSessionController,
}
