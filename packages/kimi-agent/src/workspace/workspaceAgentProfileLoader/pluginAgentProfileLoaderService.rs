use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::log::log::ILogService;
use crate::app::agent_profile_catalog::agent_profile_contribution::AGENT_PROFILE_SOURCE_PRIORITY;
use crate::app::agent_profile_catalog::agent_profile_registry::IAgentProfileRegistry;
use crate::app::plugin::plugin::IPluginService;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::wire::record::WireAsyncError;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;

use super::internal::agent_file_discovery::discover_agent_files;
use super::internal::agent_profile_from_file::profiles_from_discovery;
use super::internal::agent_profile_loader::{
    AgentProfileLoaderBase, AgentProfileLoaderHooks, AgentProfileProvide,
};
use super::plugin_agent_profile_loader::IPluginAgentProfileLoader;
use super::user_agent_profile_loader::IUserAgentProfileLoader;

pub struct PluginAgentProfileLoaderService {
    base: Arc<AgentProfileLoaderBase>,
    _subscription: DisposableRef,
}

impl PluginAgentProfileLoaderService {
    pub fn new(
        plugins: Arc<dyn IPluginService>,
        fs: Arc<dyn IHostFileSystem>,
        log: Arc<dyn ILogService>,
        user: Arc<dyn IUserAgentProfileLoader>,
        workspace: Arc<dyn IWorkspaceContext>,
        registry: Option<Arc<dyn IAgentProfileRegistry>>,
        provide: AgentProfileProvide,
    ) -> Self {
        let load_plugins = plugins.clone();
        let load_fs = fs.clone();
        let load_user = user.clone();
        let load_log = log.clone();
        let base = Arc::new(AgentProfileLoaderBase::new(
            log.clone(),
            registry,
            provide,
            AgentProfileLoaderHooks {
                source_id: "plugin".to_string(),
                priority: AGENT_PROFILE_SOURCE_PRIORITY.plugin,
                fatal: false,
                workspace_key: Some(workspace.workspace_id()),
                context_key: None,
                load: Arc::new(move || {
                    let plugins = load_plugins.clone();
                    let fs = load_fs.clone();
                    let user = load_user.clone();
                    let log = load_log.clone();
                    async move {
                        let roots = plugins.plugin_agent_roots().await?;
                        let discovery = discover_agent_files(
                            fs.as_ref(),
                            &roots,
                            Some(&|message| log.warn(message, None)),
                        )
                        .await?;
                        Ok(profiles_from_discovery(
                            discovery,
                            Arc::new(move |context| {
                                user.get_default_profile().render_system_prompt(context)
                            }),
                        ))
                    }
                    .boxed()
                }),
            },
        ));
        let reload_base = base.clone();
        let subscription = plugins.on_did_reload(move |_| {
            let base = reload_base.clone();
            let log = log.clone();
            tokio::spawn(async move {
                if let Err(error) = base.reload().await {
                    log.warn(
                        &format!("agent profile loader \"plugin\" reload failed: {error}"),
                        None,
                    );
                }
            });
        });
        base.start();
        Self {
            base,
            _subscription: subscription,
        }
    }
}

impl PluginAgentProfileLoaderService {
    pub fn dispose(&self) {
        self._subscription.dispose();
        self.base.dispose();
    }
}

impl IPluginAgentProfileLoader for PluginAgentProfileLoaderService {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        self.base.ready()
    }

    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        self.base.reload().boxed()
    }
}
