use super::catalog_routes::format_wire_session;
use super::*;

impl HttpServer {
    /// Dispatch an incoming HTTP request to the appropriate route handler.
    pub async fn handle_request(&self, req: &HttpRequest) -> HttpResponse {
        let path = req.path.trim_end_matches('/');
        let method = req.method.to_uppercase();

        // Before the credential: the guard is about *where* the request came
        // from, not who sent it. A rebinding page carries no token of its own,
        // so answering 401 first would only tell it the port is live.
        if !self.host_allowed(req.header("host")) {
            return HttpResponse::forbidden(host_guard::rejection_message(req.header("host")));
        }

        // The one REST authority: health and the schema documents answer
        // unauthenticated, matching kap-server, and everything that can read or
        // mutate state does not.
        if !ServerAuth::is_bypassed(&method, path) {
            let decision = self.auth.check_bearer(req.header("authorization"));
            if !decision.is_allowed() {
                return HttpResponse::unauthorized("Unauthorized");
            }
        }

        // Static file serving and SPA fallback for non-API routes
        if !path.starts_with("/api")
            && (method == "GET" || method == "HEAD")
            && let Some(assets_dir) = &self.web_assets_dir
        {
            return static_files::serve_static_file(assets_dir, &req.path);
        }

        // Debug RPC and reflection surface for kimi-inspect. Not mounted by
        // default: it reflects the server's internals, so it is a test tool
        // that has to be asked for, and only on a loopback bind.
        if self.debug_endpoints_enabled()
            && path.starts_with("/api/v1/debug")
            && let Some(resp) = debug::handle_debug_route(self, req).await
        {
            return resp;
        }

        let resp = self.route_system(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_models_providers(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_prompts_files(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_sessions_v2(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_tools_mcp(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_workspaces_fs(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_plugins_oauth(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_workspaces(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_subagents_gui(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_sessions(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_interaction(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_session_io(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        let resp = self.route_session_ops(&method, path, req).await;
        if !resp.is_route_miss() {
            return resp;
        }
        self.route_prompts(&method, path, req).await
    }

    pub(super) fn handle_session_patch(&self, session_id: &str, body_bytes: &[u8]) -> HttpResponse {
        let Some(session) = self.store.get_session(session_id).ok().flatten() else {
            return HttpResponse::not_found();
        };
        let body: Value = match serde_json::from_slice(body_bytes) {
            Ok(v) => v,
            Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
        };

        let mut current_wire = format_wire_session(&session, &self.store, self.engine.as_ref());

        let patch_set = if let Ok(ps) =
            serde_json::from_value::<crate::session::patch::JsonPatchSet>(body.clone())
        {
            ps
        } else if let Some(arr) = body.as_array() {
            match serde_json::from_value::<Vec<crate::session::patch::PatchOp>>(Value::Array(
                arr.clone(),
            )) {
                Ok(ops) => crate::session::patch::JsonPatchSet::new(ops),
                Err(e) => {
                    return HttpResponse::bad_request(format!(
                        "Invalid RFC 6902 patch operations: {e}"
                    ));
                }
            }
        } else if let Some(ops_val) = body.get("ops").or_else(|| body.get("patch")) {
            match serde_json::from_value::<Vec<crate::session::patch::PatchOp>>(ops_val.clone()) {
                Ok(ops) => crate::session::patch::JsonPatchSet::new(ops),
                Err(e) => {
                    return HttpResponse::bad_request(format!(
                        "Invalid patch operations in ops field: {e}"
                    ));
                }
            }
        } else if body.is_object() {
            let mut target = current_wire.clone();
            if let Some(target_obj) = target.as_object_mut() {
                for (k, v) in body.as_object().unwrap() {
                    if k == "metadata" && v.is_object() {
                        if let Some(meta_obj) = target_obj
                            .get_mut("metadata")
                            .and_then(|m| m.as_object_mut())
                        {
                            for (mk, mv) in v.as_object().unwrap() {
                                meta_obj.insert(mk.clone(), mv.clone());
                            }
                        } else {
                            target_obj.insert(k.clone(), v.clone());
                        }
                    } else {
                        target_obj.insert(k.clone(), v.clone());
                    }
                }
            }
            crate::session::patch::diff_values(&current_wire, &target)
        } else {
            return HttpResponse::bad_request(
                "Invalid patch payload: expected RFC 6902 array or object",
            );
        };

        let inverse_patch = match crate::session::patch::apply_patch(&mut current_wire, &patch_set)
        {
            Ok(inv) => inv,
            Err(e) => return HttpResponse::bad_request(format!("Failed to apply patch: {e}")),
        };

        // Persist the patched fields. A dropped write must not be reported as a
        // successful patch: the client would keep a state the store never took,
        // and the inverse patch (the only way back) would be gone with it.
        let mut persist_failures: Vec<String> = Vec::new();
        if let Some(new_title) = current_wire.get("title").and_then(|t| t.as_str())
            && session.title.as_deref() != Some(new_title)
            && let Err(e) = self.store.update_session_title(session_id, Some(new_title))
        {
            persist_failures.push(format!("session title: {e}"));
        }
        if let Some(new_meta) = current_wire.get("metadata")
            && let Err(e) = self
                .store
                .put_session_state("metadata", session_id, session_id, new_meta)
        {
            persist_failures.push(format!("session metadata: {e}"));
        }
        if let Some(new_cfg) = current_wire.get("agent_config")
            && let Err(e) =
                self.store
                    .put_session_state("agent_config", session_id, session_id, new_cfg)
        {
            persist_failures.push(format!("agent config: {e}"));
        }

        let checkpoint_id = format!("patch-{}", fastrand::u64(..));
        if let Err(e) = self.store.save_checkpoint(
            session_id,
            &checkpoint_id,
            "session_patch",
            &json!({
                "patch": patch_set,
                "inverse": inverse_patch,
            }),
        ) {
            persist_failures.push(format!("patch checkpoint: {e}"));
        }

        if !persist_failures.is_empty() {
            // Nothing is published: no client should observe a state that the
            // store does not hold.
            return HttpResponse::internal_error(format!(
                "Session patch could not be persisted ({}). The stored session does not match the requested patch.",
                persist_failures.join("; ")
            ));
        }

        let updated_event = json!({
            "type": "event.session.updated",
            "sessionId": session_id,
            "patch": patch_set,
            "inverse": inverse_patch,
        });
        self.hub
            .bus_for("global")
            .publish(&crate::events::EngineEvent::Custom(updated_event));

        HttpResponse::ok(&json!({
            "session": current_wire,
            "applied_patch": patch_set,
            "undo_patch": inverse_patch,
        }))
    }

    /// Roll the engine-owned state domains back by `count` turns.
    ///
    /// `StateStoreCallbacks::checkpoint` (`callbacks.rs`) pushes one snapshot
    /// per turn and calls it the durable undo anchor, but only the REPL's
    /// `/undo` ever popped one — the REST route deleted transcript rows and left
    /// `todo` / `plan` describing work the model can no longer see, so the next
    /// turn inherited a stale plan.
    ///
    /// The checkpoint stack is LIFO (`rollback` pops the newest snapshot), so
    /// `count` turns means `count` pops. `Ok(false)` means the stack is already
    /// empty — normal for a session that never checkpointed — so the loop stops
    /// quietly rather than reporting a failure.
    ///
    /// A session with no resolvable workdir is an error rather than a skip: the
    /// caller deletes the transcript rows before calling this, so doing nothing
    /// would leave the two permanently out of sync.
    pub(super) fn rollback_state_for_undo(
        &self,
        session_id: &str,
        count: usize,
    ) -> Result<(), String> {
        let Some(workdir) = fs_routes::resolve_session_workdir(&self.store, session_id) else {
            return Err(format!(
                "session {session_id} has no resolvable working directory, so its state domains cannot be located"
            ));
        };
        let store = crate::storage::StateStore::for_session(&workdir, session_id)
            .or_else(|_| crate::storage::StateStore::for_workspace(&workdir))
            .map_err(|e| format!("state store for {}: {e}", workdir.display()))?;
        for turn in 1..=count {
            match store.rollback() {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(
                        session_id,
                        turn,
                        count,
                        "checkpoint stack exhausted before the requested undo depth"
                    );
                    return Ok(());
                }
                Err(e) => {
                    return Err(format!(
                        "restoring the checkpoint for turn {turn} of {count} failed: {e}"
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn handle_session_patch_undo(&self, session_id: &str) -> HttpResponse {
        let Some(session) = self.store.get_session(session_id).ok().flatten() else {
            return HttpResponse::not_found();
        };
        let Some(checkpoint_data) = self
            .store
            .get_latest_checkpoint(session_id, "session_patch")
            .ok()
            .flatten()
        else {
            return HttpResponse::bad_request("No patch checkpoint available to undo");
        };
        let Some(inverse_val) = checkpoint_data.get("inverse") else {
            return HttpResponse::bad_request("Invalid checkpoint: missing inverse patch");
        };
        let inverse_set: crate::session::patch::JsonPatchSet =
            match serde_json::from_value(inverse_val.clone()) {
                Ok(s) => s,
                Err(e) => {
                    return HttpResponse::internal_error(format!(
                        "Failed to parse inverse patch: {e}"
                    ));
                }
            };

        let mut current_wire = format_wire_session(&session, &self.store, self.engine.as_ref());
        let redo_patch = match crate::session::patch::apply_patch(&mut current_wire, &inverse_set) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(format!("Failed to revert patch: {e}")),
        };

        // Same contract as the forward patch: never report an undo whose
        // restored state was not actually stored.
        let mut persist_failures: Vec<String> = Vec::new();
        if let Some(new_title) = current_wire.get("title").and_then(|t| t.as_str())
            && let Err(e) = self.store.update_session_title(session_id, Some(new_title))
        {
            persist_failures.push(format!("session title: {e}"));
        }
        if let Some(new_meta) = current_wire.get("metadata")
            && let Err(e) = self
                .store
                .put_session_state("metadata", session_id, session_id, new_meta)
        {
            persist_failures.push(format!("session metadata: {e}"));
        }
        if let Some(new_cfg) = current_wire.get("agent_config")
            && let Err(e) =
                self.store
                    .put_session_state("agent_config", session_id, session_id, new_cfg)
        {
            persist_failures.push(format!("agent config: {e}"));
        }
        if !persist_failures.is_empty() {
            return HttpResponse::internal_error(format!(
                "The patch undo could not be persisted ({}). The stored session does not match the returned state.",
                persist_failures.join("; ")
            ));
        }

        let updated_event = json!({
            "type": "event.session.updated",
            "sessionId": session_id,
            "patch": inverse_set,
            "inverse": redo_patch,
        });
        self.hub
            .bus_for("global")
            .publish(&crate::events::EngineEvent::Custom(updated_event));

        HttpResponse::ok(&json!({
            "session": current_wire,
            "applied_patch": inverse_set,
            "redo_patch": redo_patch,
        }))
    }
}
