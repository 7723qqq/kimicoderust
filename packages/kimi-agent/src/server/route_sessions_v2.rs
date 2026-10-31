use super::*;

impl HttpServer {
    pub(super) async fn route_sessions_v2(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("GET", "/api/v2/sessions") => {
                // The official Web bundle's `listSessionsV2` contract
                // (upstream `routes/v2/sessions.ts` `v2SessionPageSchema`):
                // domain-grouped items plus total/has_more/next_page_token.
                // Page-token pagination is accepted for compatibility and
                // answered as a single complete page (the fork's session
                // count is small); `page_token` stays null. `view=by_workspace`
                // groups the matching set per workspace instead
                // (`v2SessionGroupPageSchema`), which is what the sessions
                // sidebar loads first.
                let sessions = self.store.list_sessions().unwrap_or_default();
                let engine = self.engine.as_ref();
                let query_archived = req.query_param("meta.archived").map(|v| v == "true");
                let query_has_prompt = req.query_param("meta.has_prompt").map(|v| v == "true");
                let view_by_workspace =
                    req.query_param("view").is_some_and(|v| v == "by_workspace");
                let group_page_size = req
                    .query_param("group.page_size")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(5);

                let mut items: Vec<Value> = Vec::new();
                for s in sessions {
                    if query_archived.is_some_and(|want| s.archived != want) {
                        continue;
                    }
                    let busy = engine.map(|e| e.is_busy(&s.session_id)).unwrap_or(false);
                    let has_prompt = self
                        .store
                        .load_session_history(&s.session_id)
                        .map(|history| !history.is_empty())
                        .unwrap_or(false);
                    if query_has_prompt.is_some_and(|want| has_prompt != want) {
                        continue;
                    }
                    let model = self
                        .store
                        .get_state("agent_config", &s.session_id)
                        .ok()
                        .flatten()
                        .and_then(|c| c.get("model").and_then(|m| m.as_str()).map(str::to_string))
                        .or_else(|| engine.map(|e| e.model_name().to_string()));
                    let workspace_cwd = s
                        .workspace_id
                        .as_deref()
                        .and_then(|wid| self.store.get_workspace(wid).ok().flatten())
                        .map(|w| w.root);
                    items.push(json!({
                        "id": s.session_id,
                        "workspace": {
                            "id": s.workspace_id,
                            "cwd": workspace_cwd,
                        },
                        "meta": {
                            "title": s.title,
                            "last_prompt": Value::Null,
                            "created_at": s.created_at,
                            "updated_at": s.updated_at,
                            "archived": s.archived,
                            "archived_at": Value::Null,
                            "has_prompt": has_prompt,
                        },
                        "activity": {
                            // v2ActivityStatusSchema vocabulary
                            // (running/approval/question/failed/idle):
                            // the bundle's mapper maps `running` → busy
                            // and `approval`/`question` → pending
                            // interactions.
                            "status": if busy { "running" } else { "idle" },
                            "model": model
                        }
                    }));
                }
                let total = items.len();
                if view_by_workspace {
                    // Group every session under its workspace: each group
                    // carries the workspace object and the first
                    // `group.page_size` sessions of that workspace.
                    let workspaces = self.store.list_workspaces().unwrap_or_default();
                    let mut groups: Vec<Value> = workspaces
                        .iter()
                        .map(|w| {
                            let members: Vec<&Value> = items
                                .iter()
                                .filter(|i| i["workspace"]["id"] == json!(w.id))
                                .collect();
                            let group_total = members.len();
                            let sessions: Vec<Value> =
                                members.into_iter().take(group_page_size).cloned().collect();
                            json!({
                                "workspace": {
                                    "id": w.id,
                                    "cwd": w.root,
                                },
                                "sessions": sessions,
                                "total": group_total,
                            })
                        })
                        .collect();
                    // Sessions whose workspace row is missing (deleted row or
                    // ad-hoc cwd sessions) land in a synthetic group keyed by
                    // the session's own workspace id so they stay reachable.
                    let orphan_workspaces: Vec<Value> = items
                        .iter()
                        .filter(|i| {
                            workspaces
                                .iter()
                                .all(|w| json!(w.id) != i["workspace"]["id"])
                        })
                        .map(|i| i["workspace"].clone())
                        .collect();
                    for ws_value in orphan_workspaces {
                        let members: Vec<&Value> = items
                            .iter()
                            .filter(|i| i["workspace"]["id"] == ws_value["id"])
                            .collect();
                        let group_total = members.len();
                        let group_sessions: Vec<Value> =
                            members.into_iter().take(group_page_size).cloned().collect();
                        groups.push(json!({
                            "workspace": ws_value,
                            "sessions": group_sessions,
                            "total": group_total,
                        }));
                    }
                    HttpResponse::ok(&json!({
                        "groups": groups,
                        "total": total,
                        "has_more": false,
                        "next_page_token": Value::Null
                    }))
                } else {
                    HttpResponse::ok(&json!({
                        "items": items,
                        "total": total,
                        "has_more": false,
                        "next_page_token": Value::Null
                    }))
                }
            }
            // Batch archive/restore (upstream `routes/v2/sessions.ts`): the
            // official Web bundle posts `{ ids: [...] }` and folds per-item
            // results; a missing session folds into its own item instead of
            // failing the whole batch.
            ("POST", "/api/v2/sessions:archive") | ("POST", "/api/v2/sessions:restore") => {
                let restore = req.path.ends_with(":restore");
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let Some(ids) = body.get("ids").and_then(Value::as_array) else {
                    return HttpResponse::bad_request("Missing ids");
                };
                if ids.len() > 5000 {
                    return HttpResponse::json(
                        422,
                        &json!({ "code": crate::server::envelope::error_codes::VALIDATION_FAILED, "msg": "ids exceeds 5000" }),
                    );
                }
                let results: Vec<Value> = ids
                    .iter()
                    .filter_map(|id| id.as_str())
                    .map(|id| {
                        let outcome = if restore {
                            self.store.restore_session(id)
                        } else {
                            self.store.archive_session(id)
                        };
                        match outcome {
                            Ok(true) => json!({ "id": id, "ok": true }),
                            Ok(false) => json!({
                                "id": id,
                                "ok": false,
                                "error": "session not found"
                            }),
                            Err(e) => json!({ "id": id, "ok": false, "error": e.to_string() }),
                        }
                    })
                    .collect();
                let succeeded = results.iter().filter(|r| r["ok"] == json!(true)).count();
                HttpResponse::ok(&json!({
                    "results": results,
                    "succeeded": succeeded,
                    "failed": results.len() - succeeded
                }))
            }
            // Tools endpoints
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}
