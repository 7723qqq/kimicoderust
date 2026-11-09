use super::*;

impl HttpServer {
    pub(super) async fn route_models_providers(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            // v2 exposes the model catalog at `GET /api/v1/models`
            // (`kap-server/src/routes/modelCatalog.ts:151-164`).
            ("GET", "/api/v1/models") => {
                let config = self.config().await;
                HttpResponse::ok(&crate::server::model_catalog::models(&config))
            }
            ("GET", "/api/v1/providers") => {
                let config = self.config().await;
                let has_cached_token = |provider: &str| self.has_cached_token(provider);
                HttpResponse::ok(&crate::server::model_catalog::providers(
                    &config,
                    &has_cached_token,
                ))
            }
            ("POST", "/api/v1/providers") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(value) => value,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let form = match serde_json::from_value::<
                    crate::server::provider_write::CreateProviderForm,
                >(body)
                {
                    Ok(form) => form,
                    Err(error) => return provider_validation_error(error.to_string()),
                };
                let has_cached_token = |provider: &str| self.has_cached_token(provider);
                match crate::server::provider_write::create(
                    &self.config_override,
                    self.config_write_path().as_deref(),
                    &has_cached_token,
                    form,
                )
                .await
                {
                    Ok(item) => {
                        self.publish_config_changed(&["providers"]).await;
                        HttpResponse::json(201, &item)
                    }
                    Err((status, code, message)) => {
                        HttpResponse::json(status, &json!({ "code": code, "msg": message }))
                    }
                }
            }
            ("POST", "/api/v1/providers:refresh") => {
                let result = self.refresh_models("all", None).await;
                HttpResponse::ok(&result)
            }
            ("POST", "/api/v1/providers:refresh_oauth") => {
                let result = self.refresh_models("oauth", None).await;
                HttpResponse::ok(&result)
            }
            ("POST", p) if p.starts_with("/api/v1/providers/") && p.ends_with(":refresh") => {
                let provider_id = crate::server::router::decode_path_segment(
                    p.trim_start_matches("/api/v1/providers/")
                        .trim_end_matches(":refresh"),
                );
                let result = self.refresh_models("all", Some(&provider_id)).await;
                HttpResponse::ok(&result)
            }
            ("GET", p)
                if p.starts_with("/api/v1/providers/")
                    && !p.starts_with("/api/v1/providers/catalog") =>
            {
                let provider_id = crate::server::router::decode_path_segment(
                    p.trim_start_matches("/api/v1/providers/"),
                );
                let has_cached_token = |provider: &str| self.has_cached_token(provider);
                match crate::server::provider_write::get(
                    &self.config_override,
                    self.config_write_path().as_deref(),
                    &has_cached_token,
                    &provider_id,
                )
                .await
                {
                    Ok(item) => HttpResponse::ok(&item),
                    Err((status, code, message)) => {
                        HttpResponse::json(status, &json!({ "code": code, "msg": message }))
                    }
                }
            }
            ("PUT", p) if p.starts_with("/api/v1/providers/") => {
                let provider_id = crate::server::router::decode_path_segment(
                    p.trim_start_matches("/api/v1/providers/"),
                );
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(value) => value,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let form = match serde_json::from_value::<
                    crate::server::provider_write::ReplaceProviderForm,
                >(body)
                {
                    Ok(form) => form,
                    Err(error) => return provider_validation_error(error.to_string()),
                };
                let has_cached_token = |provider: &str| self.has_cached_token(provider);
                match crate::server::provider_write::replace(
                    &self.config_override,
                    self.config_write_path().as_deref(),
                    &has_cached_token,
                    &provider_id,
                    form,
                )
                .await
                {
                    Ok(item) => {
                        self.publish_config_changed(&["providers"]).await;
                        HttpResponse::ok(&item)
                    }
                    Err((status, code, message)) => {
                        HttpResponse::json(status, &json!({ "code": code, "msg": message }))
                    }
                }
            }
            ("DELETE", p) if p.starts_with("/api/v1/providers/") => {
                let provider_id = crate::server::router::decode_path_segment(
                    p.trim_start_matches("/api/v1/providers/"),
                );
                match crate::server::provider_write::delete(
                    &self.config_override,
                    self.config_write_path().as_deref(),
                    &provider_id,
                )
                .await
                {
                    Ok(item) => {
                        self.publish_config_changed(&["providers"]).await;
                        HttpResponse::ok(&item)
                    }
                    Err((status, code, message)) => {
                        HttpResponse::json(status, &json!({ "code": code, "msg": message }))
                    }
                }
            }
            ("GET", "/api/v1/catalog/providers") | ("GET", "/api/v1/providers/catalog") => {
                HttpResponse::ok(&self.catalog_providers().await)
            }
            ("GET", p) if p.starts_with("/api/v1/catalog/providers/") => {
                let catalog_id = p
                    .strip_prefix("/api/v1/catalog/providers/")
                    .unwrap_or_default();
                // Serve the same entry the list route advertises — including the
                // resolved `base_url` — instead of fabricating a placeholder
                // with no endpoint and no models (v2 #3909 exposes `base_url` on
                // both routes). An unknown id is a 404 rather than a fake entry.
                match self.catalog_provider(catalog_id).await {
                    Some(item) => HttpResponse::ok(&item),
                    None => HttpResponse::not_found(),
                }
            }
            // v2 `importModelsDevProvider`: write the chosen catalog entry
            // into the config's providers / models sections.
            ("POST", "/api/v1/providers:import_catalog") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(value) => value,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                self.import_catalog_provider(&body).await
            }
            // v2 `importCustomRegistry`: import a models.dev-shaped private
            // registry as configured providers.
            ("POST", "/api/v1/providers:import_registry") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(value) => value,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                self.import_registry_provider(&body).await
            }
            ("POST", p) if p.starts_with("/api/v1/models/") => {
                let tail = p.strip_prefix("/api/v1/models/").unwrap_or_default();
                let model_id = if let Some(m) = tail.strip_suffix(":set_default") {
                    m.to_string()
                } else if tail == "set_default" {
                    let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
                    body.get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("kimi-latest")
                        .to_string()
                } else {
                    tail.to_string()
                };
                let mut config = self
                    .config_override
                    .lock()
                    .await
                    .clone()
                    .unwrap_or_else(|| {
                        crate::config::KimiConfig::discover()
                            .map(|(c, _)| c)
                            .unwrap_or_default()
                    });
                config.default_model = Some(model_id.clone());
                *self.config_override.lock().await = Some(config);
                HttpResponse::ok(&json!({ "model": model_id }))
            }
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}
