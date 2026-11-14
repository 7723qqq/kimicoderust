use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::{BoxFuture, join_all};
use indexmap::IndexMap;
use kimi_core::primitives::errors::{Error2, to_error_message};
use kimi_core::primitives::errors::domains::core::VALIDATION_FAILED;
use kimi_core::primitives::errors::domains::fs::FS_PATH_NOT_FOUND;
use kimi_core::primitives::errors::domains::mcp::MCP_SERVER_NOT_FOUND;
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::_base::errors::errors::BugIndicatingError;
use crate::_base::log::log::ILogService;
use crate::app::bootstrap::bootstrap::PluginNodeRunner;
use crate::features::externalHooks::internal::types::HookDef;
use crate::features::skill::catalog::file_skill_discovery::discover_file_skills;
use crate::features::skill::catalog::skill_discovery::SkillDiscoveryResult;
use crate::features::skill::catalog::types::{SkillPluginContext, SkillRoot};
use crate::mcp_core::config_schema::{McpServerConfig, McpTransport};
use crate::wire::record::WireAsyncError;

use super::archive::{download_zip, extract_zip};
use super::commands::{LoadPluginCommandInput, load_plugin_command};
use super::errors::{PLUGIN_LOAD_FAILED, PLUGIN_NOT_FOUND};
use super::github_resolver::{
    GithubSourceInput, GithubSourceKind, resolve_github_commit_sha, resolve_github_source,
};
use super::manifest::{ParsedManifestResult, parse_manifest};
pub use super::node_fallback::KIMI_NODE_FALLBACK_SUBCOMMAND;
use super::source::{GithubRef, GithubRefKind, ResolvedSource, resolve_install_source};
use super::store::{InstalledFile, InstalledRecord, read_installed, write_installed};
use super::types::{
    EnabledPluginSessionStart, EnabledPluginSystemPrompt, PluginAgentRoot, PluginCapabilityState,
    PluginCommandDef, PluginDiagnosticSeverity, PluginGithubMetadata, PluginGithubRef,
    PluginGithubRefKind, PluginInfo, PluginInterface, PluginMcpServerEntry, PluginMcpServerInfo,
    PluginOrigin, PluginRecord, PluginSource, PluginState, PluginSummary, PluginUpdateStatus,
    ReloadSummary, normalize_plugin_id,
};

pub type DiscoverSkills = Arc<
    dyn Fn(Vec<SkillRoot>) -> BoxFuture<'static, Result<SkillDiscoveryResult, WireAsyncError>>
        + Send
        + Sync,
>;

pub struct PluginManagerOptions {
    pub kimi_home_dir: String,
    pub discover_skills: Option<DiscoverSkills>,
    pub log: Option<Arc<dyn ILogService>>,
    pub plugin_node_runner: Option<PluginNodeRunner>,
}

#[derive(Debug, Clone)]
pub struct PluginProvenance {
    pub origin: PluginOrigin,
    pub catalog_id: Option<String>,
    pub marketplace_source: Option<String>,
}

#[derive(Debug, Clone)]
struct ManagedPluginCopy {
    root: String,
    previous_root: Option<String>,
}

pub struct PluginManager {
    kimi_home_dir: String,
    discover_skills: DiscoverSkills,
    log: Option<Arc<dyn ILogService>>,
    plugin_node_runner: Option<PluginNodeRunner>,
    records: RwLock<IndexMap<String, PluginRecord>>,
}

impl PluginManager {
    pub fn new(options: PluginManagerOptions) -> Self {
        let discover_skills = options.discover_skills.unwrap_or_else(|| {
            Arc::new(|roots| {
                Box::pin(async move { discover_file_skills(roots).await })
                    as BoxFuture<'static, Result<SkillDiscoveryResult, WireAsyncError>>
            })
        });
        let plugin_node_runner = options.plugin_node_runner.or_else(|| {
            is_kimi_native_binary().then(|| PluginNodeRunner {
                command: std::env::current_exe()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                args: vec![KIMI_NODE_FALLBACK_SUBCOMMAND.to_string()],
            })
        });
        Self {
            kimi_home_dir: options.kimi_home_dir,
            discover_skills,
            log: options.log,
            plugin_node_runner,
            records: RwLock::new(IndexMap::new()),
        }
    }

    pub async fn load(&self) -> Result<(), WireAsyncError> {
        let file = read_installed(&self.kimi_home_dir).await?;
        let mut next = IndexMap::new();
        for entry in file.plugins {
            next.insert(entry.id.clone(), self.materialize(entry).await?);
        }
        *self.records.write() = next;
        Ok(())
    }

    pub fn list(&self) -> Vec<PluginRecord> {
        let mut records = self.records.read().values().cloned().collect::<Vec<_>>();
        records.sort_by(|left, right| left.id.cmp(&right.id));
        records
    }

    pub fn get(&self, id: &str) -> Option<PluginRecord> {
        self.records.read().get(&normalize_plugin_id(id)).cloned()
    }

    pub async fn install(
        &self,
        source: &str,
        provenance: Option<PluginProvenance>,
    ) -> Result<PluginRecord, WireAsyncError> {
        let mut zip_tmp_dir = None;
        let mut managed_copy: Option<ManagedPluginCopy> = None;
        let operation = async {
            let resolved = resolve_install_source(source)?;
            let (source_root, original_source, source_type, github) = match resolved {
                ResolvedSource::LocalPath { path } => {
                    let source_root = normalize_install_root(&path).await?;
                    (source_root, path, PluginSource::LocalPath, None)
                }
                ResolvedSource::ZipUrl { path } => {
                    let buffer = download_zip(&path, None).await?;
                    let temporary = make_temp_dir(&std::env::temp_dir(), "kimi-plugin-zip-").await?;
                    zip_tmp_dir = Some(temporary.clone());
                    let source_root = extract_zip(buffer, &temporary.to_string_lossy()).await?;
                    (
                        source_root,
                        source.trim().to_string(),
                        PluginSource::ZipUrl,
                        None,
                    )
                }
                ResolvedSource::Github { owner, repo, ref_ } => {
                    let resolution = resolve_github_source(GithubSourceInput {
                        kind: GithubSourceKind::Github,
                        owner: owner.clone(),
                        repo: repo.clone(),
                        ref_: ref_.clone(),
                    })
                    .await?;
                    let installed_sha = installed_github_sha(&owner, &repo, &resolution.ref_).await?;
                    let github = PluginGithubMetadata {
                        owner: owner.clone(),
                        repo: repo.clone(),
                        ref_: resolution.ref_.clone(),
                        installed_sha: installed_sha.clone(),
                    };
                    let zip_url = installed_sha.map_or_else(
                        || resolution.tarball_url.clone(),
                        |sha| format!("https://codeload.github.com/{owner}/{repo}/zip/{sha}"),
                    );
                    let buffer = download_zip(&zip_url, None).await?;
                    let temporary = make_temp_dir(&std::env::temp_dir(), "kimi-plugin-zip-").await?;
                    zip_tmp_dir = Some(temporary.clone());
                    let source_root = extract_zip(buffer, &temporary.to_string_lossy()).await?;
                    (
                        source_root,
                        source.trim().to_string(),
                        PluginSource::Github,
                        Some(github),
                    )
                }
            };
            let parsed = parse_manifest(&source_root).await;
            let Some(manifest) = &parsed.manifest else {
                let message = parsed
                    .diagnostics
                    .iter()
                    .find(|diagnostic| diagnostic.severity == PluginDiagnosticSeverity::Error)
                    .map(|diagnostic| diagnostic.message.clone())
                    .unwrap_or_else(|| "no manifest".to_string());
                let message = if source_type == PluginSource::LocalPath {
                    format!("Cannot install plugin at {source_root}: {message}")
                } else {
                    format!("Cannot install plugin from {original_source}: {message}")
                };
                let mut details = Map::new();
                details.insert("sourceType".to_string(), json!(source_type));
                return Err(Box::new(
                    Error2::new(PLUGIN_LOAD_FAILED, message).with_details(details),
                ) as WireAsyncError);
            };
            let id = normalize_plugin_id(&manifest.name);
            let copy = copy_plugin_to_managed_root(&self.kimi_home_dir, &id, &source_root).await?;
            managed_copy = Some(copy);
            let normalized_root = managed_copy.as_ref().unwrap().root.clone();
            let managed_parsed = parse_manifest(&normalized_root).await;
            let existing = self.records.read().get(&id).cloned();
            let now = iso_now();
            let record = record_from(RecordFromInput {
                id: id.clone(),
                root: normalized_root,
                enabled: existing.as_ref().is_none_or(|existing| existing.enabled),
                installed_at: existing
                    .as_ref()
                    .map(|existing| existing.installed_at.clone())
                    .unwrap_or_else(|| now.clone()),
                updated_at: Some(now),
                original_source: Some(original_source),
                origin: provenance.as_ref().map(|provenance| provenance.origin),
                catalog_id: provenance.as_ref().and_then(|provenance| provenance.catalog_id.clone()),
                marketplace_source: provenance
                    .as_ref()
                    .and_then(|provenance| provenance.marketplace_source.clone()),
                source: Some(source_type),
                capabilities: existing.as_ref().and_then(|existing| existing.capabilities.clone()),
                github,
                parsed: managed_parsed,
                discover_skills: self.discover_skills.clone(),
            })
            .await?;
            let mut next = self.records.read().clone();
            next.insert(id, record.clone());
            self.persist(&next).await?;
            *self.records.write() = next;
            if let Some(previous_root) = managed_copy
                .as_ref()
                .and_then(|copy| copy.previous_root.clone())
            {
                let _ = tokio::fs::remove_dir_all(previous_root).await;
            }
            managed_copy = None;
            Ok(record)
        }
        .await;
        if let Some(temporary) = zip_tmp_dir {
            let _ = tokio::fs::remove_dir_all(temporary).await;
        }
        match operation {
            Ok(record) => Ok(record),
            Err(error) => {
                if let Some(copy) = managed_copy
                    && let Err(rollback_error) = rollback_managed_plugin_copy(&copy).await
                {
                    let combined = PluginRollbackError {
                        original: error.to_string(),
                        rollback: rollback_error.to_string(),
                    };
                    return Err(Box::new(
                        Error2::new(
                            PLUGIN_LOAD_FAILED,
                            "Plugin installation failed and the previous managed copy could not be restored",
                        )
                        .with_cause(Box::new(combined)),
                    ));
                }
                Err(error)
            }
        }
    }

    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), WireAsyncError> {
        let key = normalize_plugin_id(id);
        let current = self.records.read().get(&key).cloned().ok_or_else(|| plugin_not_found(id))?;
        if current.enabled == enabled {
            return Ok(());
        }
        let mut next = self.records.read().clone();
        let updated = next.get_mut(&key).unwrap();
        updated.enabled = enabled;
        updated.updated_at = Some(iso_now());
        self.persist(&next).await?;
        *self.records.write() = next;
        Ok(())
    }

    pub async fn set_mcp_server_enabled(
        &self,
        id: &str,
        server: &str,
        enabled: bool,
    ) -> Result<(), WireAsyncError> {
        let key = normalize_plugin_id(id);
        let current = self.records.read().get(&key).cloned().ok_or_else(|| plugin_not_found(id))?;
        if current
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.mcp_servers.as_ref())
            .is_none_or(|servers| !servers.contains_key(server))
        {
            let mut details = Map::new();
            details.insert("id".to_string(), Value::String(id.to_string()));
            details.insert("server".to_string(), Value::String(server.to_string()));
            return Err(Box::new(
                Error2::new(
                    MCP_SERVER_NOT_FOUND,
                    format!("Plugin \"{id}\" does not declare MCP server \"{server}\""),
                )
                .with_details(details),
            ));
        }
        let mut capabilities = current.capabilities.clone().unwrap_or_default();
        let mut servers = capabilities.mcp_servers.clone().unwrap_or_default();
        servers.insert(
            server.to_string(),
            super::types::PluginMcpServerState { enabled },
        );
        capabilities.mcp_servers = Some(servers);
        let mut next = self.records.read().clone();
        let updated = next.get_mut(&key).unwrap();
        updated.capabilities = Some(capabilities);
        updated.updated_at = Some(iso_now());
        self.persist(&next).await?;
        *self.records.write() = next;
        Ok(())
    }

    pub async fn remove(&self, id: &str) -> Result<(), WireAsyncError> {
        let key = normalize_plugin_id(id);
        let record = self.records.read().get(&key).cloned().ok_or_else(|| plugin_not_found(id))?;
        assert_managed_plugin_root(&record.root, &self.kimi_home_dir).await?;
        let mut next = self.records.read().clone();
        next.shift_remove(&key);
        match tokio::fs::remove_dir_all(&record.root).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Box::new(error)),
        }
        self.persist(&next).await?;
        *self.records.write() = next;
        Ok(())
    }

    pub async fn check_updates(&self) -> Result<Vec<PluginUpdateStatus>, WireAsyncError> {
        let records = self
            .records
            .read()
            .values()
            .filter(|record| record.source == PluginSource::Github && record.github.is_some())
            .cloned()
            .collect::<Vec<_>>();
        let results = join_all(records.into_iter().map(|record| async move {
            match check_github_update(&record).await {
                Ok(status) => status,
                Err(error) => {
                    if let Some(log) = &self.log {
                        log.warn(
                            "plugin update check failed",
                            Some(json!({"id": record.id, "error": error.to_string()})),
                        );
                    }
                    failed_update_status(&record, error.as_ref())
                }
            }
        }))
        .await;
        let mut results = results;
        results.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(results)
    }

    pub async fn reload(&self) -> Result<ReloadSummary, WireAsyncError> {
        let previous_ids = self.records.read().keys().cloned().collect::<Vec<_>>();
        let file = read_installed(&self.kimi_home_dir).await?;
        let mut next = IndexMap::new();
        let mut errors = Vec::new();
        for entry in file.plugins {
            match self.materialize(entry.clone()).await {
                Ok(record) => {
                    next.insert(entry.id, record);
                }
                Err(error) => errors.push(super::types::PluginReloadError {
                    id: entry.id,
                    message: error.to_string(),
                }),
            }
        }
        let added = next
            .keys()
            .filter(|id| !previous_ids.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        let removed = previous_ids
            .iter()
            .filter(|id| !next.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        *self.records.write() = next;
        Ok(ReloadSummary {
            added,
            removed,
            errors,
        })
    }

    pub fn enabled_hooks(&self) -> Vec<HookDef> {
        let mut output = Vec::new();
        for record in self.records.read().values() {
            if !record.enabled || record.state != PluginState::Ok || record.manifest.is_none() {
                continue;
            }
            for hook in record
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.hooks.as_ref())
                .into_iter()
                .flatten()
            {
                let mut hook = hook.clone();
                hook.cwd = Some(record.root.clone());
                hook.env = Some(HashMap::from([
                    ("KIMI_CODE_HOME".to_string(), self.kimi_home_dir.clone()),
                    ("KIMI_PLUGIN_ROOT".to_string(), record.root.clone()),
                ]));
                output.push(hook);
            }
        }
        output
    }

    pub async fn enabled_commands(&self) -> Vec<PluginCommandDef> {
        let records = self.records.read().values().cloned().collect::<Vec<_>>();
        let mut output = Vec::new();
        for record in records {
            if !record.enabled || record.state != PluginState::Ok || record.manifest.is_none() {
                continue;
            }
            for entry in record
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.commands.as_ref())
                .into_iter()
                .flatten()
            {
                if let Some(definition) = load_plugin_command(LoadPluginCommandInput {
                    command_path: entry.path.clone(),
                    plugin_id: record.id.clone(),
                    fallback_name: Some(entry.name.clone()),
                })
                .await
                {
                    output.push(definition);
                }
            }
        }
        output
    }

    pub fn plugin_skill_roots(&self) -> Vec<SkillRoot> {
        let mut roots = Vec::new();
        for record in self.records.read().values() {
            if !record.enabled || record.state != PluginState::Ok || record.manifest.is_none() {
                continue;
            }
            roots.extend(skill_roots(
                &record.id,
                record.manifest.as_ref(),
                record.skill_instructions.clone(),
            ));
        }
        roots
    }

    pub fn plugin_agent_roots(&self) -> Vec<PluginAgentRoot> {
        let mut roots = Vec::new();
        for record in self.records.read().values() {
            if !record.enabled || record.state != PluginState::Ok || record.manifest.is_none() {
                continue;
            }
            for dir in record
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.agents.as_ref())
                .into_iter()
                .flatten()
            {
                roots.push(PluginAgentRoot {
                    path: dir.clone(),
                    source: "plugin".to_string(),
                });
            }
        }
        roots
    }

    pub fn enabled_session_starts(&self) -> Vec<EnabledPluginSessionStart> {
        self.records
            .read()
            .values()
            .filter(|record| record.enabled && record.state == PluginState::Ok)
            .filter_map(|record| {
                Some(EnabledPluginSessionStart {
                    plugin_id: record.id.clone(),
                    skill_name: record
                        .manifest
                        .as_ref()?
                        .session_start
                        .as_ref()?
                        .skill
                        .clone(),
                })
            })
            .collect()
    }

    pub fn enabled_system_prompts(&self) -> Vec<EnabledPluginSystemPrompt> {
        self.records
            .read()
            .values()
            .filter(|record| record.enabled && record.state == PluginState::Ok)
            .filter_map(|record| {
                Some(EnabledPluginSystemPrompt {
                    plugin_id: record.id.clone(),
                    content: record.manifest.as_ref()?.system_prompt.clone()?,
                })
            })
            .collect()
    }

    pub fn enabled_mcp_servers(&self) -> HashMap<String, McpServerConfig> {
        let mut output = HashMap::new();
        for record in self.records.read().values() {
            if !record.enabled || record.state != PluginState::Ok || record.manifest.is_none() {
                continue;
            }
            for (name, config) in record
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.mcp_servers.as_ref())
                .into_iter()
                .flatten()
            {
                if !is_mcp_server_enabled(record, name, config) {
                    continue;
                }
                output.insert(
                    plugin_mcp_runtime_name(&record.id, name),
                    with_plugin_mcp_runtime(
                        with_mcp_server_enabled(config, true),
                        &record.root,
                        &self.kimi_home_dir,
                        self.plugin_node_runner.as_ref(),
                    ),
                );
            }
        }
        output
    }

    pub fn mcp_server_entries(&self) -> Vec<PluginMcpServerEntry> {
        let mut output = Vec::new();
        for record in self.records.read().values() {
            if record.state != PluginState::Ok || record.manifest.is_none() {
                continue;
            }
            for (name, config) in record
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.mcp_servers.as_ref())
                .into_iter()
                .flatten()
            {
                let enabled = record.enabled && is_mcp_server_enabled(record, name, config);
                output.push(PluginMcpServerEntry {
                    name: plugin_mcp_runtime_name(&record.id, name),
                    config: with_plugin_mcp_runtime(
                        with_mcp_server_enabled(config, enabled),
                        &record.root,
                        &self.kimi_home_dir,
                        self.plugin_node_runner.as_ref(),
                    ),
                    plugin_id: record.id.clone(),
                    server_name: name.clone(),
                });
            }
        }
        output
    }

    pub fn summaries(&self) -> Vec<PluginSummary> {
        self.list().iter().map(record_to_summary).collect()
    }

    pub fn info(&self, id: &str, locale: Option<&str>) -> Option<PluginInfo> {
        self.get(id)
            .map(|record| record_to_info(&record, locale))
    }

    async fn persist(&self, records: &IndexMap<String, PluginRecord>) -> Result<(), WireAsyncError> {
        let installed = records
            .values()
            .map(|record| InstalledRecord {
                id: record.id.clone(),
                root: record.root.clone(),
                source: record.source,
                origin: Some(record.origin),
                catalog_id: record.catalog_id.clone(),
                marketplace_source: record.marketplace_source.clone(),
                enabled: record.enabled,
                installed_at: record.installed_at.clone(),
                updated_at: record.updated_at.clone(),
                original_source: record.original_source.clone(),
                capabilities: record.capabilities.clone(),
                github: record.github.clone(),
            })
            .collect();
        write_installed(
            &self.kimi_home_dir,
            &InstalledFile {
                version: 1,
                plugins: installed,
            },
        )
        .await
    }

    async fn materialize(&self, entry: InstalledRecord) -> Result<PluginRecord, WireAsyncError> {
        let parsed = parse_manifest(&entry.root).await;
        record_from(RecordFromInput {
            id: entry.id,
            root: entry.root.clone(),
            enabled: entry.enabled,
            installed_at: entry.installed_at,
            updated_at: entry.updated_at,
            original_source: entry.original_source,
            origin: entry.origin,
            catalog_id: entry.catalog_id,
            marketplace_source: entry.marketplace_source,
            capabilities: entry.capabilities,
            github: entry.github,
            source: Some(entry.source),
            parsed,
            discover_skills: self.discover_skills.clone(),
        })
        .await
    }
}

async fn installed_github_sha(
    owner: &str,
    repo: &str,
    ref_: &PluginGithubRef,
) -> Result<Option<String>, WireAsyncError> {
    if ref_.kind == PluginGithubRefKind::Sha && ref_.value.len() == 40 {
        return Ok(Some(ref_.value.to_lowercase()));
    }
    resolve_github_commit_sha(owner, repo, &ref_.value)
        .await
        .map(Some)
}

fn failed_update_status(record: &PluginRecord, error: &(dyn std::error::Error + 'static)) -> PluginUpdateStatus {
    let current = record.github.as_ref().unwrap().ref_.clone();
    PluginUpdateStatus {
        id: record.id.clone(),
        source: PluginSource::Github,
        current: Some(current.clone()),
        latest: current.clone(),
        display_version: current.value,
        update_available: false,
        check_failed: Some(true),
        error_message: Some(to_error_message(error, false)),
    }
}

async fn check_github_update(record: &PluginRecord) -> Result<PluginUpdateStatus, WireAsyncError> {
    let github = record.github.as_ref().ok_or_else(|| {
        Box::new(BugIndicatingError::new(format!(
            "Plugin \"{}\" has no GitHub metadata",
            record.id
        ))) as WireAsyncError
    })?;
    let current = github.ref_.clone();
    let pinned = explicit_github_ref(record);
    if pinned.as_ref().is_some_and(|pinned| {
        matches!(pinned.kind, PluginGithubRefKind::Tag | PluginGithubRefKind::Sha)
    }) {
        return Ok(PluginUpdateStatus {
            id: record.id.clone(),
            source: PluginSource::Github,
            current: Some(current.clone()),
            latest: current.clone(),
            display_version: current.value,
            update_available: false,
            check_failed: None,
            error_message: None,
        });
    }
    if let Some(pinned) = pinned.filter(|pinned| pinned.kind == PluginGithubRefKind::Branch) {
        let latest_sha = resolve_github_commit_sha(&github.owner, &github.repo, &pinned.value).await?;
        return Ok(PluginUpdateStatus {
            id: record.id.clone(),
            source: PluginSource::Github,
            current: Some(current.clone()),
            latest: current,
            display_version: latest_sha[..12].to_string(),
            update_available: github.installed_sha.as_deref() != Some(latest_sha.as_str()),
            check_failed: None,
            error_message: None,
        });
    }
    let latest = resolve_github_source(GithubSourceInput {
        kind: GithubSourceKind::Github,
        owner: github.owner.clone(),
        repo: github.repo.clone(),
        ref_: None,
    })
    .await?;
    let mut update_available = current.kind != latest.ref_.kind || current.value != latest.ref_.value;
    if !update_available
        && matches!(latest.ref_.kind, PluginGithubRefKind::Branch | PluginGithubRefKind::Tag)
    {
        let latest_sha = resolve_github_commit_sha(&github.owner, &github.repo, &latest.ref_.value).await?;
        update_available = github.installed_sha.as_deref() != Some(latest_sha.as_str());
    }
    Ok(PluginUpdateStatus {
        id: record.id.clone(),
        source: PluginSource::Github,
        current: Some(current),
        latest: latest.ref_,
        display_version: latest.display_version,
        update_available,
        check_failed: None,
        error_message: None,
    })
}

fn explicit_github_ref(record: &PluginRecord) -> Option<PluginGithubRef> {
    let fallback = record.github.as_ref().and_then(|github| {
        (github.ref_.kind == PluginGithubRefKind::Sha
            || (github.ref_.kind == PluginGithubRefKind::Branch && github.ref_.value != "HEAD"))
            .then(|| github.ref_.clone())
    });
    let Some(original_source) = &record.original_source else {
        return fallback;
    };
    match resolve_install_source(original_source) {
        Ok(ResolvedSource::Github { ref_, .. }) => ref_.map(|ref_| PluginGithubRef {
            kind: match ref_.kind {
                GithubRefKind::Branch => PluginGithubRefKind::Branch,
                GithubRefKind::Tag => PluginGithubRefKind::Tag,
                GithubRefKind::Sha => PluginGithubRefKind::Sha,
            },
            value: ref_.value,
        }),
        Ok(_) => fallback,
        Err(_) => fallback,
    }
}

fn plugin_not_found(id: &str) -> WireAsyncError {
    let mut details = Map::new();
    details.insert("id".to_string(), Value::String(id.to_string()));
    Box::new(
        Error2::new(
            PLUGIN_NOT_FOUND,
            format!("Plugin \"{id}\" is not installed"),
        )
        .with_details(details),
    )
}

async fn assert_managed_plugin_root(root: &str, kimi_home_dir: &str) -> Result<(), WireAsyncError> {
    let managed_dir = Path::new(kimi_home_dir).join("plugins").join("managed");
    let root_real = match tokio::fs::canonicalize(root).await {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Box::new(error)),
    };
    let managed_real = tokio::fs::canonicalize(&managed_dir)
        .await
        .unwrap_or(managed_dir);
    if root_real == managed_real || !root_real.starts_with(&managed_real) {
        return Err(Box::new(BugIndicatingError::new(format!(
            "Refusing to remove plugin root outside the managed directory: {}",
            root_real.to_string_lossy()
        ))));
    }
    Ok(())
}

async fn normalize_install_root(root_path: &str) -> Result<String, WireAsyncError> {
    let trimmed = root_path.trim();
    if !Path::new(trimmed).is_absolute() {
        let mut details = Map::new();
        details.insert("path".to_string(), Value::String(root_path.to_string()));
        return Err(Box::new(
            Error2::new(
                VALIDATION_FAILED,
                format!("Plugin root must be an absolute path (got \"{root_path}\")"),
            )
            .with_details(details),
        ));
    }
    let resolved = match tokio::fs::canonicalize(trimmed).await {
        Ok(resolved) => resolved,
        Err(error) => {
            let mut details = Map::new();
            details.insert("path".to_string(), Value::String(trimmed.to_string()));
            return Err(Box::new(
                Error2::new(FS_PATH_NOT_FOUND, format!("Plugin root does not exist: {trimmed}"))
                    .with_details(details)
                    .with_cause(Box::new(error)),
            ));
        }
    };
    if !tokio::fs::metadata(&resolved).await?.is_dir() {
        let mut details = Map::new();
        details.insert("path".to_string(), Value::String(trimmed.to_string()));
        return Err(Box::new(
            Error2::new(
                VALIDATION_FAILED,
                format!("Plugin root is not a directory: {trimmed}"),
            )
            .with_details(details),
        ));
    }
    Ok(resolved.to_string_lossy().into_owned())
}

async fn copy_plugin_to_managed_root(
    kimi_home_dir: &str,
    id: &str,
    source_root: &str,
) -> Result<ManagedPluginCopy, WireAsyncError> {
    let managed_root = Path::new(kimi_home_dir)
        .join("plugins")
        .join("managed")
        .join(id);
    let managed_dir = managed_root.parent().unwrap();
    tokio::fs::create_dir_all(managed_dir).await?;
    let staging_root = make_temp_dir(managed_dir, &format!("{id}-")).await?;
    let previous_root = PathBuf::from(format!("{}-previous", staging_root.to_string_lossy()));
    let mut moved_previous_root = false;
    let mut published = false;
    let result = async {
        copy_recursive(PathBuf::from(source_root), staging_root.clone()).await?;
        match tokio::fs::rename(&managed_root, &previous_root).await {
            Ok(()) => moved_previous_root = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Box::new(error) as WireAsyncError),
        }
        tokio::fs::rename(&staging_root, &managed_root).await?;
        published = true;
        let root = tokio::fs::canonicalize(&managed_root).await?;
        Ok::<PathBuf, WireAsyncError>(root)
    }
    .await;
    match result {
        Ok(root) => Ok(ManagedPluginCopy {
            root: root.to_string_lossy().into_owned(),
            previous_root: moved_previous_root.then(|| previous_root.to_string_lossy().into_owned()),
        }),
        Err(error) => {
            let cleanup = if published { &managed_root } else { &staging_root };
            let _ = tokio::fs::remove_dir_all(cleanup).await;
            if moved_previous_root {
                tokio::fs::rename(&previous_root, &managed_root).await?;
            }
            Err(error)
        }
    }
}

async fn rollback_managed_plugin_copy(copy: &ManagedPluginCopy) -> Result<(), WireAsyncError> {
    match tokio::fs::remove_dir_all(&copy.root).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Box::new(error)),
    }
    if let Some(previous_root) = &copy.previous_root {
        tokio::fs::rename(previous_root, &copy.root).await?;
    }
    Ok(())
}

struct RecordFromInput {
    id: String,
    root: String,
    enabled: bool,
    installed_at: String,
    updated_at: Option<String>,
    original_source: Option<String>,
    origin: Option<PluginOrigin>,
    catalog_id: Option<String>,
    marketplace_source: Option<String>,
    capabilities: Option<PluginCapabilityState>,
    github: Option<PluginGithubMetadata>,
    source: Option<PluginSource>,
    parsed: ParsedManifestResult,
    discover_skills: DiscoverSkills,
}

async fn record_from(input: RecordFromInput) -> Result<PluginRecord, WireAsyncError> {
    let parsed = input.parsed;
    let has_error = parsed
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == PluginDiagnosticSeverity::Error);
    let skill_count = count_discovered_plugin_skills(
        &input.id,
        parsed.manifest.as_ref(),
        input.discover_skills,
    )
    .await?;
    let skill_instructions = parsed
        .manifest
        .as_ref()
        .and_then(|manifest| manifest.skill_instructions.clone());
    Ok(PluginRecord {
        id: input.id,
        root: input.root,
        source: input.source.unwrap_or(PluginSource::LocalPath),
        origin: input.origin.unwrap_or(PluginOrigin::Unknown),
        catalog_id: input.catalog_id,
        marketplace_source: input.marketplace_source,
        enabled: input.enabled,
        state: if has_error || parsed.manifest.is_none() {
            PluginState::Error
        } else {
            PluginState::Ok
        },
        installed_at: input.installed_at,
        updated_at: input.updated_at,
        original_source: input.original_source,
        capabilities: input.capabilities,
        github: input.github,
        skill_instructions,
        skill_count,
        manifest: parsed.manifest,
        interface_locales: parsed.interface_locales,
        manifest_kind: parsed.manifest_kind,
        manifest_path: parsed.manifest_path,
        shadowed_manifest_path: parsed.shadowed_manifest_path,
        diagnostics: parsed.diagnostics,
    })
}

fn record_to_summary(record: &PluginRecord) -> PluginSummary {
    let interface = record.manifest.as_ref().and_then(|manifest| manifest.interface.as_ref());
    PluginSummary {
        id: record.id.clone(),
        display_name: interface
            .and_then(|interface| interface.display_name.clone())
            .unwrap_or_else(|| record.id.clone()),
        description: record
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.description.clone()),
        short_description: interface.and_then(|interface| interface.short_description.clone()),
        long_description: interface.and_then(|interface| interface.long_description.clone()),
        version: record.manifest.as_ref().and_then(|manifest| manifest.version.clone()),
        enabled: record.enabled,
        state: record.state,
        skill_count: record.skill_count,
        mcp_server_count: record
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.mcp_servers.as_ref())
            .map_or(0, HashMap::len),
        enabled_mcp_server_count: plugin_mcp_servers_info(record, None)
            .iter()
            .filter(|server| server.enabled)
            .count(),
        hook_count: record
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.hooks.as_ref())
            .map_or(0, Vec::len),
        command_count: record
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.commands.as_ref())
            .map_or(0, Vec::len),
        has_errors: record
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == PluginDiagnosticSeverity::Error),
        source: record.source,
        origin: record.origin,
        catalog_id: record.catalog_id.clone(),
        marketplace_source: record.marketplace_source.clone(),
        original_source: record.original_source.clone(),
        github: record.github.clone(),
    }
}

fn record_to_info(record: &PluginRecord, locale: Option<&str>) -> PluginInfo {
    let summary = record_to_summary(record);
    PluginInfo {
        id: summary.id,
        display_name: summary.display_name,
        description: summary.description,
        short_description: summary.short_description,
        long_description: summary.long_description,
        version: summary.version,
        enabled: summary.enabled,
        state: summary.state,
        skill_count: summary.skill_count,
        mcp_server_count: summary.mcp_server_count,
        enabled_mcp_server_count: summary.enabled_mcp_server_count,
        hook_count: summary.hook_count,
        command_count: summary.command_count,
        has_errors: summary.has_errors,
        source: summary.source,
        origin: summary.origin,
        catalog_id: summary.catalog_id,
        marketplace_source: summary.marketplace_source,
        original_source: summary.original_source,
        github: summary.github,
        root: record.root.clone(),
        installed_at: record.installed_at.clone(),
        updated_at: record.updated_at.clone(),
        manifest_kind: record.manifest_kind,
        manifest_path: record.manifest_path.clone(),
        manifest: record.manifest.clone(),
        mcp_servers: plugin_mcp_servers_info(record, locale),
        shadowed_manifest_path: record.shadowed_manifest_path.clone(),
        diagnostics: record.diagnostics.clone(),
    }
}

fn is_mcp_server_enabled(record: &PluginRecord, name: &str, config: &McpServerConfig) -> bool {
    record
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.mcp_servers.as_ref())
        .and_then(|servers| servers.get(name))
        .map(|state| state.enabled)
        .unwrap_or(config.enabled != Some(false))
}

fn plugin_mcp_servers_info(record: &PluginRecord, locale: Option<&str>) -> Vec<PluginMcpServerInfo> {
    let localized = plugin_interface_for_locale(record, locale);
    let mut output = record
        .manifest
        .as_ref()
        .and_then(|manifest| manifest.mcp_servers.as_ref())
        .into_iter()
        .flatten()
        .map(|(name, config)| plugin_mcp_server_info(record, name, config, localized.as_ref()))
        .collect::<Vec<_>>();
    output.sort_by(|left, right| left.name.cmp(&right.name));
    output
}

fn plugin_interface_for_locale(record: &PluginRecord, locale: Option<&str>) -> Option<PluginInterface> {
    let locale = locale?;
    let entries = record
        .interface_locales
        .as_ref()
        .into_iter()
        .flatten()
        .map(|(key, value)| (normalized_locale(key), value.clone()))
        .collect::<Vec<_>>();
    let normalized = normalized_locale(locale);
    let base = normalized.split('-').next().unwrap_or(&normalized);
    if let Some((_, interface)) = entries.iter().find(|(key, _)| key == &normalized) {
        return Some(interface.clone());
    }
    if base != normalized
        && let Some((_, interface)) = entries.iter().find(|(key, _)| key == base)
    {
        return Some(interface.clone());
    }
    if let Some(default_locale) = record
        .manifest
        .as_ref()
        .and_then(|manifest| manifest.interface.as_ref())
        .and_then(|interface| interface.default_locale.as_deref())
    {
        let normalized_default = normalized_locale(default_locale);
        if normalized_default.split('-').next() == Some(base) {
            return entries
                .iter()
                .find(|(key, _)| key == &normalized_default)
                .map(|(_, interface)| interface.clone());
        }
    }
    let mut matching = entries
        .into_iter()
        .filter(|(key, _)| key.split('-').next() == Some(base))
        .collect::<Vec<_>>();
    matching.sort_by(|(left, _), (right, _)| left.cmp(right));
    matching.into_iter().next().map(|(_, interface)| interface)
}

fn normalized_locale(value: &str) -> String {
    value.replace('_', "-").to_lowercase()
}

fn plugin_mcp_server_info(
    record: &PluginRecord,
    name: &str,
    config: &McpServerConfig,
    localized: Option<&PluginInterface>,
) -> PluginMcpServerInfo {
    let base_override = record
        .manifest
        .as_ref()
        .and_then(|manifest| manifest.interface.as_ref())
        .and_then(|interface| interface.mcp_overrides.as_ref())
        .and_then(|overrides| overrides.get(name));
    let override_ = localized
        .and_then(|interface| interface.mcp_overrides.as_ref())
        .and_then(|overrides| overrides.get(name));
    let display_name = override_
        .and_then(|override_| override_.display_name.clone())
        .or_else(|| base_override.and_then(|override_| override_.display_name.clone()));
    let description = override_
        .and_then(|override_| override_.description.clone())
        .or_else(|| base_override.and_then(|override_| override_.description.clone()));
    let icon_url = override_
        .and_then(|override_| override_.icon_url.clone())
        .or_else(|| base_override.and_then(|override_| override_.icon_url.clone()));
    let mut header_keys = config
        .headers
        .as_ref()
        .map(|headers| headers.keys().cloned().collect::<Vec<_>>());
    if let Some(keys) = &mut header_keys {
        keys.sort();
    }
    let mut env_keys = config
        .env
        .as_ref()
        .map(|env| env.keys().cloned().collect::<Vec<_>>());
    if let Some(keys) = &mut env_keys {
        keys.sort();
    }
    PluginMcpServerInfo {
        name: name.to_string(),
        display_name,
        description,
        icon_url,
        runtime_name: plugin_mcp_runtime_name(&record.id, name),
        enabled: is_mcp_server_enabled(record, name, config),
        transport: config.transport,
        command: (config.transport == McpTransport::Stdio)
            .then(|| config.command.clone())
            .flatten(),
        args: (config.transport == McpTransport::Stdio)
            .then(|| config.args.clone())
            .flatten(),
        cwd: (config.transport == McpTransport::Stdio)
            .then(|| config.cwd.clone())
            .flatten(),
        url: (config.transport != McpTransport::Stdio)
            .then(|| config.url.clone())
            .flatten(),
        env_keys: (config.transport == McpTransport::Stdio)
            .then_some(env_keys)
            .flatten(),
        header_keys: (config.transport != McpTransport::Stdio)
            .then_some(header_keys)
            .flatten(),
    }
}

fn plugin_mcp_runtime_name(plugin_id: &str, server_name: &str) -> String {
    format!("plugin-{plugin_id}:{server_name}")
}

fn with_mcp_server_enabled(config: &McpServerConfig, enabled: bool) -> McpServerConfig {
    McpServerConfig {
        enabled: Some(enabled),
        ..config.clone()
    }
}

fn with_plugin_mcp_runtime(
    config: &McpServerConfig,
    plugin_root: &str,
    kimi_home_dir: &str,
    plugin_node_runner: Option<&PluginNodeRunner>,
) -> McpServerConfig {
    if config.transport != McpTransport::Stdio {
        return config.clone();
    }
    let mut env = config.env.clone().unwrap_or_default();
    env.insert("KIMI_CODE_HOME".to_string(), kimi_home_dir.to_string());
    env.insert("KIMI_PLUGIN_ROOT".to_string(), plugin_root.to_string());
    if config.command.as_deref() == Some("node") && is_electron() {
        env.insert("ELECTRON_RUN_AS_NODE".to_string(), "1".to_string());
        return McpServerConfig {
            command: Some(
                std::env::current_exe()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
            args: Some(config.args.clone().unwrap_or_default()),
            cwd: Some(config.cwd.clone().unwrap_or_else(|| plugin_root.to_string())),
            env: Some(env),
            ..config.clone()
        };
    }
    if config.command.as_deref() == Some("node")
        && let Some(runner) = plugin_node_runner
    {
        let mut args = runner.args.clone();
        args.extend(config.args.clone().unwrap_or_default());
        return McpServerConfig {
            command: Some(runner.command.clone()),
            args: Some(args),
            cwd: Some(config.cwd.clone().unwrap_or_else(|| plugin_root.to_string())),
            env: Some(env),
            ..config.clone()
        };
    }
    McpServerConfig {
        cwd: Some(config.cwd.clone().unwrap_or_else(|| plugin_root.to_string())),
        env: Some(env),
        ..config.clone()
    }
}

fn is_electron() -> bool {
    std::env::var_os("ELECTRON_VERSION").is_some()
        || std::env::var_os("ELECTRON_RUN_AS_NODE").is_some()
}

fn is_kimi_native_binary() -> bool {
    let executable_is_node = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_name().map(|name| name.to_string_lossy().to_lowercase()))
        .is_some_and(|name| name.starts_with("node"));
    if executable_is_node {
        return false;
    }
    std::env::var("__KIMI_BUN_ASSETS__")
        .ok()
        .and_then(|assets| serde_json::from_str::<Value>(&assets).ok())
        .and_then(|assets| assets.as_object().cloned())
        .is_some_and(|assets| !assets.is_empty())
}

async fn count_discovered_plugin_skills(
    plugin_id: &str,
    manifest: Option<&super::types::PluginManifest>,
    discover_skills: DiscoverSkills,
) -> Result<usize, WireAsyncError> {
    let dirs = manifest
        .and_then(|manifest| manifest.skills.as_ref())
        .cloned()
        .unwrap_or_default();
    if dirs.is_empty() {
        return Ok(0);
    }
    let roots = skill_roots(
        plugin_id,
        manifest,
        manifest.and_then(|manifest| manifest.skill_instructions.clone()),
    );
    Ok(discover_skills(roots).await?.skills.len())
}

fn skill_roots(
    plugin_id: &str,
    manifest: Option<&super::types::PluginManifest>,
    instructions: Option<String>,
) -> Vec<SkillRoot> {
    let Some(manifest) = manifest else {
        return Vec::new();
    };
    manifest
        .skills
        .as_ref()
        .into_iter()
        .flatten()
        .map(|dir| SkillRoot {
            path: dir.clone(),
            source: "extra".to_string(),
            workspace_dir: None,
            plugin: Some(SkillPluginContext {
                id: plugin_id.to_string(),
                instructions: instructions.clone(),
            }),
            scan_mode: if manifest.root_skill_fallback == Some(true) {
                Some("root-skill-only".to_string())
            } else {
                None
            },
        })
        .collect()
}

async fn make_temp_dir(parent: &Path, prefix: &str) -> Result<PathBuf, WireAsyncError> {
    tokio::fs::create_dir_all(parent).await?;
    loop {
        let path = parent.join(format!("{prefix}{}", Uuid::new_v4()));
        match tokio::fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Box::new(error)),
        }
    }
}

fn copy_recursive(source: PathBuf, destination: PathBuf) -> BoxFuture<'static, Result<(), WireAsyncError>> {
    Box::pin(async move {
        let metadata = tokio::fs::symlink_metadata(&source).await?;
        if metadata.is_dir() {
            tokio::fs::create_dir_all(&destination).await?;
            let mut entries = tokio::fs::read_dir(&source).await?;
            while let Some(entry) = entries.next_entry().await? {
                copy_recursive(entry.path(), destination.join(entry.file_name())).await?;
            }
        } else if metadata.is_file() {
            if let Some(parent) = destination.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(&source, &destination).await?;
        } else if metadata.file_type().is_symlink() {
            let target = tokio::fs::read_link(&source).await?;
            create_symlink(&source, &target, &destination).await?;
        }
        Ok(())
    })
}

#[cfg(unix)]
async fn create_symlink(_source: &Path, target: &Path, destination: &Path) -> Result<(), WireAsyncError> {
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::symlink(target, destination).await?;
    Ok(())
}

#[cfg(windows)]
async fn create_symlink(source: &Path, target: &Path, destination: &Path) -> Result<(), WireAsyncError> {
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let metadata = tokio::fs::metadata(source).await?;
    if metadata.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination)?;
    } else {
        std::os::windows::fs::symlink_file(target, destination)?;
    }
    Ok(())
}

fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[derive(Debug)]
struct PluginRollbackError {
    original: String,
    rollback: String,
}

impl fmt::Display for PluginRollbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Plugin installation failed and the previous managed copy could not be restored: {}; rollback: {}",
            self.original, self.rollback
        )
    }
}

impl std::error::Error for PluginRollbackError {}
