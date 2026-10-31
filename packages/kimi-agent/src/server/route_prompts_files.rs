use super::*;

impl HttpServer {
    pub(super) async fn route_prompts_files(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("GET", "/api/v1/prompts") => {
                // The live prompt queue is per session; the collection route
                // reports every session's active + queued prompts with the
                // owning session id attached.
                let mut items: Vec<Value> = Vec::new();
                for session in self.store.list_sessions().unwrap_or_default() {
                    let (active, queued) = self.prompt_queue.snapshot(&session.session_id);
                    if let Some(mut active) = active {
                        active["session_id"] = json!(session.session_id);
                        items.push(active);
                    }
                    for mut queued in queued {
                        queued["session_id"] = json!(session.session_id);
                        items.push(queued);
                    }
                }
                HttpResponse::ok(&json!({ "items": items }))
            }
            ("POST", "/api/v1/prompts") => {
                // Collection-level submit: the session path carries the
                // enqueue contract, so this route delegates to it instead of
                // minting an id. Previously it answered `{"status":"enqueued"}`
                // without touching the queue — a prompt that never ran.
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let session_id = match body.get("session_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.trim().is_empty() => id.trim().to_string(),
                    _ => return HttpResponse::bad_request("Field 'session_id' is required"),
                };
                self.submit_session_prompt(&session_id, body).await
            }
            ("GET", "/api/v1/files") => match self.file_store.list() {
                Ok(metas) => {
                    let items: Vec<Value> = metas
                        .iter()
                        .filter_map(|meta| serde_json::to_value(meta).ok())
                        .collect();
                    HttpResponse::ok(&json!({ "files": items }))
                }
                Err((status, code, message)) => {
                    HttpResponse::json(status, &json!({ "code": code, "msg": message }))
                }
            },
            ("POST", "/api/v1/files") => {
                let content_type = req.header("content-type").unwrap_or_default().to_string();
                match crate::server::files::upload(&self.file_store, &content_type, &req.body) {
                    Ok(meta) => HttpResponse::json(200, &meta),
                    Err((status, code, message)) => {
                        HttpResponse::json(status, &json!({ "code": code, "msg": message }))
                    }
                }
            }
            ("GET", p) if p.starts_with("/api/v1/files/") => {
                let file_id = crate::server::router::decode_path_segment(
                    p.trim_start_matches("/api/v1/files/"),
                );
                match self.file_store.get(&file_id) {
                    Ok((meta, path)) => {
                        let bytes = match std::fs::read(&path) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                return HttpResponse::internal_error(format!(
                                    "cannot read {}: {error}",
                                    path.display()
                                ));
                            }
                        };
                        let total = bytes.len() as u64;
                        let response = match req
                            .header("range")
                            .and_then(|header| crate::server::files::parse_range(header, total))
                        {
                            Some((start, end)) => {
                                let slice = bytes[start as usize..=end as usize].to_vec();
                                HttpResponse::bytes(206, meta.media_type.clone(), slice)
                                    .with_header(
                                        "content-range",
                                        format!("bytes {start}-{end}/{total}"),
                                    )
                            }
                            None => HttpResponse::bytes(200, meta.media_type.clone(), bytes),
                        };
                        response
                            .with_header(
                                "content-disposition",
                                crate::server::files::content_disposition(
                                    &meta.name,
                                    &meta.media_type,
                                ),
                            )
                            .with_header("accept-ranges", "bytes")
                            .with_header("etag", format!("\"{}-{total}\"", meta.id))
                    }
                    Err((status, code, message)) => {
                        HttpResponse::json(status, &json!({ "code": code, "msg": message }))
                    }
                }
            }
            ("DELETE", p) if p.starts_with("/api/v1/files/") => {
                let file_id = crate::server::router::decode_path_segment(
                    p.trim_start_matches("/api/v1/files/"),
                );
                match self.file_store.delete(&file_id) {
                    Ok(()) => HttpResponse::ok(&json!({ "deleted": true })),
                    Err((status, code, message)) => {
                        HttpResponse::json(status, &json!({ "code": code, "msg": message }))
                    }
                }
            }
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}
