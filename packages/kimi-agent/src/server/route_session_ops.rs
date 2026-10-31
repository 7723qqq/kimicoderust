use super::catalog_routes::format_wire_session;
use super::*;

impl HttpServer {
    pub(super) async fn route_session_ops(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("GET", p)
                if (p.starts_with("/api/v1/sessions/") || p.starts_with("/sessions/"))
                    && (p.contains("/fs/") || p.contains(":fs/"))
                    && p.ends_with(":download") =>
            {
                let rest = p
                    .strip_prefix("/api/v1/sessions/")
                    .or_else(|| p.strip_prefix("/sessions/"))
                    .unwrap_or_default();
                let (session_id, file_path_raw) = if let Some((s, f)) = rest.split_once("/fs/") {
                    (s, f)
                } else if let Some((s, f)) = rest.split_once(":fs/") {
                    (s, f)
                } else {
                    return HttpResponse::not_found();
                };
                let rel_path = file_path_raw
                    .strip_suffix(":download")
                    .unwrap_or(file_path_raw);
                let work_dir = match fs_routes::resolve_session_workdir(&self.store, session_id) {
                    Some(d) => d,
                    None => return HttpResponse::not_found(),
                };
                let target = match fs_routes::resolve_safe_path(&work_dir, rel_path) {
                    Ok(t) => t,
                    Err(resp) => return resp,
                };
                if !target.is_file() {
                    return HttpResponse::not_found();
                }
                match std::fs::read(&target) {
                    Ok(bytes) => {
                        let mime = media::mime_for_path(&target);
                        HttpResponse::bytes(200, mime, bytes)
                    }
                    Err(e) => HttpResponse::internal_error(format!("Failed to read file: {e}")),
                }
            }
            ("POST", p)
                if p == "/api/v1/workspace/fs::search"
                    || p == "/workspace/fs::search"
                    || p == "/api/v1/workspace/fs:search"
                    || p == "/workspace/fs:search"
                    || p == "/api/v1/fs::suggest"
                    || p == "/fs::suggest"
                    || p == "/api/v1/fs:suggest"
                    || p == "/fs:suggest"
                    // v2 spells these `/workspace/fs::search` and
                    // `/workspace/fs::suggest` (double colon,
                    // kap-server/src/routes/fs.ts:414,460); the official Web
                    // bundle calls both with a *single* colon. The `::search`
                    // sibling already accepted both spellings here, so `suggest`
                    // now matches it — otherwise every file-mention request paid
                    // a 404 round-trip before the client's fallback to search.
                    || p == "/api/v1/workspace/fs::suggest"
                    || p == "/workspace/fs::suggest"
                    || p == "/api/v1/workspace/fs:suggest"
                    || p == "/workspace/fs:suggest" =>
            {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => json!({}),
                };
                let work_dir = if let Some(ws) = body.get("workspace").and_then(|v| v.as_str()) {
                    if let Ok(Some(ws_entry)) = self.store.get_workspace(ws) {
                        PathBuf::from(ws_entry.root)
                    } else {
                        PathBuf::from(ws)
                    }
                } else if let Some(roots) = body.get("roots").and_then(|v| v.as_array())
                    && let Some(r0) = roots.first().and_then(|r| r.as_str())
                {
                    PathBuf::from(r0)
                } else {
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                };
                fs_routes::handle_search(&work_dir, &body)
            }

            // Terminal endpoints
            ("GET", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/terminals") => {
                let session_id = p
                    .strip_prefix("/api/v1/sessions/")
                    .unwrap_or_default()
                    .strip_suffix("/terminals")
                    .unwrap_or_default();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let items = self.terminal_manager.list(session_id).await;
                HttpResponse::ok(&json!({ "items": items }))
            }
            ("POST", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/terminals") => {
                let session_id = p
                    .strip_prefix("/api/v1/sessions/")
                    .unwrap_or_default()
                    .strip_suffix("/terminals")
                    .unwrap_or_default();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let work_dir = fs_routes::resolve_session_workdir(&self.store, session_id)
                    .unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                    });
                let cwd_str = body
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(|c| work_dir.join(c).to_string_lossy().to_string())
                    .unwrap_or_else(|| work_dir.to_string_lossy().to_string());
                let shell_opt = body.get("shell").and_then(|v| v.as_str());
                let cols_opt = match terminal_dimension(body.get("cols"), "cols") {
                    Ok(value) => value,
                    Err(e) => return HttpResponse::bad_request(e),
                };
                let rows_opt = match terminal_dimension(body.get("rows"), "rows") {
                    Ok(value) => value,
                    Err(e) => return HttpResponse::bad_request(e),
                };

                match self
                    .terminal_manager
                    .create(session_id, &cwd_str, shell_opt, cols_opt, rows_opt)
                    .await
                {
                    Ok(term) => HttpResponse::json(201, &json!(term)),
                    Err(err) => HttpResponse::internal_error(err),
                }
            }
            ("GET", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.contains("/terminals/")
                    && !p.ends_with("/output") =>
            {
                let parts: Vec<&str> = p.split("/terminals/").collect();
                if parts.len() != 2 {
                    return HttpResponse::not_found();
                }
                let session_id = parts[0]
                    .strip_prefix("/api/v1/sessions/")
                    .unwrap_or_default();
                let terminal_id = parts[1];
                match self.terminal_manager.get(session_id, terminal_id).await {
                    Some(term) => HttpResponse::ok(&json!(term)),
                    None => HttpResponse::not_found(),
                }
            }
            ("POST", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.contains("/terminals/")
                    && (p.ends_with(":close") || p.ends_with("/close")) =>
            {
                let parts: Vec<&str> = p.split("/terminals/").collect();
                if parts.len() != 2 {
                    return HttpResponse::not_found();
                }
                let session_id = parts[0]
                    .strip_prefix("/api/v1/sessions/")
                    .unwrap_or_default();
                let tail = parts[1];
                let terminal_id = tail
                    .strip_suffix(":close")
                    .or_else(|| tail.strip_suffix("/close"))
                    .unwrap_or(tail);
                match self.terminal_manager.close(session_id, terminal_id).await {
                    Ok(()) => HttpResponse::ok(&json!({ "closed": true })),
                    Err(e) => HttpResponse::bad_request(e),
                }
            }
            ("POST", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.contains("/terminals/")
                    && (p.ends_with(":write") || p.ends_with("/write")) =>
            {
                let parts: Vec<&str> = p.split("/terminals/").collect();
                if parts.len() != 2 {
                    return HttpResponse::not_found();
                }
                let session_id = parts[0]
                    .strip_prefix("/api/v1/sessions/")
                    .unwrap_or_default();
                let tail = parts[1];
                let terminal_id = tail
                    .strip_suffix(":write")
                    .or_else(|| tail.strip_suffix("/write"))
                    .unwrap_or(tail);
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let data = match body.get("data").and_then(|v| v.as_str()) {
                    Some(data) => data,
                    None => {
                        return HttpResponse::bad_request("Field 'data' must be a string");
                    }
                };
                match self
                    .terminal_manager
                    .write(session_id, terminal_id, data.as_bytes())
                    .await
                {
                    Ok(()) => HttpResponse::ok(&json!({ "written": true })),
                    Err(e) => HttpResponse::bad_request(e),
                }
            }
            ("POST", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.contains("/terminals/")
                    && (p.ends_with(":resize") || p.ends_with("/resize")) =>
            {
                let parts: Vec<&str> = p.split("/terminals/").collect();
                if parts.len() != 2 {
                    return HttpResponse::not_found();
                }
                let session_id = parts[0]
                    .strip_prefix("/api/v1/sessions/")
                    .unwrap_or_default();
                let tail = parts[1];
                let terminal_id = tail
                    .strip_suffix(":resize")
                    .or_else(|| tail.strip_suffix("/resize"))
                    .unwrap_or(tail);
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let cols = match terminal_dimension(body.get("cols"), "cols") {
                    Ok(Some(cols)) => cols,
                    Ok(None) => 80,
                    Err(e) => return HttpResponse::bad_request(e),
                };
                let rows = match terminal_dimension(body.get("rows"), "rows") {
                    Ok(Some(rows)) => rows,
                    Ok(None) => 24,
                    Err(e) => return HttpResponse::bad_request(e),
                };
                match self
                    .terminal_manager
                    .resize(session_id, terminal_id, cols, rows)
                    .await
                {
                    Ok(()) => HttpResponse::ok(&json!({ "resized": true })),
                    Err(e) => HttpResponse::bad_request(e),
                }
            }
            ("GET", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.contains("/terminals/")
                    && p.ends_with("/output") =>
            {
                let parts: Vec<&str> = p.split("/terminals/").collect();
                if parts.len() != 2 {
                    return HttpResponse::not_found();
                }
                let session_id = parts[0]
                    .strip_prefix("/api/v1/sessions/")
                    .unwrap_or_default();
                let tail = parts[1];
                let terminal_id = tail.strip_suffix("/output").unwrap_or(tail);
                let since_seq = req
                    .query_param("since_seq")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                match self
                    .terminal_manager
                    .output(session_id, terminal_id, since_seq)
                    .await
                {
                    Ok((output, last_seq)) => {
                        // `last_seq` is the absolute sequence of the newest
                        // buffered frame (`total` is the same value: the total
                        // number of frames the terminal has produced). It used to
                        // report the buffer length, which stops matching the
                        // sequence numbers as soon as frames are evicted.
                        let count = output.len();
                        HttpResponse::ok(&json!({
                            "output": output,
                            "count": count,
                            "last_seq": last_seq,
                            "total": last_seq,
                        }))
                    }
                    Err(e) => HttpResponse::bad_request(e),
                }
            }

            ("GET", p)
                if p.starts_with("/api/v1/sessions/")
                    && (p.ends_with("/file-history/changes")
                        || p.ends_with(":file-history/changes")
                        || p.ends_with(":file-history:changes")) =>
            {
                let rest = p.strip_prefix("/api/v1/sessions/").unwrap_or_default();
                let session_id = rest
                    .split('/')
                    .next()
                    .unwrap_or_default()
                    .split(':')
                    .next()
                    .unwrap_or_default();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let turn_id = req
                    .query_param("turn_id")
                    .and_then(|t| t.parse::<usize>().ok());
                match self.store.get_file_history_changes(session_id, turn_id) {
                    Ok((changes, recorded)) => HttpResponse::ok(&json!({
                        "sessionId": session_id,
                        "changes": changes,
                        "enabled": true,
                        "recorded": recorded,
                    })),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("GET", p)
                if p.starts_with("/api/v1/sessions/")
                    && (p.ends_with("/file-history/content")
                        || p.ends_with(":file-history/content")
                        || p.ends_with(":file-history:content")) =>
            {
                let rest = p.strip_prefix("/api/v1/sessions/").unwrap_or_default();
                let session_id = rest
                    .split('/')
                    .next()
                    .unwrap_or_default()
                    .split(':')
                    .next()
                    .unwrap_or_default();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let turn_id = match req
                    .query_param("turn_id")
                    .and_then(|t| t.parse::<usize>().ok())
                {
                    Some(t) => t,
                    None => {
                        return HttpResponse::bad_request(
                            "Query parameter 'turn_id' is required and must be an integer",
                        );
                    }
                };
                let file_path = match req.query_param("path") {
                    Some(path) if !path.trim().is_empty() => path,
                    _ => return HttpResponse::bad_request("Query parameter 'path' is required"),
                };
                let phase = req.query_param("phase").unwrap_or_else(|| "end".into());

                match self
                    .store
                    .get_file_history_content(session_id, turn_id, &file_path, &phase)
                {
                    Ok(Some(item)) => HttpResponse::ok(&json!({
                        "sessionId": session_id,
                        "version": item.version,
                        "content": item.content,
                        "binary": item.binary.unwrap_or(false),
                    })),
                    Ok(None) => HttpResponse::ok(&json!({
                        "sessionId": session_id,
                        "version": 0,
                        "content": Value::Null,
                        "binary": false,
                    })),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("GET", p) if p.starts_with("/api/v1/sessions/") && p.contains("/media/") => {
                let parts: Vec<&str> = p.split("/media/").collect();
                if parts.len() != 2 {
                    return HttpResponse::not_found();
                }
                let session_prefix = parts[0];
                let file_id = parts[1];
                let session_id = session_prefix
                    .strip_prefix("/api/v1/sessions/")
                    .unwrap_or_default();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                if let Some(path) = media::locate_session_media(session_id, file_id)
                    && let Ok(data) = std::fs::read(&path)
                {
                    let mime = media::mime_for_path(&path);
                    return HttpResponse::bytes(200, mime, data);
                }
                HttpResponse::not_found()
            }
            ("GET", p) if extract_session_action(p, "events").is_some() => {
                let session_id = extract_session_action(p, "events").unwrap();
                let exists = self.store.get_session(session_id).ok().flatten().is_some()
                    || self.hub.lane_exists(session_id);
                if !exists {
                    return HttpResponse::not_found();
                }
                let since = req
                    .query_param("since")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let limit = req
                    .query_param("limit")
                    .and_then(|l| l.parse::<usize>().ok())
                    .unwrap_or(100)
                    .min(1000);

                match self.store.get_wire_events(session_id, since, limit) {
                    Ok(events) => {
                        let total = self
                            .store
                            .count_wire_events(session_id)
                            .unwrap_or(events.len());
                        let latest_seq = self.store.latest_wire_event_seq(session_id).unwrap_or(0);
                        HttpResponse::ok(&json!({
                            "sessionId": session_id,
                            "events": events,
                            "total": total,
                            "latestSeq": latest_seq,
                        }))
                    }
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("GET", p) if extract_session_action(p, "projection").is_some() => {
                let session_id = extract_session_action(p, "projection").unwrap();
                let exists = self.store.get_session(session_id).ok().flatten().is_some()
                    || self.hub.lane_exists(session_id);
                if !exists {
                    return HttpResponse::not_found();
                }
                match self.store.fold_projection(session_id) {
                    Ok(messages) => HttpResponse::ok(&json!({
                        "sessionId": session_id,
                        "messages": messages,
                    })),
                    Err(e) => HttpResponse::internal_error(format!("Projection error: {e}")),
                }
            }

            ("GET", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.strip_prefix("/api/v1/sessions/")
                        .is_some_and(|rest| !rest.contains('/'))
                    && !p.ends_with("/prompt") =>
            {
                let session_id = p.strip_prefix("/api/v1/sessions/").unwrap_or_default();
                if session_id.is_empty() || session_id.contains('/') || session_id.contains(':') {
                    return HttpResponse::not_found();
                }
                match self.store.get_session(session_id) {
                    Ok(Some(session)) => {
                        // v2 `GET /sessions/{session_id}` answers the session
                        // document itself (`toWireSession` → `sessionSchema`,
                        // kap-server/src/routes/sessions.ts:1024), wrapped only
                        // in the standard envelope — messages live on their own
                        // `/messages` route. Serving the document bare is what
                        // lets a client map the response directly.
                        let wire_session =
                            format_wire_session(&session, &self.store, self.engine.as_ref());
                        HttpResponse::ok(&wire_session)
                    }
                    Ok(None) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("DELETE", p) if p.starts_with("/api/v1/sessions/") && !p.ends_with("/prompt") => {
                let session_id = p.strip_prefix("/api/v1/sessions/").unwrap_or_default();
                if session_id.is_empty() || session_id.contains('/') || session_id.contains(':') {
                    return HttpResponse::not_found();
                }
                let workspace_id = self
                    .store
                    .get_session(session_id)
                    .ok()
                    .flatten()
                    .map(|s| s.workspace_id);
                match self.store.delete_session(session_id) {
                    Ok(true) => {
                        self.interaction_manager.cancel_session(session_id);
                        // v2 `sessionExternalHooksService.triggerSessionEnd`:
                        // deleting the session archives it —the close
                        // reason is `archive` (REPL exit is `exit`).
                        // Observational: the deletion proceeds regardless.
                        let guard = crate::tools::external_hooks::HookGuard::new(
                            self.config().await.hooks.clone(),
                        );
                        guard
                            .notify_session_lifecycle(
                                "SessionEnd",
                                "archive",
                                json!({ "reason": "archive", "session_title": "" }),
                            )
                            .await;
                        let del_event = json!({
                            "type": "event.session.deleted",
                            "sessionId": session_id,
                            "workspaceId": workspace_id,
                        });
                        self.hub
                            .bus_for("global")
                            .publish(&crate::events::EngineEvent::Custom(del_event));
                        HttpResponse::ok(&json!({ "deleted": true, "sessionId": session_id }))
                    }
                    Ok(false) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("POST", p)
                if extract_session_action(p, "events:compact").is_some()
                    || extract_session_action(p, "events/compact").is_some() =>
            {
                let session_id = extract_session_action(p, "events:compact")
                    .or_else(|| extract_session_action(p, "events/compact"))
                    .unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let body: Value = serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
                let summary = body
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .unwrap_or("Context compaction boundary");
                match self.store.checkpoint_compress(session_id, summary) {
                    Ok(()) => HttpResponse::ok(&json!({
                        "sessionId": session_id,
                        "compacted": true,
                        "summary": summary
                    })),
                    Err(e) => HttpResponse::internal_error(format!("Compaction error: {e}")),
                }
            }
            ("POST", p)
                if extract_session_action(p, "events:undo").is_some()
                    || extract_session_action(p, "events/undo").is_some() =>
            {
                let session_id = extract_session_action(p, "events:undo")
                    .or_else(|| extract_session_action(p, "events/undo"))
                    .unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                match self.store.undo_to_last_checkpoint(session_id) {
                    Ok(count) => HttpResponse::ok(&json!({
                        "sessionId": session_id,
                        "undone": true,
                        "deletedEvents": count
                    })),
                    Err(crate::native::event_store::EventStoreError::UndoCompactionBoundary) => {
                        HttpResponse::bad_request("Cannot undo across compaction boundary")
                    }
                    Err(crate::native::event_store::EventStoreError::CheckpointNotFound) => {
                        HttpResponse::bad_request("No checkpoint found to undo to")
                    }
                    Err(e) => HttpResponse::internal_error(format!("Undo error: {e}")),
                }
            }
            ("PATCH", p) if p.starts_with("/api/v1/sessions/") && !p.ends_with("/prompt") => {
                let session_id = p.strip_prefix("/api/v1/sessions/").unwrap_or_default();
                if session_id.is_empty() || session_id.contains('/') || session_id.contains(':') {
                    return HttpResponse::not_found();
                }
                self.handle_session_patch(session_id, &req.body)
            }
            ("POST", p) if extract_session_action(p, "patch").is_some() => {
                let session_id = extract_session_action(p, "patch").unwrap();
                self.handle_session_patch(session_id, &req.body)
            }
            ("POST", p)
                if extract_session_action(p, "patch:undo").is_some()
                    || extract_session_action(p, "undo_patch").is_some() =>
            {
                let session_id = extract_session_action(p, "patch:undo")
                    .or_else(|| extract_session_action(p, "undo_patch"))
                    .unwrap();
                self.handle_session_patch_undo(session_id)
            }
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}
