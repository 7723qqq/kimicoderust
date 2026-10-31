use super::*;

impl HttpServer {
    pub(super) async fn route_subagents_gui(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("GET", "/api/v1/subagents") => {
                let list = self.subagent_manager().list().await;
                HttpResponse::ok(&json!({ "subagents": list }))
            }
            ("GET", p) if extract_session_action(p, "subagents").is_some() => {
                let session_id = extract_session_action(p, "subagents").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let list = self.subagent_manager().list().await;
                HttpResponse::ok(&json!({ "subagents": list }))
            }
            ("GET", p)
                if p.starts_with("/api/v1/subagents/")
                    && !p.ends_with(":kill")
                    && !p.ends_with("/kill") =>
            {
                let subagent_id = &p["/api/v1/subagents/".len()..];
                if let Some(inst) = self.subagent_manager().get_instance(subagent_id).await {
                    HttpResponse::ok(&json!(inst))
                } else {
                    HttpResponse::not_found()
                }
            }
            ("POST", p)
                if p.starts_with("/api/v1/subagents/")
                    && (p.ends_with(":kill") || p.ends_with("/kill")) =>
            {
                let subagent_id = p["/api/v1/subagents/".len()..]
                    .trim_end_matches(":kill")
                    .trim_end_matches("/kill");
                match self.subagent_manager().kill(subagent_id).await {
                    Ok(true) => HttpResponse::ok(&json!({ "killed": true, "id": subagent_id })),
                    Ok(false) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::internal_error(e),
                }
            }

            // GUI store endpoints (mirrors localStorage API for Web UI / Desktop)
            ("GET", "/api/v1/gui/store/getItem") => {
                let key = match req.query_param("key") {
                    Some(k) if !k.is_empty() => k,
                    _ => return HttpResponse::bad_request("Query parameter 'key' is required"),
                };
                match self.store.gui_get_item(&key) {
                    Ok(val) => HttpResponse::ok(&json!({ "value": val })),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("POST", "/api/v1/gui/store/setItem") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let key = match body.get("key").and_then(|v| v.as_str()) {
                    Some(k) if !k.is_empty() => k,
                    _ => return HttpResponse::bad_request("Field 'key' is required"),
                };
                let val = match body.get("value").and_then(|v| v.as_str()) {
                    Some(v) => v,
                    _ => return HttpResponse::bad_request("Field 'value' is required"),
                };
                match self.store.gui_set_item(key, val) {
                    Ok(_) => HttpResponse::ok(&json!(null)),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("POST", "/api/v1/gui/store/removeItem") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let key = match body.get("key").and_then(|v| v.as_str()) {
                    Some(k) if !k.is_empty() => k,
                    _ => return HttpResponse::bad_request("Field 'key' is required"),
                };
                match self.store.gui_remove_item(key) {
                    Ok(_) => HttpResponse::ok(&json!(null)),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("POST", "/api/v1/gui/store/clear") => match self.store.gui_clear() {
                Ok(_) => HttpResponse::ok(&json!(null)),
                Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
            },
            ("GET", "/api/v1/gui/store/length") => match self.store.gui_length() {
                Ok(len) => HttpResponse::ok(&json!({ "length": len })),
                Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
            },

            // Session sub-resources: status, abort, fork, compact, undo, messages, goal, init
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}
