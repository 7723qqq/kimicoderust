use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::_base::di::lifecycle::IDisposable;
use crate::app::environment::environment::IEnvironmentService;
use crate::environment::environment::{EnvironmentBinding, LOCAL_ENVIRONMENT_ID, environment_binding_id};
use crate::environment::environment_registry::{
    EnvironmentError, EnvironmentGenerationSnapshot, EnvironmentRegistryChange,
};
use crate::features::skill::workspace::environment_skill_discovery::EnvironmentSkillDiscovery;
use crate::features::skill::workspace::explicit_file_skill_source::ExplicitFileSkillSource;
use crate::features::skill::workspace::extra_file_skill_source::ExtraFileSkillSource;
use crate::features::skill::workspace::plugin_skill_source::PluginSkillSource;
use crate::features::skill::workspace::workspace_skill_catalog::IWorkspaceSkillCatalog;
use crate::wire::record::WireAsyncError;
use crate::workspace::session_lifecycle::session_lifecycle_service::SessionLifecycleService;
use crate::workspace::state::workspace_state::IWorkspaceStateService;
use crate::workspace::workspace_agent_profile_loader::explicit_agent_profile_loader::IExplicitAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::explicit_agent_profile_loader_service::ExplicitAgentProfileLoaderService;
use crate::workspace::workspace_agent_profile_loader::extra_agent_profile_loader::IExtraAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::extra_agent_profile_loader_service::ExtraAgentProfileLoaderService;
use crate::workspace::workspace_agent_profile_loader::plugin_agent_profile_loader::IPluginAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::plugin_agent_profile_loader_service::PluginAgentProfileLoaderService;
use crate::workspace::workspace_agent_profile_loader::user_agent_profile_loader::IUserAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::user_agent_profile_loader_service::UserAgentProfileLoaderService;
use crate::workspace::workspace_agent_profile_loader::workspace_agent_profile_loader::IWorkspaceAgentProfileLoader;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;
use crate::workspace::workspace_fs::fs::IWorkspaceFsService;
use crate::workspace::workspace_git::workspace_git::IWorkspaceGitService;
use crate::workspace::workspace_instructions::workspace_instructions::IWorkspaceInstructionsService;
use crate::workspace::workspace_mcp::workspace_mcp::IWorkspaceMcpService;
use crate::workspace::workspace_mcp::workspace_mcp_service::WorkspaceMcpService;
use crate::workspace::workspace_mcp_config::workspace_mcp_config::IWorkspaceMcpConfigService;
use crate::workspace::workspace_mcp_config::workspace_mcp_config_service::WorkspaceMcpConfigService;
use crate::workspace::workspace_trust::trust_disclosure::IWorkspaceTrustDisclosure;
use crate::workspace::workspace_trust::trust_disclosure_service::WorkspaceTrustDisclosureService;
use crate::workspace::workspace_trust::workspace_trust::IWorkspaceTrust;
use crate::workspace::workspace_trust::workspace_trust_service::WorkspaceTrustService;

use super::program_dependencies::{ProgramDependencies, ProgramSessionControllerInput};
use super::program_workdirs::{
    PROGRAM_CAPABILITIES, ProgramDisposable, ProgramHarness, ProgramWorkdirs,
    SharedDisposables, own,
};

pub use super::program_workdirs::ProgramWorkdir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgramStatus {
    Preparing,
    Ready,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramCatalogSkillsSnapshot {
    pub total: usize,
    pub invocable: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgramCatalogSnapshot {
    pub skills: ProgramCatalogSkillsSnapshot,
    pub agent_profiles: usize,
    pub mcp_servers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramSourceCount {
    pub source: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgramAgentProfileSourceSnapshot {
    pub source_id: String,
    pub priority: i64,
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgramSourceProvenanceSnapshot {
    pub skills: Vec<ProgramSourceCount>,
    pub skill_roots: Vec<String>,
    pub agent_profiles: Vec<ProgramAgentProfileSourceSnapshot>,
    pub instruction_paths: Vec<String>,
    pub mcp_servers: Vec<String>,
}

#[derive(Clone)]
pub struct ProgramSnapshot {
    pub workspace_id: String,
    pub binding: EnvironmentBinding,
    pub status: ProgramStatus,
    pub ready: bool,
    pub generation: Option<String>,
    pub trusted: Option<bool>,
    pub catalog: ProgramCatalogSnapshot,
    pub sources: ProgramSourceProvenanceSnapshot,
    pub environments: Vec<EnvironmentGenerationSnapshot>,
}

struct ProgramGeneration {
    id: String,
    workdirs: Arc<ProgramWorkdirs>,
    mcp_config: Arc<dyn IWorkspaceMcpConfigService>,
    mcp: Arc<dyn IWorkspaceMcpService>,
    trust: Arc<dyn IWorkspaceTrust>,
    trust_disclosure: Arc<dyn IWorkspaceTrustDisclosure>,
    user_agent_profiles: Arc<dyn IUserAgentProfileLoader>,
    plugin_agent_profiles: Arc<dyn IPluginAgentProfileLoader>,
    explicit_agent_profiles: Arc<dyn IExplicitAgentProfileLoader>,
    extra_agent_profiles: Arc<dyn IExtraAgentProfileLoader>,
    disposables: SharedDisposables,
    ready: std::sync::atomic::AtomicBool,
    failed: std::sync::atomic::AtomicBool,
    references: std::sync::atomic::AtomicUsize,
    retired: std::sync::atomic::AtomicBool,
}

type CleanupFuture = Shared<BoxFuture<'static, Result<(), SharedProgramError>>>;

struct ProgramState {
    status: ProgramStatus,
    generation: Option<Arc<ProgramGeneration>>,
    generation_failed: bool,
    disposed: bool,
    cleanups: HashMap<usize, CleanupFuture>,
    next_cleanup: usize,
    listeners: HashMap<u64, Arc<dyn Fn(ProgramSnapshot) + Send + Sync>>,
    next_listener_id: u64,
}

struct ProgramInner {
    workspace_id: String,
    binding: EnvironmentBinding,
    environments: Arc<dyn IEnvironmentService>,
    context: Arc<dyn IWorkspaceContext>,
    dependencies: Arc<ProgramDependencies>,
    state: Arc<Mutex<ProgramState>>,
    registry_subscription: Mutex<Option<Box<dyn IDisposable>>>,
    program_ready: std::sync::atomic::AtomicBool,
    ready_notify: tokio::sync::Notify,
}

#[derive(Clone)]
pub struct Program {
    inner: Arc<ProgramInner>,
}

impl Program {
    pub fn new(
        workspace_id: String,
        environments: Arc<dyn IEnvironmentService>,
        context: Arc<dyn IWorkspaceContext>,
        dependencies: ProgramDependencies,
    ) -> Self {
        let binding = EnvironmentBinding {
            environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            cwd: context.cwd(),
        };
        let this = Self {
            inner: Arc::new(ProgramInner {
                workspace_id,
                binding,
                environments,
                context,
                dependencies: Arc::new(dependencies),
                state: Arc::new(Mutex::new(ProgramState {
                    status: ProgramStatus::Preparing,
                    generation: None,
                    generation_failed: false,
                    disposed: false,
                    cleanups: HashMap::new(),
                    next_cleanup: 0,
                    listeners: HashMap::new(),
                    next_listener_id: 0,
                })),
                registry_subscription: Mutex::new(None),
                program_ready: std::sync::atomic::AtomicBool::new(false),
                ready_notify: tokio::sync::Notify::new(),
            }),
        };
        let weak = Arc::downgrade(&this.inner);
        let subscription = this.inner.environments.on_did_change(move |change| {
            if let Some(inner) = weak.upgrade() {
                Self { inner }.on_environment_change(change);
            }
        });
        *this.inner.registry_subscription.lock() = Some(subscription);
        this.reconcile_generation();
        this
    }

    pub fn workspace_id(&self) -> String {
        self.inner.workspace_id.clone()
    }

    pub fn binding(&self) -> EnvironmentBinding {
        self.inner.binding.clone()
    }

    pub fn status(&self) -> ProgramStatus {
        self.inner.state.lock().status
    }

    pub fn ready(&self) -> BoxFuture<'static, ()> {
        let inner = self.inner.clone();
        async move {
            let notified = inner.ready_notify.notified();
            if inner.program_ready.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            notified.await;
        }
        .boxed()
    }

    pub fn state(&self) -> Arc<dyn IWorkspaceStateService> {
        self.require_generation().workdirs.root.state.clone()
    }

    pub fn dirs(&self) -> Arc<dyn IWorkspaceDirs> {
        self.require_generation().workdirs.root.dirs.clone()
    }

    pub fn fs(&self) -> Arc<dyn IWorkspaceFsService> {
        self.require_generation().workdirs.root.fs.clone()
    }

    pub fn git(&self) -> Arc<dyn IWorkspaceGitService> {
        self.require_generation().workdirs.root.git.clone()
    }

    pub fn instructions(&self) -> Arc<dyn IWorkspaceInstructionsService> {
        self.require_generation().workdirs.root.instructions.clone()
    }

    pub fn mcp_config(&self) -> Arc<dyn IWorkspaceMcpConfigService> {
        self.require_generation().mcp_config.clone()
    }

    pub fn mcp(&self) -> Arc<dyn IWorkspaceMcpService> {
        self.require_generation().mcp.clone()
    }

    pub fn trust(&self) -> Arc<dyn IWorkspaceTrust> {
        self.require_generation().trust.clone()
    }

    pub fn trust_disclosure(&self) -> Arc<dyn IWorkspaceTrustDisclosure> {
        self.require_generation().trust_disclosure.clone()
    }

    pub fn skills(&self) -> Arc<dyn IWorkspaceSkillCatalog> {
        self.require_generation().workdirs.root.skills.clone()
    }

    pub fn agent_profiles(&self) -> Arc<dyn IWorkspaceAgentProfileLoader> {
        self.require_generation().workdirs.root.agent_profiles.clone()
    }

    pub fn user_agent_profiles(&self) -> Arc<dyn IUserAgentProfileLoader> {
        self.require_generation().user_agent_profiles.clone()
    }

    pub fn session_controller_generation(&self) -> String {
        self.require_generation().id.clone()
    }

    fn root_key(&self) -> String {
        environment_binding_id(LOCAL_ENVIRONMENT_ID, &self.inner.context.cwd())
    }

    pub fn create_session_controller(&self) -> Arc<SessionLifecycleService> {
        let generation = self.require_generation();
        generation.references.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released_sink = released.clone();
        let this = self.clone();
        let generation_for_release = generation.clone();
        let release = Arc::new(move || {
            if released_sink.swap(true, std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            this.release_generation(generation_for_release.clone());
            let _ = released;
        });
        let workdirs = generation.workdirs.clone();
        let input = ProgramSessionControllerInput {
            context: self.inner.context.clone(),
            extra_agent_profiles: generation.extra_agent_profiles.clone(),
            explicit_agent_profiles: generation.explicit_agent_profiles.clone(),
            user_agent_profiles: generation.user_agent_profiles.clone(),
            plugin_agent_profiles: generation.plugin_agent_profiles.clone(),
            mcp: generation.mcp.clone(),
            acquire_workdir: Arc::new(move |environment_id, cwd, follows_primary| {
                let workdirs = workdirs.clone();
                async move { workdirs.acquire(&environment_id, &cwd, follows_primary).await }.boxed()
            }),
            on_dispose: Some(Box::new(move || release())),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.inner.dependencies.create_session_controller)(input)
        }));
        match result {
            Ok(controller) => controller,
            Err(error) => {
                let release = released.clone();
                release.store(true, std::sync::atomic::Ordering::Relaxed);
                self.release_generation(generation);
                std::panic::resume_unwind(error)
            }
        }
    }

    pub async fn reload(&self) -> Result<(), WireAsyncError> {
        let generation = self.require_generation();
        generation.workdirs.reload().await?;
        let _ = generation.user_agent_profiles.reload().await;
        Ok(())
    }

    /// Wait for the plugin-contributed agent profiles to finish loading (v2
    /// `waitForPluginAgentProfiles`, `program.ts:45-47`). The host that blocks
    /// startup on this is a seam concern; the loader readiness itself is engine
    /// state.
    pub async fn wait_for_plugin_agent_profiles(&self) -> Result<(), WireAsyncError> {
        self.require_generation().plugin_agent_profiles.ready().await
    }

    pub fn snapshot(&self) -> ProgramSnapshot {
        let generation = self.inner.state.lock().generation.clone();
        let skills = generation
            .as_ref()
            .map(|generation| generation.workdirs.root.skills.catalog().list_skills())
            .unwrap_or_default();
        let mut skills_by_source = HashMap::new();
        for skill in &skills {
            *skills_by_source.entry(skill.source.clone()).or_insert(0) += 1;
        }
        let root_key = self.root_key();
        let agent_profiles = self
            .inner
            .dependencies
            .agent_profiles
            .entries()
            .into_iter()
            .filter(|entry| {
                (entry.workspace_key.is_none()
                    || entry.workspace_key.as_deref() == Some(self.inner.workspace_id.as_str()))
                    && (entry.context_key.is_none()
                        || entry.context_key.as_deref() == Some(root_key.as_str()))
            })
            .map(|entry| ProgramAgentProfileSourceSnapshot {
                source_id: entry.source_id,
                priority: entry.priority,
                profiles: entry
                    .contribution
                    .profiles
                    .iter()
                    .map(|profile| profile.name.clone())
                    .collect(),
            })
            .collect::<Vec<_>>();
        let mcp_servers = generation
            .as_ref()
            .map(|generation| generation.mcp_config.servers().keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        ProgramSnapshot {
            workspace_id: self.inner.workspace_id.clone(),
            binding: self.inner.binding.clone(),
            status: self.inner.state.lock().status,
            ready: generation.as_ref().is_some_and(|generation| {
                generation.ready.load(std::sync::atomic::Ordering::Relaxed)
            }),
            generation: generation.as_ref().map(|generation| generation.id.clone()),
            trusted: generation.as_ref().map(|generation| generation.trust.is_trusted()),
            catalog: ProgramCatalogSnapshot {
                skills: ProgramCatalogSkillsSnapshot {
                    total: skills.len(),
                    invocable: generation
                        .as_ref()
                        .map(|generation| {
                            generation
                                .workdirs
                                .root
                                .skills
                                .catalog()
                                .list_invocable_skills()
                                .len()
                        })
                        .unwrap_or(0),
                    skipped: generation
                        .as_ref()
                        .map(|generation| {
                            generation
                                .workdirs
                                .root
                                .skills
                                .catalog()
                                .get_skipped_by_policy()
                                .len()
                        })
                        .unwrap_or(0),
                },
                agent_profiles: agent_profiles.iter().map(|source| source.profiles.len()).sum(),
                mcp_servers: mcp_servers.len(),
            },
            sources: ProgramSourceProvenanceSnapshot {
                skills: skills_by_source
                    .into_iter()
                    .map(|(source, count)| ProgramSourceCount { source, count })
                    .collect(),
                skill_roots: generation
                    .as_ref()
                    .map(|generation| generation.workdirs.root.skills.catalog().get_skill_roots())
                    .unwrap_or_default(),
                agent_profiles,
                instruction_paths: generation
                    .as_ref()
                    .and_then(|generation| {
                        generation.workdirs.root.instructions.snapshot().agents_md_paths
                    })
                    .unwrap_or_default(),
                mcp_servers,
            },
            environments: self.inner.environments.snapshot().environments,
        }
    }

    pub fn on_did_change(&self, listener: Arc<dyn Fn(ProgramSnapshot) + Send + Sync>) -> Box<dyn IDisposable> {
        let id = {
            let mut state = self.inner.state.lock();
            let id = state.next_listener_id;
            state.next_listener_id += 1;
            state.listeners.insert(id, listener);
            id
        };
        let state = self.inner.state.clone();
        crate::_base::di::lifecycle::to_disposable(move || {
            state.lock().listeners.remove(&id);
        })
    }

    pub async fn dispose(&self) -> Result<(), WireAsyncError> {
        let generation = {
            let mut state = self.inner.state.lock();
            if !state.disposed {
                state.disposed = true;
                self.inner.registry_subscription.lock().take();
                let generation = state.generation.take();
                state.listeners.clear();
                generation
            } else {
                None
            }
        };
        if let Some(generation) = generation {
            self.retire_generation(generation);
        }
        let cleanups = self
            .inner
            .state
            .lock()
            .cleanups
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut errors = Vec::new();
        for result in futures_util::future::join_all(cleanups).await {
            if let Err(error) = result {
                errors.push(error);
            }
        }
        if errors.len() == 1 {
            return Err(errors.remove(0).boxed());
        }
        if errors.len() > 1 {
            return Err(Box::new(ProgramDisposalError {
                message: format!("program {} disposal failed", self.inner.workspace_id),
                errors,
            }));
        }
        Ok(())
    }

    fn require_generation(&self) -> Arc<ProgramGeneration> {
        self.inner
            .state
            .lock()
            .generation
            .clone()
            .unwrap_or_else(|| {
                panic!(
                    "program {} has no available local environment generation",
                    self.inner.workspace_id
                )
            })
    }

    fn on_environment_change(&self, change: EnvironmentRegistryChange) {
        if change.environment_id != LOCAL_ENVIRONMENT_ID || self.inner.state.lock().disposed {
            return;
        }
        self.reconcile_generation();
    }

    fn reconcile_generation(&self) {
        let local = self.inner.environments.current(LOCAL_ENVIRONMENT_ID);
        let Some(local) = local else {
            let previous = self.inner.state.lock().generation.take();
            if let Some(previous) = previous {
                self.retire_generation(previous);
            }
            self.refresh();
            return;
        };
        let generation_id = self
            .inner
            .state
            .lock()
            .generation
            .as_ref()
            .map(|generation| generation.id.clone());
        if generation_id.as_deref() != Some(local.identity.generation.as_str()) {
            let previous = self.inner.state.lock().generation.take();
            self.inner.state.lock().generation_failed = false;
            match self.create_generation() {
                Ok(next) => {
                    self.inner.state.lock().generation = Some(next.clone());
                    if let Some(previous) = previous {
                        self.retire_generation(previous);
                    }
                    self.observe_readiness(next);
                }
                Err(error) => {
                    if error
                        .downcast_ref::<EnvironmentError>()
                        .is_none_or(|error| error.code() != "environment.unavailable")
                    {
                        self.inner.state.lock().generation_failed = true;
                        self.resolve_program_ready();
                    }
                }
            }
        }
        self.refresh();
    }

    fn create_generation(&self) -> Result<Arc<ProgramGeneration>, WireAsyncError> {
        let lease = self.inner.environments.acquire(&self.inner.binding, PROGRAM_CAPABILITIES);
        let disposables = Arc::new(Mutex::new(Vec::new()));
        let result = (|| {
            let host_fs = lease.environment.fs.clone().unwrap();
            let skill_discovery = EnvironmentSkillDiscovery::new(
                self.inner.dependencies.log.clone(),
                host_fs.clone(),
            );
            let explicit_skills = own(
                ExplicitFileSkillSource::new(
                    skill_discovery.clone(),
                    self.inner.context.clone(),
                    self.inner.dependencies.bootstrap.clone(),
                    host_fs.clone(),
                    self.inner.dependencies.log.clone(),
                ),
                &disposables,
            );
            let extra_skills = own(
                ExtraFileSkillSource::new(
                    skill_discovery.clone(),
                    self.inner.dependencies.config.clone(),
                    self.inner.context.clone(),
                    self.inner.dependencies.bootstrap.clone(),
                    host_fs.clone(),
                    self.inner.dependencies.log.clone(),
                ),
                &disposables,
            );
            let plugin_skills = Arc::new(PluginSkillSource::new(
                skill_discovery,
                self.inner.dependencies.plugins.clone(),
            ));
            let registry = self.inner.dependencies.agent_profiles.clone();
            let provide = Arc::new(move |registration| {
                registry.register(registration)
            });
            let user_agent_profiles = own(
                UserAgentProfileLoaderService::new(
                    self.inner.dependencies.bootstrap.clone(),
                    host_fs.clone(),
                    self.inner.dependencies.log.clone(),
                    self.inner.dependencies.builtin_agent_profiles.clone(),
                    self.inner.context.clone(),
                    Some(self.inner.dependencies.agent_profiles.clone()),
                    provide.clone(),
                ),
                &disposables,
            );
            let plugin_agent_profiles = own(
                PluginAgentProfileLoaderService::new(
                    self.inner.dependencies.plugins.clone(),
                    host_fs.clone(),
                    self.inner.dependencies.log.clone(),
                    user_agent_profiles.clone(),
                    self.inner.context.clone(),
                    Some(self.inner.dependencies.agent_profiles.clone()),
                    provide.clone(),
                ),
                &disposables,
            );
            let harness = ProgramHarness {
                user_agent_profiles: user_agent_profiles.clone(),
                explicit_skills,
                extra_skills,
                plugin_skills,
            };
            let workdirs = own(
                ProgramWorkdirs::new(
                    self.inner.environments.clone(),
                    self.inner.context.clone(),
                    self.inner.dependencies.clone(),
                    harness,
                    &disposables,
                ),
                &disposables,
            );
            let root = &workdirs.root;
            let explicit_agent_profiles = own(
                ExplicitAgentProfileLoaderService::new(
                    self.inner.context.clone(),
                    root.dirs.clone(),
                    self.inner.dependencies.bootstrap.clone(),
                    host_fs.clone(),
                    self.inner.dependencies.log.clone(),
                    user_agent_profiles.clone(),
                    Some(self.inner.dependencies.agent_profiles.clone()),
                    provide.clone(),
                ),
                &disposables,
            );
            let extra_agent_profiles = own(
                ExtraAgentProfileLoaderService::new(
                    self.inner.dependencies.config.clone(),
                    self.inner.context.clone(),
                    root.dirs.clone(),
                    self.inner.dependencies.bootstrap.clone(),
                    host_fs.clone(),
                    self.inner.dependencies.log.clone(),
                    user_agent_profiles.clone(),
                    Some(self.inner.dependencies.agent_profiles.clone()),
                    provide,
                ),
                &disposables,
            );
            let trust = own(
                WorkspaceTrustService::new(
                    root.dirs.clone(),
                    self.inner.dependencies.trust_store.clone(),
                    root.state.clone(),
                    self.inner.dependencies.config.clone(),
                ),
                &disposables,
            );
            let mcp_config = own(
                WorkspaceMcpConfigService::new(
                    root.dirs.clone(),
                    self.inner.dependencies.bootstrap.clone(),
                    self.inner.dependencies.plugins.clone(),
                    self.inner.dependencies.log.clone(),
                    self.inner.dependencies.config.clone(),
                    host_fs.clone(),
                    trust.clone(),
                    self.inner.dependencies.config_store.clone(),
                ),
                &disposables,
            );
            let mcp = Arc::new(WorkspaceMcpService::new(
                self.inner.context.clone(),
                root.dirs.clone(),
                self.inner.environments.clone().into(),
                mcp_config.clone(),
                self.inner.dependencies.oauth.clone(),
                self.inner.dependencies.log.clone(),
                self.inner.dependencies.telemetry.clone(),
                self.inner.dependencies.identity.clone(),
                self.inner.dependencies.session_manager.clone(),
            ));
            disposables.lock().push(Arc::new(McpProgramDisposable(mcp.clone())));
            let trust_disclosure = Arc::new(WorkspaceTrustDisclosureService::new(
                self.inner.context.clone(),
                host_fs,
                self.inner.dependencies.bootstrap.clone(),
                self.inner.dependencies.config.clone(),
                root.local_config.clone(),
                root.dirs.clone(),
                trust.clone(),
                root.skills.clone(),
                root.agent_profiles.clone(),
                self.inner.dependencies.agent_profiles.clone(),
                root.instructions.clone(),
                self.inner.dependencies.log.clone(),
            ));
            Ok(Arc::new(ProgramGeneration {
                id: lease.environment.identity.generation.clone(),
                workdirs,
                mcp_config,
                mcp,
                trust,
                trust_disclosure,
                user_agent_profiles,
                plugin_agent_profiles,
                explicit_agent_profiles,
                extra_agent_profiles,
                disposables: disposables.clone(),
                ready: std::sync::atomic::AtomicBool::new(false),
                failed: std::sync::atomic::AtomicBool::new(false),
                references: std::sync::atomic::AtomicUsize::new(1),
                retired: std::sync::atomic::AtomicBool::new(false),
            }))
        })();
        if result.is_err() {
            self.cleanup_generation(disposables);
        }
        lease.dispose();
        result
    }

    fn observe_readiness(&self, generation: Arc<ProgramGeneration>) {
        let this = self.clone();
        tokio::spawn(async move {
            let results = futures_util::future::join_all([
                generation.workdirs.root.dirs.ready(),
                generation.workdirs.root.instructions.ready(),
                generation.mcp_config.ready(),
                generation.mcp.ready(),
                generation.workdirs.root.skills.ready(),
                generation.workdirs.root.agent_profiles.ready(),
            ])
            .await;
            let failed = results.into_iter().any(|result| result.is_err());
            let current = this
                .inner
                .state
                .lock()
                .generation
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &generation));
            if !current {
                return;
            }
            if failed {
                generation.failed.store(true, std::sync::atomic::Ordering::Relaxed);
            } else {
                generation.ready.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            this.resolve_program_ready();
            this.refresh();
        });
    }

    fn retire_generation(&self, generation: Arc<ProgramGeneration>) {
        if generation.retired.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.release_generation(generation);
    }

    fn release_generation(&self, generation: Arc<ProgramGeneration>) {
        let references = generation.references.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) - 1;
        if references != 0 || !generation.retired.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.cleanup_generation(generation.disposables.clone());
    }

    fn cleanup_generation(&self, disposables: SharedDisposables) {
        let workspace_id = self.inner.workspace_id.clone();
        let log = self.inner.dependencies.log.clone();
        let cleanup = async move {
            let mut errors = Vec::new();
            for disposable in disposables.lock().drain(..).rev() {
                if let Err(error) = disposable.dispose().await {
                    errors.push(SharedProgramError::new(error));
                }
            }
            if errors.len() == 1 {
                return Err(errors.remove(0));
            }
            if errors.len() > 1 {
                return Err(SharedProgramError(Arc::new(format!(
                    "program {workspace_id} generation disposal failed"
                ))));
            }
            Ok(())
        }
        .boxed()
        .shared();
        let id = {
            let mut state = self.inner.state.lock();
            let id = state.next_cleanup;
            state.next_cleanup += 1;
            state.cleanups.insert(id, cleanup.clone());
            id
        };
        let state = &self.inner.state;
        let _ = state;
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let result = cleanup.await;
            inner.state.lock().cleanups.remove(&id);
            if let Err(error) = result {
                log.error(
                    "program generation disposal failed",
                    Some(serde_json::json!({
                        "workspaceId": inner.workspace_id,
                        "error": error.to_string(),
                    })),
                );
            }
        });
    }

    fn resolve_program_ready(&self) {
        self.inner.program_ready.store(true, std::sync::atomic::Ordering::Release);
        self.inner.ready_notify.notify_waiters();
    }

    fn refresh(&self) {
        let local = self.inner.environments.current(LOCAL_ENVIRONMENT_ID);
        let state = self.inner.state.lock();
        let generation = state.generation.clone();
        drop(state);
        let status = if local.as_ref().is_none_or(|local| local.status == "connecting") {
            ProgramStatus::Preparing
        } else if self.inner.state.lock().generation_failed
            || generation.as_ref().is_some_and(|generation| {
                generation.failed.load(std::sync::atomic::Ordering::Relaxed)
            })
        {
            ProgramStatus::Degraded
        } else if !generation.as_ref().is_some_and(|generation| {
            generation.ready.load(std::sync::atomic::Ordering::Relaxed)
        }) {
            if generation.is_none() && local.as_ref().is_some_and(|local| local.status != "ready") {
                ProgramStatus::Degraded
            } else {
                ProgramStatus::Preparing
            }
        } else if local.as_ref().is_some_and(|local| local.status == "ready") {
            ProgramStatus::Ready
        } else {
            ProgramStatus::Degraded
        };
        self.inner.state.lock().status = status;
        let snapshot = self.snapshot();
        let listeners = self
            .inner
            .state
            .lock()
            .listeners
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            listener(snapshot.clone());
        }
    }
}

struct McpProgramDisposable(Arc<WorkspaceMcpService>);

impl ProgramDisposable for McpProgramDisposable {
    fn dispose(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        self.0.dispose_async()
    }
}

#[derive(Debug, Clone)]
struct SharedProgramError(Arc<String>);

impl SharedProgramError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedProgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedProgramError {}

#[derive(Debug)]
struct ProgramDisposalError {
    message: String,
    errors: Vec<SharedProgramError>,
}

impl fmt::Display for ProgramDisposalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ProgramDisposalError {}
