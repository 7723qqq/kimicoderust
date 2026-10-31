use super::catalog_routes::format_wire_session;
use super::*;

impl HttpServer {
    pub(super) async fn route_workspaces_fs(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("GET", "/api/v1/sessions") => match self.store.list_sessions() {
                Ok(sessions) => {
                    // v2 `sessionsListQueryCoercion`
                    // (kap-server/src/routes/sessions.ts:101-136) is the contract:
                    // page_size (default 20, max 100), busy, include_archive,
                    // exclude_empty, archived_only, workspace_id. The response is
                    // `pageResponseSchema(sessionSchema)` — `{items, has_more}`.
                    let flag = |name: &str| -> Option<bool> {
                        req.query_param(name).map(|v| v == "true" || v == "1")
                    };
                    let exclude_empty = flag("exclude_empty").unwrap_or(false);
                    let archived_only = flag("archived_only").unwrap_or(false);
                    let include_archive = flag("include_archive").unwrap_or(false);
                    let busy_filter = flag("busy");
                    let page_size = req
                        .query_param("page_size")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(20)
                        .clamp(1, 100);
                    let workspace_filter = req.query_param("workspace_id");
                    let items: Vec<Value> = sessions
                        .iter()
                        .filter(|s| {
                            // `archived_only` and `include_archive` are mutually
                            // exclusive upstream; the default hides archived
                            // sessions.
                            if archived_only {
                                if !s.archived {
                                    return false;
                                }
                            } else if !include_archive && s.archived {
                                return false;
                            }
                            if let Some(want) = workspace_filter.as_deref()
                                && s.workspace_id.as_deref() != Some(want)
                            {
                                return false;
                            }
                            if let Some(want_busy) = busy_filter
                                && self
                                    .engine
                                    .as_ref()
                                    .map(|e| e.is_busy(&s.session_id))
                                    .unwrap_or(false)
                                    != want_busy
                            {
                                return false;
                            }
                            !exclude_empty
                                || !self
                                    .store
                                    .load_session_history(&s.session_id)
                                    .map(|h| h.is_empty())
                                    .unwrap_or(true)
                        })
                        .map(|s| format_wire_session(s, &self.store, self.engine.as_ref()))
                        .collect();
                    let total = items.len();
                    let page: Vec<Value> = items.into_iter().take(page_size).collect();
                    // `sessions` stays for the in-tree clients that predate the
                    // paged contract.
                    HttpResponse::ok(&json!({
                        "items": page,
                        "has_more": total > page_size,
                        "sessions": page,
                    }))
                }
                Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
            },
            // Workspaces endpoints
            ("GET", "/api/v1/workspaces") => match self.store.list_workspaces() {
                Ok(items) => HttpResponse::ok(&json!({ "items": items })),
                Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
            },
            // FS endpoints (workspace-agnostic folder browser for Web UI / Desktop)
            ("GET", "/api/v1/fs:home") | ("GET", "/api/v1/fs::home") => {
                let home_path = std::env::var("USERPROFILE")
                    .or_else(|_| std::env::var("HOME"))
                    .unwrap_or_else(|_| ".".into());
                let normalized_home = home_path.replace('\\', "/");
                let recent_roots: Vec<String> = self
                    .store
                    .list_workspaces()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|w| w.root)
                    .take(20)
                    .collect();
                HttpResponse::ok(&json!({
                    "home": normalized_home,
                    "recent_roots": recent_roots,
                }))
            }
            ("GET", "/api/v1/fs:browse") | ("GET", "/api/v1/fs::browse") => {
                let default_home = std::env::var("USERPROFILE")
                    .or_else(|_| std::env::var("HOME"))
                    .unwrap_or_else(|_| ".".into());
                let raw_target = req
                    .query_param("path")
                    .filter(|p| !p.trim().is_empty())
                    .unwrap_or(default_home);
                let target_path = std::path::Path::new(&raw_target);
                if !target_path.exists() {
                    return HttpResponse::not_found();
                }
                if !target_path.is_dir() {
                    return HttpResponse::bad_request("Path is not a directory");
                }
                let canonical = match target_path.canonicalize() {
                    Ok(p) => p,
                    Err(_) => target_path.to_path_buf(),
                };
                let canonical_str = canonical
                    .to_string_lossy()
                    .replace('\\', "/")
                    .trim_start_matches("//?/")
                    .trim_start_matches(r"\\?\")
                    .to_string();

                let parent_str = canonical.parent().map(|p| {
                    p.to_string_lossy()
                        .replace('\\', "/")
                        .trim_start_matches("//?/")
                        .trim_start_matches(r"\\?\")
                        .to_string()
                });
                let effective_parent = if parent_str.as_deref() == Some(&canonical_str) {
                    None
                } else {
                    parent_str
                };

                let mut entries = Vec::new();
                if let Ok(read_dir) = std::fs::read_dir(&canonical) {
                    for entry in read_dir.flatten() {
                        if let Ok(file_type) = entry.file_type()
                            && file_type.is_dir()
                        {
                            let file_name = entry.file_name().to_string_lossy().to_string();
                            let full_path = entry
                                .path()
                                .to_string_lossy()
                                .replace('\\', "/")
                                .trim_start_matches("//?/")
                                .trim_start_matches(r"\\?\")
                                .to_string();
                            entries.push(json!({
                                "name": file_name,
                                "path": full_path,
                                "is_dir": true,
                            }));
                        }
                    }
                }

                entries.sort_by(|a, b| {
                    let name_a = a["name"].as_str().unwrap_or_default();
                    let name_b = b["name"].as_str().unwrap_or_default();
                    let dot_a = name_a.starts_with('.');
                    let dot_b = name_b.starts_with('.');
                    if dot_a != dot_b {
                        dot_a.cmp(&dot_b)
                    } else {
                        name_a.to_lowercase().cmp(&name_b.to_lowercase())
                    }
                });

                HttpResponse::ok(&json!({
                    "path": canonical_str,
                    "parent": effective_parent,
                    "entries": entries,
                }))
            }
            // Raw file content by absolute path (kap-server `fsContent`): ETag
            // revalidation and a single `Range` slice, everything else served
            // in full. Auth is the bearer check at the HTTP layer — same trust
            // level as `fs:browse`, which also walks the whole host.
            ("GET", "/api/v1/fs::content") | ("GET", "/api/v1/fs:content") => {
                crate::server::fs_routes::handle_fs_content(
                    req.query_param("path").as_deref(),
                    req.header("if-none-match"),
                    req.header("range"),
                )
            }
            ("POST", "/api/v1/search") | ("POST", "/api/v1/sessions:search") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let query = match body.get("query").and_then(|v| v.as_str()) {
                    Some(q) if !q.trim().is_empty() => q.trim(),
                    _ => return HttpResponse::bad_request("Field 'query' is required"),
                };
                let session_id = body
                    .get("container")
                    .and_then(|c| c.get("session_id"))
                    .and_then(|v| v.as_str());
                let role = body.get("role").and_then(|v| v.as_str());
                let page_size =
                    body.get("page_size").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                // Opaque cursor: the message offset encoded as a decimal
                // string. Absent/null means the first page.
                let offset = match body.get("page_token") {
                    None | Some(Value::Null) => 0usize,
                    Some(Value::String(s)) if s.trim().is_empty() => 0usize,
                    Some(Value::String(s)) => match s.trim().parse::<usize>() {
                        Ok(n) => n,
                        Err(_) => {
                            return HttpResponse::bad_request(
                                "Field 'page_token' must be an opaque cursor from a previous response",
                            );
                        }
                    },
                    Some(_) => {
                        return HttpResponse::bad_request(
                            "Field 'page_token' must be an opaque cursor from a previous response",
                        );
                    }
                };

                // Fetch one past the page to know whether a next page exists.
                let mut hits = match self.store.search_messages(
                    query,
                    session_id,
                    role,
                    page_size.saturating_add(1),
                    offset,
                ) {
                    Ok(h) => h,
                    Err(e) => return HttpResponse::internal_error(format!("Database error: {e}")),
                };

                let has_more = hits.len() > page_size;
                hits.truncate(page_size);
                let next_page_token = if has_more {
                    Value::String((offset + page_size).to_string())
                } else {
                    Value::Null
                };

                let total_sessions = self.store.list_sessions().map(|s| s.len()).unwrap_or(0);
                let total_messages = self.store.count_messages().unwrap_or(0);

                HttpResponse::ok(&json!({
                    "items": hits,
                    "has_more": has_more,
                    "page_token": next_page_token,
                    "incomplete": false,
                    "index_state": {
                        "state": "ready",
                        "indexed_sessions": total_sessions,
                        "total_sessions": total_sessions,
                        "documents": total_messages,
                        "stale": false,
                        "degraded": false
                    },
                    "source": "sqlite"
                }))
            }
            ("POST", "/api/v1/workspace/fs:search") | ("POST", "/api/v1/workspace/fs::search") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let ws_ref = match body.get("workspace").and_then(|v| v.as_str()) {
                    Some(w) if !w.trim().is_empty() => w.trim(),
                    _ => return HttpResponse::bad_request("Field 'workspace' is required"),
                };
                let query = body
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase();
                let limit = body.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

                let root_dir = if let Ok(Some(ws)) = self.store.get_workspace(ws_ref) {
                    std::path::PathBuf::from(ws.root)
                } else {
                    std::path::PathBuf::from(ws_ref)
                };

                if !root_dir.is_dir() {
                    return HttpResponse::bad_request("Workspace root is not a valid directory");
                }

                let mut items = Vec::new();
                let mut truncated = false;
                let walker = ignore::WalkBuilder::new(&root_dir)
                    .hidden(false)
                    .git_ignore(true)
                    .max_depth(Some(8))
                    .build();

                for result in walker.flatten() {
                    let path = result.path();
                    if path == root_dir {
                        continue;
                    }
                    let rel_path = match path.strip_prefix(&root_dir) {
                        Ok(p) => p.to_string_lossy().replace('\\', "/"),
                        Err(_) => continue,
                    };

                    if query.is_empty() || rel_path.to_lowercase().contains(&query) {
                        if items.len() >= limit {
                            truncated = true;
                            break;
                        }
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let kind = if path.is_dir() { "directory" } else { "file" };
                        items.push(json!({
                            "path": rel_path,
                            "name": name,
                            "kind": kind,
                            "score": 100,
                            "match_positions": [],
                        }));
                    }
                }

                HttpResponse::ok(&json!({
                    "items": items,
                    "truncated": truncated,
                }))
            }
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}
