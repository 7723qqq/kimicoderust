use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::log::log::ILogService;
use crate::app::agent_profile_catalog::agent_profile_contribution::AGENT_PROFILE_SOURCE_PRIORITY;
use crate::app::agent_profile_catalog::agent_profile_registry::IAgentProfileRegistry;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::wire::record::WireAsyncError;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;

use super::config_section::EXTRA_AGENT_DIRS_SECTION;
use super::extra_agent_profile_loader::IExtraAgentProfileLoader;
use super::internal::agent_file_discovery::discover_agent_files;
use super::internal::agent_profile_from_file::profiles_from_discovery;
use super::internal::agent_profile_loader::{
    AgentProfileLoaderBase, AgentProfileLoaderHooks, AgentProfileProvide,
};
use super::internal::agent_roots::configured_agent_roots;
use super::internal::types::AgentFileSource;
use super::user_agent_profile_loader::IUserAgentProfileLoader;

pub struct ExtraAgentProfileLoaderService {
    base: Arc<AgentProfileLoaderBase>,
    _subscriptions: Vec<DisposableRef>,
}

impl ExtraAgentProfileLoaderService {
    pub fn new(
        config_service: Arc<dyn IConfigService>,
        workspace: Arc<dyn IWorkspaceContext>,
        dirs: Arc<dyn IWorkspaceDirs>,
        bootstrap: Arc<dyn IBootstrapService>,
        fs: Arc<dyn IHostFileSystem>,
        log: Arc<dyn ILogService>,
        user: Arc<dyn IUserAgentProfileLoader>,
        registry: Option<Arc<dyn IAgentProfileRegistry>>,
        provide: AgentProfileProvide,
    ) -> Self {
        let load_log = log.clone();
        let load_config = config_service.clone();
        let load_dirs = dirs.clone();
        let load_bootstrap = bootstrap.clone();
        let load_fs = fs.clone();
        let load_user = user.clone();
        let base = Arc::new(AgentProfileLoaderBase::new(
            log.clone(),
            registry,
            provide,
            AgentProfileLoaderHooks {
                source_id: "extra".to_string(),
                priority: AGENT_PROFILE_SOURCE_PRIORITY.extra,
                fatal: false,
                workspace_key: Some(workspace.workspace_id()),
                context_key: None,
                load: Arc::new(move || {
                    let config = load_config.clone();
                    let dirs = load_dirs.clone();
                    let bootstrap = load_bootstrap.clone();
                    let fs = load_fs.clone();
                    let user = load_user.clone();
                    let log = load_log.clone();
                    async move {
                        config.ready().await?;
                        let dirs_config = config
                            .get(EXTRA_AGENT_DIRS_SECTION)
                            .as_ref()
                            .and_then(|value| {
                                value.as_array().map(|items| {
                                    items
                                        .iter()
                                        .filter_map(|item| item.as_str().map(str::to_string))
                                        .collect::<Vec<_>>()
                                })
                            })
                            .unwrap_or_default();
                        let warn_log = log.clone();
                        let roots = configured_agent_roots(
                            fs.as_ref(),
                            &dirs_config,
                            &dirs.primary(),
                            &bootstrap.os_home_dir(),
                            AgentFileSource::Extra,
                            Some(&move |message, error| {
                                warn_log.warn(
                                    message,
                                    error.map(|error| {
                                        serde_json::json!({ "error": error.to_string() })
                                    }),
                                );
                            }),
                        )
                        .await?;
                        let discovery = discover_agent_files(
                            fs.as_ref(),
                            &roots,
                            Some(&|message, _error| log.warn(message, None)),
                        )
                        .await?;
                        Ok(profiles_from_discovery(
                            discovery,
                            Arc::new(move |context| {
                                (user.get_default_profile().render_system_prompt)(context)
                            }),
                        ))
                    }
                    .boxed()
                }),
            },
        ));
        let mut subscriptions = Vec::new();
        let reload_base = base.clone();
        let reload_log = log.clone();
        subscriptions.push(config_service.on_did_section_change(Box::new(move |event| {
            if event.domain != EXTRA_AGENT_DIRS_SECTION {
                return;
            }
            let base = reload_base.clone();
            let log = reload_log.clone();
            tokio::spawn(async move {
                if let Err(error) = base.reload().await {
                    log.warn(
                        &format!("agent profile loader \"extra\" reload failed: {error}"),
                        None,
                    );
                }
            });
        })));
        let reload_base = base.clone();
        subscriptions.push(dirs.on_did_change(Arc::new(move || {
            let base = reload_base.clone();
            let log = log.clone();
            tokio::spawn(async move {
                if let Err(error) = base.reload().await {
                    log.warn(
                        &format!("agent profile loader \"extra\" primary reload failed: {error}"),
                        None,
                    );
                }
            });
        })));
        base.start();
        Self {
            base,
            _subscriptions: subscriptions,
        }
    }
}

impl ExtraAgentProfileLoaderService {
    pub fn dispose(&self) {
        for subscription in &self._subscriptions {
            subscription.dispose();
        }
        self.base.dispose();
    }
}

impl IExtraAgentProfileLoader for ExtraAgentProfileLoaderService {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        self.base.ready()
    }

    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let base = self.base.clone();
        Box::pin(async move { base.reload().await })
    }
}
