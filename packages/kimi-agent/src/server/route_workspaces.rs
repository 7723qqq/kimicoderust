use super::*;

impl HttpServer {
    pub(super) async fn route_workspaces(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("POST", "/api/v1/workspaces") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let root = match body.get("root").and_then(|v| v.as_str()) {
                    Some(r) if !r.trim().is_empty() => r.trim(),
                    _ => return HttpResponse::bad_request("Field 'root' is required"),
                };
                let name = body.get("name").and_then(|v| v.as_str());
                match self.store.create_workspace(root, name) {
                    Ok(ws) => {
                        let ws_json = json!(ws);
                        self.hub
                            .bus_for("global")
                            .publish(&crate::events::EngineEvent::Custom(json!({
                                "type": "event.workspace.created",
                                "workspace": ws_json,
                            })));
                        HttpResponse::json(201, &ws_json)
                    }
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("GET", p) if p.starts_with("/api/v1/workspaces/") && p.ends_with("/trust") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 6 {
                    return HttpResponse::not_found();
                }
                let workspace_id = segments[4];
                if self
                    .store
                    .get_workspace(workspace_id)
                    .ok()
                    .flatten()
                    .is_none()
                {
                    return HttpResponse::not_found();
                }
                match self.store.is_workspace_trusted(workspace_id) {
                    Ok(persisted) => HttpResponse::ok(&json!({
                        "trusted": self.effective_workspace_trust(persisted).await,
                    })),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("POST", p) if p.starts_with("/api/v1/workspaces/") && p.ends_with("/trust") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 6 {
                    return HttpResponse::not_found();
                }
                let workspace_id = segments[4];
                if self
                    .store
                    .get_workspace(workspace_id)
                    .ok()
                    .flatten()
                    .is_none()
                {
                    return HttpResponse::not_found();
                }
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                // Fail-closed: a body without an explicit `trusted` boolean is
                // not a grant; only `{"trusted": true}` records trust.
                let trusted = body
                    .get("trusted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                match self.store.set_workspace_trusted(workspace_id, trusted) {
                    // Upstream #4517 (kap-server `/untrust`): the answer is the
                    // *effective* state after the write, which stays `true`
                    // while a process or config override applies.
                    Ok(_) => HttpResponse::ok(&json!({
                        "trusted": self.effective_workspace_trust(trusted).await,
                    })),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("GET", p) if p.starts_with("/api/v1/workspaces/") && p.ends_with("/skills") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 6 {
                    return HttpResponse::not_found();
                }
                let workspace_id = segments[4];
                let ws = match self.store.get_workspace(workspace_id) {
                    Ok(Some(w)) => w,
                    Ok(None) => return HttpResponse::not_found(),
                    Err(e) => return HttpResponse::internal_error(format!("Database error: {e}")),
                };
                let root_path = std::path::PathBuf::from(&ws.root);
                let config = self.config().await;
                let merge = config.resolve_merge_all_available_skills();
                let extra = config.extra_skill_dirs_paths();
                let plugin_dirs = self.plugin_manager.plugin_skill_dirs();
                let skills = crate::skills::scan_all_skills_with_extra_merge_and_plugins(
                    Some(&root_path),
                    &extra,
                    &plugin_dirs,
                    merge,
                );
                HttpResponse::ok(&json!({ "skills": skills }))
            }
            ("GET", p) if p.starts_with("/api/v1/workspaces/") && p.ends_with("/plugins") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 6 {
                    return HttpResponse::not_found();
                }
                let workspace_id = segments[4];
                if self
                    .store
                    .get_workspace(workspace_id)
                    .ok()
                    .flatten()
                    .is_none()
                {
                    return HttpResponse::not_found();
                }
                let plugins = self.plugin_manager.list_plugins();
                HttpResponse::ok(&json!({ "plugins": plugins }))
            }
            ("GET", p)
                if p.starts_with("/api/v1/workspaces/")
                    && !p.ends_with("/trust")
                    && !p.ends_with("/skills")
                    && !p.ends_with("/plugins") =>
            {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 5 {
                    return HttpResponse::not_found();
                }
                let workspace_id = segments[4];
                match self.store.get_workspace(workspace_id) {
                    Ok(Some(ws)) => HttpResponse::ok(&json!(ws)),
                    Ok(None) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("PATCH", p) if p.starts_with("/api/v1/workspaces/") && !p.ends_with("/trust") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 5 {
                    return HttpResponse::not_found();
                }
                let workspace_id = segments[4];
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let Some(name) = body.get("name").and_then(|v| v.as_str()) else {
                    return HttpResponse::bad_request("Field 'name' is required");
                };
                match self.store.update_workspace_name(workspace_id, name) {
                    Ok(Some(ws)) => {
                        self.hub
                            .bus_for("global")
                            .publish(&crate::events::EngineEvent::Custom(json!({
                                "type": "event.workspace.updated",
                                "workspace": ws,
                            })));
                        HttpResponse::ok(&json!(ws))
                    }
                    Ok(None) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("DELETE", p) if p.starts_with("/api/v1/workspaces/") && !p.ends_with("/trust") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 5 {
                    return HttpResponse::not_found();
                }
                let workspace_id = segments[4];
                let root = self
                    .store
                    .get_workspace(workspace_id)
                    .ok()
                    .flatten()
                    .map(|w| w.root);
                match self.store.delete_workspace(workspace_id) {
                    Ok(true) => {
                        self.hub
                            .bus_for("global")
                            .publish(&crate::events::EngineEvent::Custom(json!({
                                "type": "event.workspace.deleted",
                                "workspace_id": workspace_id,
                                "root": root.unwrap_or_default(),
                            })));
                        HttpResponse::ok(&json!({ "deleted": true }))
                    }
                    Ok(false) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            // Cron endpoints: session-scoped or global
            ("GET", p)
                if (p.starts_with("/api/v1/sessions/") && p.ends_with("/cron"))
                    || p == "/api/v1/cron" =>
            {
                if p != "/api/v1/cron" {
                    let segments: Vec<&str> = p.split('/').collect();
                    if segments.len() != 6 {
                        return HttpResponse::not_found();
                    }
                    let session_id = segments[4];
                    if self.store.get_session(session_id).ok().flatten().is_none() {
                        return HttpResponse::not_found();
                    }
                }
                let scheduler = self.cron_scheduler.lock().await;
                let entries = scheduler.list_entries();
                HttpResponse::ok(&json!({ "entries": entries }))
            }
            ("POST", p)
                if (p.starts_with("/api/v1/sessions/") && p.ends_with("/cron"))
                    || p == "/api/v1/cron" =>
            {
                let mut owning_session: Option<String> = None;
                if p != "/api/v1/cron" {
                    let segments: Vec<&str> = p.split('/').collect();
                    if segments.len() != 6 {
                        return HttpResponse::not_found();
                    }
                    let session_id = segments[4];
                    if self.store.get_session(session_id).ok().flatten().is_none() {
                        return HttpResponse::not_found();
                    }
                    owning_session = Some(session_id.to_string());
                }
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let cron_expr = match body.get("cron").and_then(|v| v.as_str()) {
                    Some(c) if !c.trim().is_empty() => c.trim(),
                    _ => return HttpResponse::bad_request("Field 'cron' is required"),
                };
                let prompt = match body.get("prompt").and_then(|v| v.as_str()) {
                    Some(p) if !p.trim().is_empty() => p.trim(),
                    _ => return HttpResponse::bad_request("Field 'prompt' is required"),
                };
                let id = body
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("cron-{}", fastrand::u64(..)));
                let recurring = body
                    .get("recurring")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);

                let entry = CronEntry {
                    id: id.clone(),
                    cron: cron_expr.to_string(),
                    prompt: prompt.to_string(),
                    recurring,
                    session_id: owning_session.clone(),
                    created_at: Some(chrono::Utc::now().timestamp_millis()),
                };
                let mut scheduler = self.cron_scheduler.lock().await;
                if scheduler.add_entry(entry) {
                    // Persist so the schedule survives a restart and is
                    // reloaded by `run_serve`.
                    self.persist_cron_entries(&scheduler.list_entries());
                    HttpResponse::json(
                        201,
                        &json!({
                            "id": id,
                            "cron": cron_expr,
                            "prompt": prompt,
                            "recurring": recurring
                        }),
                    )
                } else {
                    HttpResponse::bad_request("Invalid cron expression")
                }
            }
            ("DELETE", p)
                if (p.starts_with("/api/v1/sessions/") && p.contains("/cron/"))
                    || p.starts_with("/api/v1/cron/") =>
            {
                let task_id = if p.starts_with("/api/v1/cron/") {
                    let segments: Vec<&str> = p.split('/').collect();
                    if segments.len() != 5 {
                        return HttpResponse::not_found();
                    }
                    segments[4]
                } else {
                    let segments: Vec<&str> = p.split('/').collect();
                    if segments.len() != 7 || segments[5] != "cron" {
                        return HttpResponse::not_found();
                    }
                    let session_id = segments[4];
                    if self.store.get_session(session_id).ok().flatten().is_none() {
                        return HttpResponse::not_found();
                    }
                    segments[6]
                };
                let mut scheduler = self.cron_scheduler.lock().await;
                if scheduler.remove_entry(task_id) {
                    // Keep the persisted set in sync with the in-memory one.
                    self.persist_cron_entries(&scheduler.list_entries());
                    HttpResponse::ok(&json!({ "deleted": true, "taskId": task_id }))
                } else {
                    HttpResponse::not_found()
                }
            }

            // Tasks endpoints: session-scoped or global
            ("GET", p)
                if (p.starts_with("/api/v1/sessions/") && p.ends_with("/tasks"))
                    || p == "/api/v1/tasks" =>
            {
                if p != "/api/v1/tasks" {
                    let segments: Vec<&str> = p.split('/').collect();
                    if segments.len() != 6 {
                        return HttpResponse::not_found();
                    }
                    let session_id = segments[4];
                    if self.store.get_session(session_id).ok().flatten().is_none() {
                        return HttpResponse::not_found();
                    }
                }
                let tasks = self.task_runner.list();
                // v2 answers `{items}` only, with an optional `status` filter
                // (kap-server/src/routes/tasks.ts:82-87).
                let items: Vec<Value> = match req.query_param("status") {
                    Some(wanted) => tasks
                        .into_iter()
                        .filter(|task| task.get("status").and_then(|v| v.as_str()) == Some(&wanted))
                        .collect(),
                    None => tasks,
                };
                HttpResponse::ok(&json!({ "items": items }))
            }
            ("GET", p)
                if (p.starts_with("/api/v1/sessions/")
                    && p.contains("/tasks/")
                    && !p.ends_with("/stop"))
                    || (p.starts_with("/api/v1/tasks/") && !p.ends_with("/stop")) =>
            {
                let task_id = if p.starts_with("/api/v1/tasks/") {
                    let segments: Vec<&str> = p.split('/').collect();
                    if segments.len() != 5 {
                        return HttpResponse::not_found();
                    }
                    segments[4]
                } else {
                    let segments: Vec<&str> = p.split('/').collect();
                    if segments.len() != 7 || segments[5] != "tasks" {
                        return HttpResponse::not_found();
                    }
                    let session_id = segments[4];
                    if self.store.get_session(session_id).ok().flatten().is_none() {
                        return HttpResponse::not_found();
                    }
                    segments[6]
                };
                match self.task_runner.entry(task_id) {
                    // v2's `getTask` answers the bare wire task
                    // (`okEnvelope(toWireTask(...))`, kap-server/src/routes/tasks.ts:134).
                    Some(entry) => HttpResponse::ok(&entry),
                    None => HttpResponse::not_found(),
                }
            }
            ("POST", p)
                if (p.starts_with("/api/v1/sessions/")
                    && p.contains("/tasks/")
                    && p.ends_with("/stop"))
                    || (p.starts_with("/api/v1/tasks/") && p.ends_with("/stop")) =>
            {
                let task_id = if p.starts_with("/api/v1/tasks/") {
                    let segments: Vec<&str> = p.split('/').collect();
                    if segments.len() != 6 {
                        return HttpResponse::not_found();
                    }
                    segments[4]
                } else {
                    let segments: Vec<&str> = p.split('/').collect();
                    if segments.len() != 8 || segments[5] != "tasks" {
                        return HttpResponse::not_found();
                    }
                    let session_id = segments[4];
                    if self.store.get_session(session_id).ok().flatten().is_none() {
                        return HttpResponse::not_found();
                    }
                    segments[6]
                };
                match self
                    .task_runner
                    .stop(
                        task_id,
                        serde_json::from_slice::<Value>(&req.body)
                            .ok()
                            .as_ref()
                            .and_then(|body| body.get("reason"))
                            .and_then(|reason| reason.as_str()),
                    )
                    .await
                {
                    Ok(outcome) => {
                        let wire = outcome.wire;
                        HttpResponse::ok(&json!({ "stopped": true, "task": wire }))
                    }
                    Err(_) => HttpResponse::not_found(),
                }
            }
            ("POST", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.contains("/tasks/")
                    && p.ends_with(":cancel") =>
            {
                let remainder = &p["/api/v1/sessions/".len()..];
                let (session_id, rest) = match remainder.split_once("/tasks/") {
                    Some(pair) => pair,
                    None => return HttpResponse::not_found(),
                };
                let task_id = match rest.strip_suffix(":cancel") {
                    Some(tid) if !tid.is_empty() => tid,
                    _ => return HttpResponse::not_found(),
                };
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                if let Some(entry) = self.task_runner.entry(task_id) {
                    let status = entry.get("status").and_then(|v| v.as_str()).unwrap_or("");
                    if status != "running" && status != "pending" {
                        return HttpResponse::json(
                            409,
                            &json!({ "error": "task.already_finished", "code": 40904 }),
                        );
                    }
                    let _ = self
                        .task_runner
                        .stop(task_id, Some("Cancelled by client"))
                        .await;
                    HttpResponse::ok(&json!({ "cancelled": true }))
                } else {
                    HttpResponse::json(404, &json!({ "error": "task.not_found", "code": 40406 }))
                }
            }
            ("POST", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.contains("/tasks/")
                    && p.ends_with(":detach") =>
            {
                let remainder = &p["/api/v1/sessions/".len()..];
                let (session_id, rest) = match remainder.split_once("/tasks/") {
                    Some(pair) => pair,
                    None => return HttpResponse::not_found(),
                };
                let task_id = match rest.strip_suffix(":detach") {
                    Some(tid) if !tid.is_empty() => tid,
                    _ => return HttpResponse::not_found(),
                };
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                if let Some(entry) = self.task_runner.entry(task_id) {
                    let status = entry.get("status").cloned().unwrap_or(json!("completed"));
                    HttpResponse::ok(&json!({ "detached": false, "status": status }))
                } else {
                    HttpResponse::json(404, &json!({ "error": "task.not_found", "code": 40406 }))
                }
            }

            // Subagent management endpoints
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }

    /// The trust routes report the *effective* state (upstream #4517): the
    /// persisted flag OR'd with the `KIMI_CODE_TRUST_WORKSPACE` process
    /// override, short-circuited to trusted while `[workspace] require_trust`
    /// is `false`. The persisted record is what `set_workspace_trusted`
    /// writes; the overrides never rewrite it.
    async fn effective_workspace_trust(&self, persisted: bool) -> bool {
        !self.config().await.resolve_workspace_require_trust()
            || crate::workspace_trust::env_trust_override()
            || persisted
    }
}
