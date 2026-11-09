//! Plugin manager registry and the `plugin_*` napi exports.

use super::*;
use napi_derive::napi;
use std::sync::{Arc, LazyLock, Mutex};

/// The plugin registry lives in the same SQLite store the standalone server
/// uses (`<data_dir>/sessions.db`), so the CLI and `kimi web` read one install
/// state. The host calls [`init_plugin_store`] once with its data dir; every
/// plugin export below answers from the manager it installs.
static PLUGIN_MANAGER: LazyLock<Mutex<Option<Arc<crate::server::plugins::PluginManager>>>> =
    LazyLock::new(|| Mutex::new(None));

/// Open the plugin registry against `<data_dir>/sessions.db`. `marketplace_dir`
/// is the directory holding `marketplace.json` (the host resolves it), so a
/// relative catalog `source` resolves to a real plugin root. `node_runner` is
/// the host executable, recorded so plugin stdio servers declared with
/// `command: "node"` are re-executed inside the host runtime
/// (`<runner> __plugin_run_node <entry>`) instead of requiring a system
/// Node.js; pass `null` to keep spawning `node` as declared. Idempotent: a
/// second call replaces the manager, which is harmless because the state lives
/// in the file, not in the manager.
#[napi]
pub fn init_plugin_store(
    data_dir: String,
    marketplace_dir: Option<String>,
    node_runner: Option<String>,
) -> napi::Result<()> {
    guard_sync_panic(|| {
        let db_path = std::path::Path::new(&data_dir).join("sessions.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        }
        let store = crate::session::sqlite_store::SqliteSessionStore::open(&db_path)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let manager = crate::server::plugins::PluginManager::new(Arc::new(store))
            .with_marketplace_dir(marketplace_dir.map(std::path::PathBuf::from))
            .with_home_dir(Some(std::path::PathBuf::from(&data_dir)))
            .with_node_runner(
                node_runner
                    .filter(|runner| !runner.trim().is_empty())
                    .map(std::path::PathBuf::from),
            );
        *PLUGIN_MANAGER.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(manager));
        Ok(())
    })
}

/// The enabled plugins' prompt block and hooks when the host has opened the
/// plugin store; an unopened store contributes nothing (the headless path must
/// not force the registry open).
pub(crate) fn plugin_contributions() -> (Option<String>, Vec<crate::permission::HookDef>) {
    let manager = PLUGIN_MANAGER
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    match manager {
        Some(manager) => (
            manager.plugin_system_prompt_block(),
            manager.enabled_hooks(),
        ),
        None => (None, Vec::new()),
    }
}

fn plugin_manager() -> napi::Result<Arc<crate::server::plugins::PluginManager>> {
    PLUGIN_MANAGER
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .ok_or_else(|| {
            napi::Error::from_reason(
                "plugin store not initialized; call initPluginStore(dataDir) first",
            )
        })
}

/// The enabled plugins' skill roots, read from the process-wide registry.
/// Empty when the host never called [`init_plugin_store`], so a process without
/// plugins scans exactly what it did before.
pub(crate) fn plugin_skill_dirs() -> Vec<crate::skills::PluginSkillDir> {
    PLUGIN_MANAGER
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|manager| manager.plugin_skill_dirs())
        .unwrap_or_default()
}

/// Resolve the system prompt for a napi-owned session.
///
/// The host may pass a prompt of its own (`[models.<alias>].systemPrompt` is
/// plumbed through `native_llm`), and that always wins. When it does not — an
/// empty string, the `"sys"` sentinel the ACP path uses, or the one-line stub
/// the native SDK used to send — build the engine's real prompt from the
/// session workspace. `ServerEngine::session_spec` answers the same three cases
/// for the server; keeping the logic here means every napi caller gets it
/// without each host having to know about `prompt/system.md`.
pub(crate) fn build_session_system_prompt(params: &JsRunTurnParams) -> String {
    const STANDALONE_SERVICE_SENTINEL: &str =
        "You are kimi-agent, running as a standalone service.";
    if let Some(provider) = params
        .providers
        .as_ref()
        .and_then(|providers| providers.first())
        && !provider.system_prompt.trim().is_empty()
    {
        return provider.system_prompt.clone();
    }
    let supplied = params.system_prompt.trim();
    let is_placeholder = supplied.is_empty()
        || supplied == "sys"
        || supplied.starts_with(STANDALONE_SERVICE_SENTINEL)
        || supplied.starts_with("You are Kimi Code, an intelligent AI coding assistant");
    if !is_placeholder {
        return params.system_prompt.clone();
    }
    let Some(root) = params.workspace_root.as_deref().filter(|r| !r.is_empty()) else {
        // No workspace to describe: keep whatever the host sent rather than
        // fabricating an environment section for an unknown root.
        return params.system_prompt.clone();
    };
    let plugin_dirs = plugin_skill_dirs();
    // The same roots the pipeline spec carries (`with_skill_scan`: empty
    // `skill_dirs` plus these plugin roots), so the prompt's skills section
    // and the `Skill` tool's scan cannot disagree — plugin skills render
    // under their qualified `<plugin_id>:<skill>` names in both.
    let merge_all_available_skills = crate::config::resolved_merge_all_available_skills();
    let builder = crate::prompt::SystemPromptBuilder::new(root)
        .with_plugin_skill_dirs(plugin_dirs)
        .with_merge_all_available_skills(merge_all_available_skills)
        .with_memory(true);
    match params.agent_profile.as_deref().map(str::trim) {
        Some(profile) if !profile.is_empty() => builder.with_profile(profile).build(),
        _ => builder.build(),
    }
}

/// The enabled plugins' MCP servers, read from the process-wide registry.
pub(crate) fn plugin_mcp_configs() -> Vec<crate::server::plugins::PluginMcpConfig> {
    PLUGIN_MANAGER
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|manager| manager.plugin_mcp_configs())
        .unwrap_or_default()
}

/// Drop the plugin registry and close its SQLite connection. The host calls
/// this on shutdown: Windows keeps a lock on an open database, which blocks
/// removing the data directory.
#[napi]
pub fn close_plugin_store() -> napi::Result<()> {
    guard_sync_panic(|| {
        *PLUGIN_MANAGER.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(())
    })
}

/// Every installed plugin as a JSON array of `PluginSummary` wires
/// (`id` / `name` / `version` / `enabled` / `description` / `source`).
#[napi]
pub fn plugin_list() -> napi::Result<String> {
    guard_sync_panic(|| {
        let plugins = plugin_manager()?.list_plugins();
        serde_json::to_string(&plugins).map_err(|e| napi::Error::from_reason(e.to_string()))
    })
}

/// Install a plugin from a catalog id, a catalog `source`, or a remote archive
/// URL, answering its `PluginSummary` wire as JSON — or `null` for a source the
/// catalog does not know, so the caller can report "unknown plugin" instead of
/// inventing an install record. A remote source is downloaded and extracted
/// into `<dataDir>/plugins/<id>` first.
#[napi]
pub fn plugin_install(id: String) -> napi::Result<Option<String>> {
    guard_sync_panic(|| {
        let manager = plugin_manager()?;
        let installed = manager
            .install_plugin_from(&id)
            .map_err(napi::Error::from_reason)?;
        let Some((id, _)) = installed else {
            return Ok(None);
        };
        // v2 `AgentPluginService`: every live session learns the running
        // session keeps its prompt and tools until /new or /reload.
        crate::injection::plugin::publish_plugin_change_notice("install", &id);
        manager
            .list_plugins()
            .into_iter()
            .find(|plugin| plugin.id == id)
            .map(|summary| {
                serde_json::to_string(&summary).map_err(|e| napi::Error::from_reason(e.to_string()))
            })
            .transpose()
    })
}

/// Enable or disable an installed plugin. `false` means the id is neither
/// installed nor catalogued.
#[napi]
pub fn plugin_set_enabled(id: String, enabled: bool) -> napi::Result<bool> {
    guard_sync_panic(|| {
        let changed = plugin_manager()?
            .set_plugin_enabled(&id, enabled)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        if changed {
            crate::injection::plugin::publish_plugin_change_notice(
                if enabled { "enable" } else { "disable" },
                &id,
            );
        }
        Ok(changed)
    })
}

/// Remove an installed plugin. `false` means it was not installed.
#[napi]
pub fn plugin_remove(id: String) -> napi::Result<bool> {
    guard_sync_panic(|| {
        let removed = plugin_manager()?
            .remove_plugin(&id)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        if removed {
            crate::injection::plugin::publish_plugin_change_notice("remove", &id);
        }
        Ok(removed)
    })
}

/// Full detail for one installed plugin as a JSON `PluginInfo` wire — the
/// catalog entry, the install state, and everything the manifest contributes
/// (commands, MCP servers, skill/hook counts). `null` when the id is not
/// installed.
#[napi]
pub fn plugin_info(id: String) -> napi::Result<Option<String>> {
    guard_sync_panic(|| {
        plugin_manager()?
            .plugin_info(&id)
            .map(|info| {
                serde_json::to_string(&info).map_err(|e| napi::Error::from_reason(e.to_string()))
            })
            .transpose()
    })
}

/// Every command the enabled plugins contribute, as a JSON array of
/// `PluginCommandDef` wires (`pluginId` / `name` / `description` / `body` /
/// `path`), in plugin-id order.
#[napi]
pub fn plugin_commands() -> napi::Result<String> {
    guard_sync_panic(|| {
        let commands = plugin_manager()?.enabled_commands();
        serde_json::to_string(&commands).map_err(|e| napi::Error::from_reason(e.to_string()))
    })
}

/// Enable or disable one MCP server a plugin declares. `false` means the
/// plugin does not declare a server by that name.
#[napi]
pub fn plugin_set_mcp_server_enabled(
    id: String,
    server: String,
    enabled: bool,
) -> napi::Result<bool> {
    guard_sync_panic(|| {
        let changed = plugin_manager()?
            .set_mcp_server_enabled(&id, &server, enabled)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        if changed {
            crate::injection::plugin::publish_plugin_change_notice("mcp-server", &id);
        }
        Ok(changed)
    })
}

/// Re-read the catalog and every installed manifest, answering a JSON
/// `ReloadSummary` (`{ added, removed, errors }`).
#[napi]
pub fn plugin_reload() -> napi::Result<String> {
    guard_sync_panic(|| {
        let summary = plugin_manager()?.reload();
        serde_json::to_string(&summary).map_err(|e| napi::Error::from_reason(e.to_string()))
    })
}
