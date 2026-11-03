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
