use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::instantiation::LiveRef;
use crate::app::environment::environment::IEnvironmentService;
use crate::app::git::git::{GitService, IGitService};
use crate::environment::environment::{LOCAL_ENVIRONMENT_ID, environment_binding_id};
use crate::features::skill::workspace::environment_skill_discovery::EnvironmentSkillDiscovery;
use crate::features::skill::workspace::explicit_file_skill_source::{
    ExplicitFileSkillSource, IExplicitFileSkillSource,
};
use crate::features::skill::workspace::extra_file_skill_source::{
    ExtraFileSkillSource, IExtraFileSkillSource,
};
use crate::features::skill::workspace::plugin_skill_source::IPluginSkillSource;
use crate::features::skill::workspace::root_file_skill_source::WorkspaceRootSkillSource;
use crate::features::skill::workspace::workspace_skill_catalog::IWorkspaceSkillCatalog;
use crate::features::skill::workspace::workspace_skill_catalog_service::WorkspaceSkillCatalogService;
use crate::os::interface::host_environment::HostEnvironmentInfo;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::persistence::backends::node_fs::project_local_config_service::FileProjectLocalConfigService;
use crate::wire::record::WireAsyncError;
use crate::workspace::state::workspace_state::IWorkspaceStateService;
use crate::workspace::state::workspace_state_service::WorkspaceStateService;
use crate::workspace::workspace_agent_profile_loader::user_agent_profile_loader::IUserAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::workspace_agent_profile_loader::IWorkspaceAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::workspace_agent_profile_loader_service::WorkspaceAgentProfileLoaderService;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;
use crate::workspace::workspace_dirs::workspace_dirs_service::WorkspaceDirsService;
use crate::workspace::workspace_fs::fs::IWorkspaceFsService;
use crate::workspace::workspace_fs::fs_service::WorkspaceFsService;
use crate::workspace::workspace_git::workspace_git::IWorkspaceGitService;
use crate::workspace::workspace_git::workspace_git_service::WorkspaceGitService;
use crate::workspace::workspace_instructions::workspace_instructions::IWorkspaceInstructionsService;
use crate::workspace::workspace_instructions::workspace_instructions_service::WorkspaceInstructionsService;

use super::program_dependencies::ProgramDependencies;

pub const PROGRAM_CAPABILITIES: &[&str] = &["fs", "process"];

pub trait ProgramDisposable: Send + Sync {
    fn dispose(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;
}

pub type SharedDisposables = Arc<Mutex<Vec<Arc<dyn ProgramDisposable>>>>;

pub fn own<T: ProgramDisposable + 'static>(value: T, disposables: &SharedDisposables) -> Arc<T> {
    let value = Arc::new(value);
    disposables.lock().push(value.clone());
    value
}

pub fn own_arc<T: ProgramDisposable + 'static>(
    value: Arc<T>,
    disposables: &SharedDisposables,
) -> Arc<T> {
    disposables.lock().push(value.clone());
    value
}

pub struct ProgramHarness {
    pub user_agent_profiles: Arc<dyn IUserAgentProfileLoader>,
    pub explicit_skills: Arc<dyn IExplicitFileSkillSource>,
    pub extra_skills: Arc<dyn IExtraFileSkillSource>,
    pub plugin_skills: Arc<dyn IPluginSkillSource>,
}

#[derive(Clone)]
pub struct ProgramView {
    pub state: Arc<dyn IWorkspaceStateService>,
    pub local_config: Arc<FileProjectLocalConfigService>,
    pub dirs: Arc<dyn IWorkspaceDirs>,
    pub git: Arc<dyn IWorkspaceGitService>,
    pub fs: Arc<dyn IWorkspaceFsService>,
    pub instructions: Arc<dyn IWorkspaceInstructionsService>,
    pub skills: Arc<dyn IWorkspaceSkillCatalog>,
    pub agent_profiles: Arc<dyn IWorkspaceAgentProfileLoader>,
}

#[derive(Clone)]
pub struct ProgramWorkdir {
    pub context_key: String,
    pub local_config: Arc<FileProjectLocalConfigService>,
    pub dirs: Arc<dyn IWorkspaceDirs>,
    pub fs: Arc<dyn IWorkspaceFsService>,
    pub instructions: Arc<dyn IWorkspaceInstructionsService>,
    pub skills: Arc<dyn IWorkspaceSkillCatalog>,
    pub agent_profiles: Arc<dyn IWorkspaceAgentProfileLoader>,
    dispose: Arc<dyn Fn() + Send + Sync>,
}

impl ProgramWorkdir {
    pub fn dispose(&self) {
        (self.dispose)()
    }
}

struct ProgramWorkdirEntry {
    view: ProgramView,
    disposables: SharedDisposables,
    references: std::sync::atomic::AtomicUsize,
}

pub struct ProgramWorkdirs {
    pub root: ProgramView,
    environments: Arc<dyn IEnvironmentService>,
    context: Arc<dyn IWorkspaceContext>,
    dependencies: Arc<ProgramDependencies>,
    harness: Arc<ProgramHarness>,
    entries: Arc<Mutex<IndexMap<String, Arc<ProgramWorkdirEntry>>>>,
}

impl ProgramWorkdirs {
    pub fn new(
        environments: Arc<dyn IEnvironmentService>,
        context: Arc<dyn IWorkspaceContext>,
        dependencies: Arc<ProgramDependencies>,
        harness: ProgramHarness,
        own_root: &SharedDisposables,
    ) -> Self {
        let harness = Arc::new(harness);
        let root_disposables = Arc::new(Mutex::new(Vec::new()));
        let root = Self::build_parts(
            environments.clone(),
            context.clone(),
            dependencies.clone(),
            harness.clone(),
            root_disposables.clone(),
            LOCAL_ENVIRONMENT_ID,
            &context.cwd(),
            true,
            own_root,
        );
        let entries = Arc::new(Mutex::new(IndexMap::new()));
        entries.lock().insert(
            workdir_context_key(LOCAL_ENVIRONMENT_ID, &context.cwd(), true),
            Arc::new(ProgramWorkdirEntry {
                view: root,
                disposables: root_disposables,
                references: std::sync::atomic::AtomicUsize::new(1),
            }),
        );
        Self {
            root: entries.lock().values().next().unwrap().view.clone(),
            environments,
            context,
            dependencies,
            harness,
            entries,
        }
    }

    pub async fn acquire(
        &self,
        environment_id: &str,
        cwd: &str,
        follows_primary: bool,
    ) -> Result<ProgramWorkdir, WireAsyncError> {
        let context_key = workdir_context_key(environment_id, cwd, follows_primary);
        let existing = self.entries.lock().get(&context_key).cloned();
        let stale = existing.is_some() && !watchable(environment_id);
        let entry = match existing {
            Some(entry) => entry,
            None => {
                let disposables = Arc::new(Mutex::new(Vec::new()));
                let view = match Self::build_parts(
                    self.environments.clone(),
                    self.context.clone(),
                    self.dependencies.clone(),
                    self.harness.clone(),
                    disposables.clone(),
                    environment_id,
                    cwd,
                    follows_primary,
                    &disposables,
                ) {
                    Ok(view) => view,
                    Err(error) => {
                        dispose_all(&disposables);
                        return Err(error);
                    }
                };
                let entry = Arc::new(ProgramWorkdirEntry {
                    view,
                    disposables,
                    references: std::sync::atomic::AtomicUsize::new(0),
                });
                self.entries.lock().insert(context_key.clone(), entry.clone());
                entry
            }
        };
        entry.references.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released_sink = released.clone();
        let entries = self.entries.clone();
        let entry_for_dispose = entry.clone();
        let key_for_dispose = context_key.clone();
        let workdir = ProgramWorkdir {
            context_key: context_key.clone(),
            local_config: entry.view.local_config.clone(),
            dirs: entry.view.dirs.clone(),
            fs: entry.view.fs.clone(),
            instructions: entry.view.instructions.clone(),
            skills: entry.view.skills.clone(),
            agent_profiles: entry.view.agent_profiles.clone(),
            dispose: Arc::new(move || {
                if released_sink.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let previous = entry_for_dispose.references.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                if previous > 1 {
                    return;
                }
                let same = entries
                    .lock()
                    .get(&key_for_dispose)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry_for_dispose));
                if !same {
                    return;
                }
                entries.lock().shift_remove(&key_for_dispose);
                dispose_all(&entry_for_dispose.disposables);
                let _ = released;
            }),
        };
        if stale {
            if let Err(error) = reload_view(&entry.view).await {
                workdir.dispose();
                return Err(error);
            }
        }
        Ok(workdir)
    }

    pub async fn reload(&self) -> Result<(), WireAsyncError> {
        for entry in self.entries.lock().values().cloned().collect::<Vec<_>>() {
            reload_view(&entry.view).await?;
        }
        Ok(())
    }

    pub fn dispose(&self) {
        for entry in self.entries.lock().values().cloned().collect::<Vec<_>>().into_iter().rev() {
            dispose_all(&entry.disposables);
        }
        self.entries.lock().clear();
    }

    fn build_parts(
        environments: Arc<dyn IEnvironmentService>,
        context: Arc<dyn IWorkspaceContext>,
        dependencies: Arc<ProgramDependencies>,
        harness: Arc<ProgramHarness>,
        disposables: SharedDisposables,
        environment_id: &str,
        cwd: &str,
        follows_primary: bool,
        own_root: &SharedDisposables,
    ) -> Result<ProgramView, WireAsyncError> {
        let lease = environments.acquire(environment_id, PROGRAM_CAPABILITIES);
        let result = (|| {
            let host_fs = lease.environment.fs.clone().unwrap();
            let local = environments.inspect(LOCAL_ENVIRONMENT_ID);
            let in_process = watchable(environment_id);
            let trust_covered = environment_id == LOCAL_ENVIRONMENT_ID;
            let git: LiveRef<Arc<dyn IGitService>> = if in_process {
                dependencies.git.clone()
            } else {
                LiveRef::constant(Arc::new(GitService::new(
                    environments.clone(),
                    host_fs.clone(),
                    environment_id.to_string(),
                )) as Arc<dyn IGitService>)
            };
            let context = Arc::new(ContextWithCwd {
                base: context,
                cwd: cwd.to_string(),
            });
            let state = own(
                WorkspaceStateService::new(Some(dependencies.app_state.clone())),
                own_root,
            );
            let local_config = Arc::new(FileProjectLocalConfigService::new(
                dependencies.bootstrap.clone(),
                host_fs.clone(),
                lease.environment.host.as_ref().and_then(|host| host.home_dir.clone()).unwrap_or_default(),
            ));
            let dirs = own(
                WorkspaceDirsService::new(
                    context.clone(),
                    local_config.clone(),
                    dependencies.log.clone(),
                    state.clone(),
                    dependencies.workspaces.clone(),
                    dependencies.workspace_store.clone(),
                    Some(environment_id.to_string()),
                    Some(follows_primary),
                ),
                own_root,
            );
            let git = Arc::new(WorkspaceGitService::new(dirs.clone(), git));
            let fs = Arc::new(WorkspaceFsService::new(
                context.clone(),
                dirs.clone(),
                host_fs.clone(),
                environments.clone(),
                dependencies.telemetry.clone(),
                git.clone(),
                Some(environment_id.to_string()),
            ));
            let instructions = own(
                WorkspaceInstructionsService::new(
                    dirs.clone(),
                    host_fs.clone(),
                    local.host.clone().unwrap(),
                    dependencies.bootstrap.clone(),
                    dependencies.log.clone(),
                    state.clone(),
                    local.fs.clone(),
                    in_process,
                ),
                own_root,
            );
            let agent_profiles: Arc<dyn IWorkspaceAgentProfileLoader> = if trust_covered {
                own(
                    WorkspaceAgentProfileLoaderService::new(
                        context.clone(),
                        host_fs.clone(),
                        dirs.clone(),
                        dependencies.log.clone(),
                        harness.user_agent_profiles.clone(),
                        Some(dependencies.agent_profiles.clone()),
                        Some(workdir_context_key(environment_id, cwd, follows_primary)),
                        in_process,
                        Arc::new(|registration| {
                            dependencies.agent_profiles.register(registration)
                        }),
                    ),
                    own_root,
                )
            } else {
                Arc::new(NoopWorkspaceAgentProfileLoader)
            };
            let workspace_skills = own_arc(
                WorkspaceRootSkillSource::new(
                    EnvironmentSkillDiscovery::new(dependencies.log.clone(), host_fs.clone()),
                    context.clone(),
                    dependencies.config.clone(),
                    dependencies.bootstrap.clone(),
                    dirs.clone(),
                    host_fs.clone(),
                    in_process,
                ),
                own_root,
            );
            let skills = own_arc(
                WorkspaceSkillCatalogService::new(
                    dependencies.builtin_skills.clone(),
                    dependencies.user_skills.clone(),
                    harness.explicit_skills.clone(),
                    harness.extra_skills.clone(),
                    workspace_skills,
                    harness.plugin_skills.clone(),
                    state.clone(),
                ),
                own_root,
            );
            Ok(ProgramView {
                state,
                local_config,
                dirs,
                git,
                fs,
                instructions,
                skills,
                agent_profiles,
            })
        })();
        lease.dispose();
        result
    }
}

impl ProgramDisposable for ProgramWorkdirs {
    fn dispose(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        ProgramWorkdirs::dispose(self);
        async { Ok(()) }.boxed()
    }
}

struct ContextWithCwd {
    base: Arc<dyn IWorkspaceContext>,
    cwd: String,
}

impl IWorkspaceContext for ContextWithCwd {
    fn workspace_id(&self) -> String {
        self.base.workspace_id()
    }

    fn cwd(&self) -> String {
        self.cwd.clone()
    }

    fn source(&self) -> crate::workspace::workspace_context::workspace_context::WorkspaceSource {
        self.base.source()
    }

    fn remote_cwd(&self) -> Option<String> {
        self.base.remote_cwd()
    }

    fn meta(&self) -> crate::workspace::workspace_context::workspace_context::WorkspaceMeta {
        self.base.meta()
    }

    fn persistence_scope(&self) -> String {
        self.base.persistence_scope()
    }
}

struct NoopWorkspaceAgentProfileLoader;

impl IWorkspaceAgentProfileLoader for NoopWorkspaceAgentProfileLoader {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        async { Ok(()) }.boxed()
    }

    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        async { Ok(()) }.boxed()
    }
}

fn workdir_context_key(environment_id: &str, cwd: &str, follows_primary: bool) -> String {
    let base = environment_binding_id(environment_id, cwd);
    if environment_id == LOCAL_ENVIRONMENT_ID && !follows_primary {
        format!("{base}\0pinned")
    } else {
        base
    }
}

fn watchable(environment_id: &str) -> bool {
    environment_id == LOCAL_ENVIRONMENT_ID
}

async fn reload_view(view: &ProgramView) -> Result<(), WireAsyncError> {
    view.dirs.reload().await?;
    view.instructions.reload().await?;
    view.skills.reload().await?;
    let _ = view.agent_profiles.reload().await;
    Ok(())
}

fn dispose_all(disposables: &SharedDisposables) {
    for disposable in disposables.lock().drain(..).rev() {
        tokio::spawn(disposable.dispose());
    }
}

macro_rules! sync_disposable {
    ($type:ty) => {
        impl ProgramDisposable for $type {
            fn dispose(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
                self.dispose();
                async { Ok(()) }.boxed()
            }
        }
    };
}

sync_disposable!(WorkspaceStateService);
sync_disposable!(WorkspaceDirsService);
sync_disposable!(WorkspaceInstructionsService);
sync_disposable!(WorkspaceAgentProfileLoaderService);
sync_disposable!(WorkspaceRootSkillSource);
sync_disposable!(WorkspaceSkillCatalogService);
sync_disposable!(ExplicitFileSkillSource);
sync_disposable!(ExtraFileSkillSource);
sync_disposable!(crate::workspace::workspace_agent_profile_loader::user_agent_profile_loader_service::UserAgentProfileLoaderService);
sync_disposable!(crate::workspace::workspace_agent_profile_loader::plugin_agent_profile_loader_service::PluginAgentProfileLoaderService);
sync_disposable!(crate::workspace::workspace_agent_profile_loader::explicit_agent_profile_loader_service::ExplicitAgentProfileLoaderService);
sync_disposable!(crate::workspace::workspace_agent_profile_loader::extra_agent_profile_loader_service::ExtraAgentProfileLoaderService);
sync_disposable!(crate::workspace::workspace_trust::workspace_trust_service::WorkspaceTrustService);
sync_disposable!(crate::workspace::workspace_mcp_config::workspace_mcp_config_service::WorkspaceMcpConfigService);
