use super::*;

impl HttpServer {
    pub(super) async fn route_interaction(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("GET", p) if extract_session_action(p, "questions").is_some() => {
                let session_id = extract_session_action(p, "questions").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let items = self.interaction_manager.list_questions(session_id);
                HttpResponse::ok(&json!({
                    "sessionId": session_id,
                    "items": items,
                }))
            }
            ("POST", p) if p.starts_with("/api/v1/sessions/") && p.contains("/questions/") => {
                let parts: Vec<&str> = p.split("/questions/").collect();
                if parts.len() != 2 {
                    return HttpResponse::not_found();
                }
                let session_prefix = parts[0];
                let tail = parts[1];
                let session_id = session_prefix
                    .strip_prefix("/api/v1/sessions/")
                    .unwrap_or_default();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let (qid, is_dismiss) = if let Some(q) = tail.strip_suffix(":dismiss") {
                    (q, true)
                } else if let Some(q) = tail.strip_suffix("/dismiss") {
                    (q, true)
                } else if let Some(q) = tail
                    .strip_suffix(":resolve")
                    .or_else(|| tail.strip_suffix(":reply"))
                {
                    (q, false)
                } else if let Some(q) = tail
                    .strip_suffix("/resolve")
                    .or_else(|| tail.strip_suffix("/reply"))
                {
                    (q, false)
                } else {
                    (tail, false)
                };

                if is_dismiss {
                    if self.interaction_manager.dismiss_question(qid) {
                        HttpResponse::ok(&json!({
                            "dismissed": true,
                            "questionId": qid,
                            "sessionId": session_id
                        }))
                    } else {
                        HttpResponse::not_found()
                    }
                } else {
                    let body: Value = match serde_json::from_slice(&req.body) {
                        Ok(v) => v,
                        Err(e) => {
                            return HttpResponse::bad_request(format!(
                                "Invalid JSON payload for question resolution: {e}"
                            ));
                        }
                    };
                    let mut answers = std::collections::HashMap::new();
                    if let Some(map) = body.get("answers").and_then(|v| v.as_object()) {
                        for (k, v) in map {
                            let val_str = if let Some(s) = v.as_str() {
                                s.to_string()
                            } else if let Some(opt_id) = v.get("option_id").and_then(|o| o.as_str())
                            {
                                opt_id.to_string()
                            } else {
                                v.to_string()
                            };
                            answers.insert(k.clone(), val_str);
                        }
                    } else if let Some(map) = body.as_object() {
                        for (k, v) in map {
                            if k != "method" && k != "note" {
                                let val_str = v
                                    .as_str()
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| v.to_string());
                                answers.insert(k.clone(), val_str);
                            }
                        }
                    }
                    let method = body
                        .get("method")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    if self
                        .interaction_manager
                        .resolve_question(qid, answers, method)
                    {
                        HttpResponse::ok(&json!({
                            "resolved": true,
                            "questionId": qid,
                            "sessionId": session_id
                        }))
                    } else {
                        HttpResponse::not_found()
                    }
                }
            }
            ("GET", p) if extract_session_action(p, "approvals").is_some() => {
                let session_id = extract_session_action(p, "approvals").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let items = self.interaction_manager.list_approvals(session_id);
                HttpResponse::ok(&json!({
                    "sessionId": session_id,
                    "items": items,
                }))
            }
            ("POST", p) if p.starts_with("/api/v1/sessions/") && p.contains("/approvals/") => {
                let parts: Vec<&str> = p.split("/approvals/").collect();
                if parts.len() != 2 {
                    return HttpResponse::not_found();
                }
                let session_prefix = parts[0];
                let tail = parts[1];
                let session_id = session_prefix
                    .strip_prefix("/api/v1/sessions/")
                    .unwrap_or_default();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let aid = if let Some(a) = tail.strip_suffix(":resolve") {
                    a
                } else if let Some(a) = tail.strip_suffix("/resolve") {
                    a
                } else {
                    tail
                };

                // Fail closed: an approval resolution must name its decision
                // explicitly. Defaulting a missing/!parsable body to "approved"
                // let any client that could reach this route grant a dangerous
                // tool by POSTing an empty body.
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(e) => {
                        return HttpResponse::bad_request(format!(
                            "Invalid JSON payload for approval resolution: {e}"
                        ));
                    }
                };
                let decision_str = match body.get("decision").and_then(|v| v.as_str()) {
                    Some(v) => v,
                    None => {
                        return HttpResponse::bad_request(
                            "Missing required 'decision' field (expected \"approved\" or \"denied\")",
                        );
                    }
                };
                let allowed = decision_str.eq_ignore_ascii_case("approved")
                    || decision_str.eq_ignore_ascii_case("allow");
                let reason = body
                    .get("feedback")
                    .or_else(|| body.get("reason"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                if self
                    .interaction_manager
                    .resolve_approval(aid, allowed, reason)
                {
                    HttpResponse::ok(&json!({
                        "resolved": true,
                        "approvalId": aid,
                        "sessionId": session_id,
                        "decision": decision_str
                    }))
                } else {
                    HttpResponse::not_found()
                }
            }
            // Two verbs, two contracts. The official Web bundle posts through a
            // bespoke transport that hard-fails unless the response is
            // `application/zip`; the in-tree REST clients read the export
            // document as JSON over GET.
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}
