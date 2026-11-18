use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::_base::log::log::ILogService;
use crate::_base::utils::paths::{is_absolute, relative};
use crate::app::agent_profile_catalog::agent_profile_registry::IAgentProfileRegistry;
use crate::app::agent_profile_catalog::builtin_agent_profile_loader::BUILTIN_AGENT_PROFILE_SOURCE_ID;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::git::work_tree::find_git_work_tree;
use crate::app::mcp_config::config_loader::{load_mcp_servers_detailed, resolve_mcp_json_paths};
use crate::app::project_local_config::project_local_config::IProjectLocalConfigService;
use crate::features::skill::catalog::types::SkillCatalog;
use crate::features::skill::catalog::config_section::MERGE_ALL_AVAILABLE_SKILLS_SECTION;
use crate::features::skill::catalog::skill_roots::project_roots_for_dirs;
use crate::features::skill::workspace::workspace_skill_catalog::IWorkspaceSkillCatalog;
use crate::mcp_core::config_schema::McpServerConfig;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::workspace::workspace_agent_profile_loader::workspace_agent_profile_loader::IWorkspaceAgentProfileLoader;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;
use crate::workspace::workspace_instructions::workspace_instructions::IWorkspaceInstructionsService;

use super::trust_disclosure::{
    IWorkspaceTrustDisclosure, TrustGatedActivation, TrustGatedInstructionSources,
    TrustGatedMcpServer, TrustGatedMcpTransport,
};
use super::workspace_trust::{IWorkspaceTrust, WorkspaceTrustAsyncError};

static EMPTY_INSTRUCTION_SOURCES: LazyLock<TrustGatedInstructionSources> =
    LazyLock::new(TrustGatedInstructionSources::default);
static EMPTY_ACTIVATION: LazyLock<TrustGatedActivation> =
    LazyLock::new(TrustGatedActivation::default);

const SKILL_DISCLOSURE_TIMEOUT_MS: u64 = 1000;

struct WorkspaceTrustDisclosureServiceInner {
    context: Arc<dyn IWorkspaceContext>,
    fs: Arc<dyn IHostFileSystem>,
    bootstrap: Arc<dyn IBootstrapService>,
    config: Arc<dyn IConfigService>,
    local_config: Arc<dyn IProjectLocalConfigService>,
    workspace_dirs: Arc<dyn IWorkspaceDirs>,
    trust: Arc<dyn IWorkspaceTrust>,
    skills: Arc<dyn IWorkspaceSkillCatalog>,
    agent_profiles_loader: Arc<dyn IWorkspaceAgentProfileLoader>,
    agent_profiles_registry: Arc<dyn IAgentProfileRegistry>,
    instructions: Arc<dyn IWorkspaceInstructionsService>,
    log: Arc<dyn ILogService>,
}

#[derive(Clone)]
pub struct WorkspaceTrustDisclosureService {
    inner: Arc<WorkspaceTrustDisclosureServiceInner>,
}

impl WorkspaceTrustDisclosureService {
    pub fn new(
        context: Arc<dyn IWorkspaceContext>,
        fs: Arc<dyn IHostFileSystem>,
        bootstrap: Arc<dyn IBootstrapService>,
        config: Arc<dyn IConfigService>,
        local_config: Arc<dyn IProjectLocalConfigService>,
        workspace_dirs: Arc<dyn IWorkspaceDirs>,
        trust: Arc<dyn IWorkspaceTrust>,
        skills: Arc<dyn IWorkspaceSkillCatalog>,
        agent_profiles_loader: Arc<dyn IWorkspaceAgentProfileLoader>,
        agent_profiles_registry: Arc<dyn IAgentProfileRegistry>,
        instructions: Arc<dyn IWorkspaceInstructionsService>,
        log: Arc<dyn ILogService>,
    ) -> Self {
        Self {
            inner: Arc::new(WorkspaceTrustDisclosureServiceInner {
                context,
                fs,
                bootstrap,
                config,
                local_config,
                workspace_dirs,
                trust,
                skills,
                agent_profiles_loader,
                agent_profiles_registry,
                instructions,
                log,
            }),
        }
    }

    pub async fn describe_gated_activation(
        &self,
        pending_additional_dirs: Option<&[String]>,
        base_dir: Option<&str>,
    ) -> Result<TrustGatedActivation, WorkspaceTrustAsyncError> {
        let base_dir = base_dir
            .map(str::to_string)
            .unwrap_or_else(|| self.inner.workspace_dirs.primary());
        if self.inner.trust.get().await? {
            return Ok(EMPTY_ACTIVATION.clone());
        }
        self.inner.workspace_dirs.ready().await?;
        let pending_additional_dirs = pending_additional_dirs.unwrap_or(&[]);
        let pending = if pending_additional_dirs.is_empty() {
            Vec::new()
        } else {
            self.inner
                .local_config
                .resolve_additional_dirs(&base_dir, pending_additional_dirs, Some(crate::app::project_local_config::project_local_config::AdditionalDirSource::Session))
                .await?
        };
        let mut work_dirs = self.inner.workspace_dirs.shared_additional_dirs();
        work_dirs.extend(pending);
        dedup(&mut work_dirs);
        let _ = self
            .inner
            .skills
            .reload_sources(&["workspace".to_string()])
            .await;
        let warnings = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (mcp_servers, configured_dirs, skill_roots, instruction_sources) = tokio::join!(
            async {
                match self.describe_gated_mcp_servers().await {
                    Ok(servers) => servers,
                    Err(error) => {
                        self.inner.log.warn(
                            &format!("trust disclosure: MCP scan failed: {error}"),
                            None,
                        );
                        warnings
                            .lock()
                            .push("Could not inspect MCP configuration.".to_string());
                        Vec::new()
                    }
                }
            },
            async {
                match self.read_gated_additional_dirs(&work_dirs).await {
                    Ok(dirs) => dirs,
                    Err(error) => {
                        self.inner.log.warn(
                            &format!("trust disclosure: additional dirs scan failed: {error}"),
                            None,
                        );
                        warnings.lock().push(
                            "Could not inspect additional directory configuration.".to_string(),
                        );
                        (Vec::new(), Vec::new())
                    }
                }
            },
            async {
                match self.read_gated_skill_roots(&work_dirs).await {
                    Ok(roots) => roots,
                    Err(error) => {
                        self.inner.log.warn(
                            &format!("trust disclosure: skill roots scan failed: {error}"),
                            None,
                        );
                        warnings
                            .lock()
                            .push("Could not inspect project skill directories.".to_string());
                        Vec::new()
                    }
                }
            },
            async {
                match self.describe_instruction_sources(warnings.clone()).await {
                    Ok(sources) => sources,
                    Err(error) => {
                        self.inner.log.warn(
                            &format!("trust disclosure: instruction sources scan failed: {error}"),
                            None,
                        );
                        warnings
                            .lock()
                            .push("Could not inspect project instructions.".to_string());
                        EMPTY_INSTRUCTION_SOURCES.clone()
                    }
                }
            }
        );
        let (configured_dirs, configured_sources) = configured_dirs;
        let mut additional_dirs = configured_dirs;
        additional_dirs.extend(skill_roots.iter().cloned());
        dedup(&mut additional_dirs);
        let mut additional_dir_sources = configured_sources;
        additional_dir_sources.extend(skill_roots);
        dedup(&mut additional_dir_sources);
        let warnings = warnings.lock().clone();
        Ok(TrustGatedActivation {
            mcp_servers,
            additional_dirs,
            additional_dir_sources,
            warnings,
            instruction_sources,
        })
    }

    async fn describe_gated_mcp_servers(
        &self,
    ) -> Result<Vec<TrustGatedMcpServer>, WorkspaceTrustAsyncError> {
        let cwd = self.inner.workspace_dirs.primary();
        let home_dir = self.inner.bootstrap.home_dir();
        let (paths, loaded) = tokio::try_join!(
            resolve_mcp_json_paths(self.inner.fs.as_ref(), &cwd, &home_dir),
            load_mcp_servers_detailed(self.inner.fs.as_ref(), &cwd, &home_dir, true)
        )?;
        let project_paths = HashSet::from([paths.project_root, paths.project]);
        let mut servers = loaded
            .servers
            .iter()
            .filter(|(name, _)| {
                project_paths.contains(loaded.origins.get(*name).map_or("", String::as_str))
            })
            .filter(|(_, config)| config.enabled != Some(false))
            .map(|(name, config)| {
                describe_mcp_server(
                    name,
                    config,
                    loaded.origins.get(name).map_or("", String::as_str),
                )
            })
            .collect::<Vec<_>>();
        servers.sort_by(|a, b| locale_compare(&a.name, &b.name));
        let mut output = Vec::new();
        for mut server in servers {
            server.origin = realpath_or_self(self.inner.fs.as_ref(), &server.origin).await;
            output.push(server);
        }
        Ok(output)
    }

    async fn read_gated_additional_dirs(
        &self,
        work_dirs: &[String],
    ) -> Result<(Vec<String>, Vec<String>), WorkspaceTrustAsyncError> {
        let result = self
            .inner
            .local_config
            .read_additional_dirs(&self.inner.workspace_dirs.primary())
            .await?;
        let real_root = realpath_or_self(self.inner.fs.as_ref(), &result.project_root).await;
        let mut dirs = Vec::new();
        for dir in result.additional_dirs.iter().chain(work_dirs.iter()) {
            let real_path = realpath_or_self(self.inner.fs.as_ref(), dir).await;
            if is_inside_or_equal_dir(&real_path, &real_root) || dirs.contains(&real_path) {
                continue;
            }
            dirs.push(real_path);
        }
        let sources = if dirs.is_empty() {
            Vec::new()
        } else {
            vec![realpath_or_self(self.inner.fs.as_ref(), &result.config_path).await]
        };
        Ok((dirs, sources))
    }

    async fn read_gated_skill_roots(
        &self,
        work_dirs: &[String],
    ) -> Result<Vec<String>, WorkspaceTrustAsyncError> {
        if self
            .inner
            .bootstrap
            .args()
            .skill_dirs
            .as_ref()
            .is_some_and(|dirs| !dirs.is_empty())
        {
            return Ok(Vec::new());
        }
        let merge_all_available_skills = self
            .inner
            .config
            .get(MERGE_ALL_AVAILABLE_SKILLS_SECTION)
            .as_ref()
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let project_root = find_git_work_tree(
            self.inner.fs.as_ref(),
            &self.inner.workspace_dirs.primary(),
        )
        .await?
        .map_or_else(|| self.inner.workspace_dirs.primary(), |tree| tree.root);
        let mut dirs = vec![self.inner.workspace_dirs.primary()];
        dirs.extend(work_dirs.iter().cloned());
        let (roots, real_root) = tokio::join!(
            project_roots_for_dirs(&dirs, crate::features::skill::catalog::skill_roots::SkillRootsOptions { merge_all_available_skills }),
            realpath_or_self(self.inner.fs.as_ref(), &project_root)
        );
        let roots = roots?;
        Ok(roots
            .into_iter()
            .filter(|root| !is_inside_or_equal_dir(&root.path, &real_root))
            .map(|root| root.path)
            .collect())
    }

    async fn describe_instruction_sources(
        &self,
        warnings: Arc<parking_lot::Mutex<Vec<String>>>,
    ) -> Result<TrustGatedInstructionSources, WorkspaceTrustAsyncError> {
        let (skills_ready, profiles_ready, instructions_ready) = tokio::join!(
            wait_for_ready(
                {
                    let skills = self.inner.skills.clone();
                    Box::pin(async move {
                        skills
                            .ready()
                            .await
                            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> {
                                Box::new(std::io::Error::other(error))
                            })
                    }) as BoxFuture<'static, Result<(), Box<dyn std::error::Error + Send + Sync>>>
                },
                SKILL_DISCLOSURE_TIMEOUT_MS,
            ),
            self.inner.agent_profiles_loader.ready(),
            self.inner.instructions.ready()
        );
        profiles_ready?;
        instructions_ready?;
        if !skills_ready {
            warnings.lock().push(
                "Project skills are still loading; inspect the project skill directories."
                    .to_string(),
            );
        }
        let project_root = find_git_work_tree(
            self.inner.fs.as_ref(),
            &self.inner.workspace_dirs.primary(),
        )
        .await?
        .map_or_else(|| self.inner.workspace_dirs.primary(), |tree| tree.root);
        let snapshot = self.inner.instructions.snapshot();
        if let Some(warning) = snapshot.agents_md_warning {
            warnings.lock().push(warning);
        }
        let skills = self
            .inner
            .skills
            .catalog()
            .list_skills()
            .into_iter()
            .filter(|skill| skill.source == crate::features::skill::catalog::types::SkillSource::Project)
            .collect::<Vec<_>>();
        let profiles = self.effective_workspace_profiles();
        let mut agents_md_paths = Vec::new();
        for path in snapshot.agents_md_paths.unwrap_or_default() {
            if !is_inside_or_equal_dir(&path, &project_root) {
                continue;
            }
            agents_md_paths.push(realpath_or_self(self.inner.fs.as_ref(), &path).await);
        }
        agents_md_paths.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        let workspace_profiles = self
            .inner
            .agent_profiles_registry
            .entries()
            .into_iter()
            .find(|entry| {
                entry.source_id == "workspace"
                    && entry.workspace_key.as_deref()
                        == Some(self.inner.context.workspace_id().as_str())
            });
        let skill_paths = if skills_ready {
            self.source_locations(
                &skills.iter().map(|skill| skill.path.clone()).collect::<Vec<_>>(),
                &self.inner.skills.catalog().get_skill_roots(),
            )
            .await?
        } else {
            self.project_skill_root_paths().await?
        };
        let mut profile_paths = Vec::new();
        if !profiles.is_empty() {
            for root in workspace_profiles
                .as_ref()
                .map(|entry| entry.contribution.scanned_roots.clone())
                .unwrap_or_default()
            {
                profile_paths.push(format!(
                    "{}/",
                    realpath_or_self(self.inner.fs.as_ref(), &root.join("/")).await
                ));
            }
        }
        let mut paths = agents_md_paths.clone();
        paths.extend(skill_paths);
        paths.extend(profile_paths);
        dedup(&mut paths);
        let mut skill_names = skills
            .into_iter()
            .map(|skill| skill.name)
            .collect::<Vec<_>>();
        skill_names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        Ok(TrustGatedInstructionSources {
            agents_md_paths,
            skills: skill_names,
            agent_profiles: profiles,
            paths,
        })
    }

    async fn source_locations(
        &self,
        paths: &[String],
        roots: &[String],
    ) -> Result<Vec<String>, WorkspaceTrustAsyncError> {
        let mut real_roots = Vec::new();
        for root in roots {
            real_roots.push(realpath_or_self(self.inner.fs.as_ref(), root).await);
        }
        real_roots.sort_by(|left, right| {
            right
                .encode_utf16()
                .count()
                .cmp(&left.encode_utf16().count())
        });
        let mut locations = Vec::new();
        for path in paths {
            let real_path = realpath_or_self(self.inner.fs.as_ref(), path).await;
            let root = real_roots
                .iter()
                .find(|root| is_inside_or_equal_dir(&real_path, root));
            locations.push(match root {
                Some(root) => format!("{root}/"),
                None => real_path,
            });
        }
        dedup(&mut locations);
        locations.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        Ok(locations)
    }

    async fn project_skill_root_paths(
        &self,
    ) -> Result<Vec<String>, WorkspaceTrustAsyncError> {
        let merge_all_available_skills = self
            .inner
            .config
            .get(MERGE_ALL_AVAILABLE_SKILLS_SECTION)
            .as_ref()
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let mut dirs = vec![self.inner.workspace_dirs.primary()];
        dirs.extend(self.inner.workspace_dirs.shared_additional_dirs());
        let mut roots = project_roots_for_dirs(&dirs, crate::features::skill::catalog::skill_roots::SkillRootsOptions { merge_all_available_skills })
            .await?
            .into_iter()
            .map(|root| root.path)
            .collect::<Vec<_>>();
        roots.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        Ok(roots)
    }

    fn effective_workspace_profiles(&self) -> Vec<String> {
        let entries = self
            .inner
            .agent_profiles_registry
            .entries()
            .into_iter()
            .filter(|entry| {
                entry.workspace_key.is_none()
                    || entry.workspace_key.as_deref()
                        == Some(self.inner.context.workspace_id().as_str())
            })
            .collect::<Vec<_>>();
        let builtin_names = entries
            .iter()
            .find(|entry| entry.source_id == BUILTIN_AGENT_PROFILE_SOURCE_ID)
            .map(|entry| {
                entry
                    .contribution
                    .profiles
                    .iter()
                    .map(|profile| profile.name.clone())
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let mut ordered = entries
            .into_iter()
            .filter(|entry| entry.source_id != BUILTIN_AGENT_PROFILE_SOURCE_ID)
            .collect::<Vec<_>>();
        ordered.sort_by(|a, b| b.priority.cmp(&a.priority));
        let mut winners = HashMap::<String, String>::new();
        for entry in ordered {
            let mut seen = HashSet::new();
            for profile in entry.contribution.profiles {
                if !seen.insert(profile.name.clone()) || winners.contains_key(&profile.name) {
                    continue;
                }
                if builtin_names.contains(&profile.name) && profile.override_ != Some(true) {
                    continue;
                }
                winners.insert(profile.name, entry.source_id.clone());
            }
        }
        let mut names = winners
            .into_iter()
            .filter(|(_, source_id)| source_id == "workspace")
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        names
    }
}

#[async_trait]
impl IWorkspaceTrustDisclosure for WorkspaceTrustDisclosureService {
    async fn describe_gated_activation(
        &self,
        pending_additional_dirs: Option<&[String]>,
        base_dir: Option<&str>,
    ) -> Result<TrustGatedActivation, WorkspaceTrustAsyncError> {
        WorkspaceTrustDisclosureService::describe_gated_activation(
            self,
            pending_additional_dirs,
            base_dir,
        )
        .await
    }
}

fn describe_mcp_server(
    name: &str,
    config: &McpServerConfig,
    origin: &str,
) -> TrustGatedMcpServer {
    if config.transport.as_str() == "stdio" {
        return TrustGatedMcpServer {
            name: name.to_string(),
            transport: TrustGatedMcpTransport::Stdio,
            command: Some(config.command.clone().unwrap()),
            args: config.args.clone(),
            cwd: config.cwd.clone(),
            url: None,
            origin: origin.to_string(),
        };
    }
    TrustGatedMcpServer {
        name: name.to_string(),
        transport: if config.transport.as_str() == "http" {
            TrustGatedMcpTransport::Http
        } else {
            TrustGatedMcpTransport::Sse
        },
        command: None,
        args: None,
        cwd: None,
        url: Some(config.url.clone().unwrap()),
        origin: origin.to_string(),
    }
}

async fn realpath_or_self(fs: &dyn IHostFileSystem, dir: &str) -> String {
    fs.realpath(dir)
        .await
        .unwrap_or_else(|_| dir.to_string())
}

fn is_inside_or_equal_dir(child: &str, parent: &str) -> bool {
    let rel = relative(parent, child);
    rel.is_empty() || (rel != ".." && !rel.starts_with("../") && !is_absolute(&rel))
}

async fn wait_for_ready(
    ready: BoxFuture<'static, Result<(), WorkspaceTrustAsyncError>>,
    timeout_ms: u64,
) -> bool {
    let ready = tokio::spawn(ready);
    matches!(
        tokio::time::timeout(Duration::from_millis(timeout_ms), ready).await,
        Ok(Ok(Ok(())))
    )
}

fn dedup(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    let script = format!(
        "String((function (left, right) {{ return left.localeCompare(right); }})({}, {}))",
        serde_json::to_string(left).unwrap(),
        serde_json::to_string(right).unwrap()
    );
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| ctx.eval::<String, _>(script))
            })
        })
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .map(|value| value.cmp(&0))
        .unwrap_or_else(|| left.encode_utf16().cmp(right.encode_utf16()))
}
