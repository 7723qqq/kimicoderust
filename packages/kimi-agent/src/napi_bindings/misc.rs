//! Small standalone napi exports: filesystem suggestions, background
//! task inspection, and search-engine selection.

use super::*;
use napi::{JsObject, bindgen_prelude::Env};
use napi_derive::napi;

/// Workspace-root file suggestions for the host's mention picker — the
/// `POST /api/v1/fs::suggest` payload (`{ items, truncated }`) as JSON, so the
/// napi transport and the HTTP server answer from one implementation
/// (`server::fs_routes::search_files`). `limit` defaults to 50.
#[napi]
pub fn fs_suggest(work_dir: String, query: String, limit: Option<u32>) -> napi::Result<String> {
    guard_sync_panic(|| {
        let (items, truncated) = crate::server::fs_routes::search_files(
            std::path::Path::new(&work_dir),
            query.trim(),
            limit.unwrap_or(50) as usize,
        );
        serde_json::to_string(&serde_json::json!({ "items": items, "truncated": truncated }))
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    })
}

/// Every registered background task's entry wire, oldest first, output
/// omitted — the JSON array the standalone server serves from its task
/// runner (`GET /api/v1/tasks`, server/mod.rs). The runner is the one the
/// active engine pipeline attached to the process-wide subagent manager; an
/// empty array when no pipeline is live.
#[napi]
pub fn background_task_list() -> napi::Result<String> {
    guard_sync_panic(|| {
        let tasks = SUBAGENT_MANAGER
            .get_task_runner_sync()
            .map(|runner| runner.list())
            .unwrap_or_default();
        serde_json::to_string(&tasks).map_err(|e| napi::Error::from_reason(e.to_string()))
    })
}

/// One background task's output snapshot; null while the task is still
/// running (or unknown) — the [`TaskRunner::get_output`] contract.
#[napi]
pub fn background_task_output(id: String) -> napi::Result<Option<String>> {
    guard_sync_panic(|| {
        Ok(SUBAGENT_MANAGER
            .get_task_runner_sync()
            .and_then(|runner| runner.get_output(&id)))
    })
}

/// Request a cooperative stop for one background task and resolve with its
/// entry wire (`killed` once settled) — the `POST /api/v1/tasks/:id/stop`
/// semantics. The optional `reason` becomes the entry's `stopReason` (blank
/// falls back to `"Stopped by TaskStop"`). Fails for an unknown id or a
/// process without a live runner.
#[napi]
pub fn background_task_stop(
    env: Env,
    id: String,
    reason: Option<String>,
) -> napi::Result<JsObject> {
    guard_sync_panic(move || {
        let runner = SUBAGENT_MANAGER.get_task_runner_sync().ok_or_else(|| {
            napi::Error::from_reason("no background task runner is active in this process")
        })?;
        env.execute_tokio_future(
            async move {
                runner
                    .stop(&id, reason.as_deref())
                    .await
                    .map(|outcome| outcome.wire)
                    .map_err(napi::Error::from_reason)
            },
            |env, wire: serde_json::Value| {
                env.create_string_from_std(
                    serde_json::to_string(&wire).unwrap_or_else(|e| e.to_string()),
                )
            },
        )
    })
}

/// Switch the active web search engine at runtime (manual hot-switch from the
/// host). `engine` is `"bing"` or `"ddg"`. Returns the active engine name.
#[napi]
pub fn set_search_engine(engine: String) -> napi::Result<String> {
    guard_sync_panic(|| {
        let engine = crate::tools::web_search::SearchEngine::from_name(&engine)
            .ok_or_else(|| napi::Error::from_reason("unknown search engine"))?;
        crate::tools::web_search::set_active_engine(engine);
        Ok(engine.name().to_string())
    })
}

/// The effective workspace trust state (upstream #4517): the engine combines
/// `[workspace] require_trust` from `<homeDir>/config.toml`, the
/// `KIMI_CODE_TRUST_WORKSPACE` process override and the persisted
/// `trusted-workspaces.json` record, re-reading all three on every call (v2
/// `WorkspaceTrustEntry.refresh`, which the MCP registry consults on every
/// cwd-scoped listing). The host's trust prompt and the project-config gates
/// (`.mcp.json`, `workspace.additional_dir`) key off this answer.
#[napi]
pub fn workspace_trust_state(home_dir: String, work_dir: String) -> napi::Result<bool> {
    guard_sync_panic(|| {
        Ok(crate::workspace_trust::effective_trust(
            std::path::Path::new(&home_dir),
            &work_dir,
        ))
    })
}

/// Persist a workspace trust decision (v2 `WorkspaceTrustEntry.trust`):
/// append the root to `trusted-workspaces.json` unless a matching key is
/// already recorded. The write happens even while `[workspace] require_trust`
/// is `false`, so a re-enabled gate finds the records intact.
#[napi]
pub fn workspace_trust_record(home_dir: String, work_dir: String) -> napi::Result<()> {
    guard_sync_panic(|| {
        crate::workspace_trust::record_trust(std::path::Path::new(&home_dir), &work_dir)
            .map_err(napi::Error::from_reason)
    })
}

/// What trusting `work_dir` would switch on (v2 `trustDisclosureService`,
/// upstream #4056): project MCP servers, the extra directories those would
/// widen the sandbox to, the instruction files the session would start
/// reading, and one warning line per inspection that could not complete.
///
/// JSON rather than a napi object: the shape is nested and optional-heavy,
/// and the host already parses JSON from this boundary. `explicitSkillDirs`
/// mirrors `--skill-dirs` / `KIMI_CODE_SKILL_DIRS` — when the user named the
/// roots, the project scan is not what a trust decision activates.
#[napi]
pub fn workspace_trust_disclosure(
    work_dir: String,
    explicit_skill_dirs: Option<bool>,
    merge_all_available_skills: Option<bool>,
    pending_additional_dirs: Option<Vec<String>>,
) -> napi::Result<String> {
    guard_sync_panic(|| {
        let activation = crate::workspace_trust_disclosure::describe_gated_activation(
            &crate::workspace_trust_disclosure::DisclosureInput {
                work_dir: std::path::Path::new(&work_dir),
                explicit_skill_dirs: explicit_skill_dirs.unwrap_or(false),
                merge_all_available_skills: merge_all_available_skills.unwrap_or(true),
                pending_additional_dirs: pending_additional_dirs
                    .unwrap_or_default()
                    .into_iter()
                    .map(std::path::PathBuf::from)
                    .collect(),
            },
        );
        serde_json::to_string(&activation).map_err(|e| napi::Error::from_reason(e.to_string()))
    })
}
