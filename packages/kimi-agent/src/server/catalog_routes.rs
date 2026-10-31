use super::*;

impl HttpServer {
    /// The catalog payload the routes read (v2 `getModelsDevCatalog`): the
    /// TTL-cached upstream fetch, with the built-in snapshot ending the
    /// fallback chain when the fetch fails with no cache to serve.
    pub(super) async fn catalog_payload(&self) -> Value {
        match self.models_dev_cache.catalog().await {
            Ok(payload) => payload,
            Err(_) => models_dev::builtin_catalog(),
        }
    }

    /// `GET /api/v1/catalog/providers` (v2 `listModelsDevProviders`): every
    /// catalog entry mapped to its item.
    pub(super) async fn catalog_providers(&self) -> Value {
        let payload = self.catalog_payload().await;
        match models_dev::parse_catalog(&payload) {
            Ok(catalog) => models_dev::provider_items(&catalog),
            Err(_) => catalog_provider_items(),
        }
    }

    /// `GET /api/v1/catalog/providers/{id}` (v2 `getModelsDevProvider`): the
    /// same entry the list route advertises — including the resolved
    /// `base_url` — or `None` for an unknown id (the route answers 404).
    pub(super) async fn catalog_provider(&self, id: &str) -> Option<Value> {
        let payload = self.catalog_payload().await;
        let catalog = models_dev::parse_catalog(&payload).ok()?;
        let entry = catalog.get(id)?;
        Some(models_dev::provider_item(id, entry))
    }

    /// `POST /api/v1/providers:import_catalog` (v2 `importModelsDevProvider`):
    /// write the chosen catalog entry into the config's `[providers.*]` /
    /// `[models.*]` sections. Re-importing an id rewrites the provider entry
    /// and its aliases from the catalog; an OAuth-managed provider is
    /// rejected instead.
    pub(super) async fn import_catalog_provider(&self, body: &Value) -> HttpResponse {
        let invalid = |msg: String| {
            HttpResponse::json(
                400,
                &json!({ "code": crate::server::envelope::error_codes::CATALOG_IMPORT_INVALID, "msg": msg }),
            )
        };
        let Some(catalog_id) = body
            .get("catalog_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return HttpResponse::json(
                400,
                &json!({ "code": crate::server::envelope::error_codes::VALIDATION_FAILED, "msg": "catalog_id is required for :import_catalog" }),
            );
        };

        let payload = self.catalog_payload().await;
        let Ok(catalog) = models_dev::parse_catalog(&payload) else {
            return HttpResponse::json(
                503,
                &json!({ "code": crate::server::envelope::error_codes::VALIDATION_FAILED, "msg": "catalog unavailable" }),
            );
        };
        let Some(entry) = catalog.get(catalog_id) else {
            return HttpResponse::json(
                404,
                &json!({
                    "code": crate::server::envelope::error_codes::CATALOG_ENTRY_NOT_FOUND,
                    "msg": format!("catalog entry {catalog_id} does not exist"),
                }),
            );
        };

        let user_base_url = body.get("base_url").and_then(Value::as_str);
        let (wire, base_url) = match models_dev::resolve_import(entry, user_base_url) {
            models_dev::ImportResolution::Ok { wire, base_url, .. } => (wire, base_url),
            models_dev::ImportResolution::NeedsBaseUrl { .. } => {
                return invalid(format!("catalog entry {catalog_id} requires a base_url"));
            }
            models_dev::ImportResolution::Invalid { reason } => {
                return invalid(format!(
                    "catalog entry {catalog_id} cannot be imported: {reason}"
                ));
            }
        };

        let models = models_dev::provider_models(entry);
        if models.is_empty() {
            return invalid(format!(
                "catalog entry {catalog_id} has no importable models"
            ));
        }

        let target_id = body
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .unwrap_or(catalog_id);
        if !models_dev::is_provider_id(target_id) {
            return invalid(format!(
                "catalog entry id {target_id} cannot be used as a provider id"
            ));
        }

        // The credential rides on the request (v2's body carries `api_key`;
        // the fork also accepts `api_key_env`). A re-import keeps the stored
        // credential when the request supplies none — v2's
        // `reconcileProviderCredentialUpdate`, without its eager env-existence
        // check (the fork resolves `api_key_env` at request time).
        // The pre-import config reads the same source the write builds on
        // (`provider_write.rs`): the staged override, else the pinned file,
        // else discovery.
        let before = self
            .config_override
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| match self.config_write_path() {
                Some(path) => crate::config::KimiConfig::from_file(&path).unwrap_or_default(),
                None => crate::config::KimiConfig::discover()
                    .map(|(config, _)| config)
                    .unwrap_or_default(),
            });
        let existing = before.providers.get(target_id);
        if existing.is_some_and(|provider| provider.oauth.is_some()) {
            return HttpResponse::json(
                400,
                &json!({
                    "code": crate::server::envelope::error_codes::PROVIDER_OAUTH_MANAGED,
                    "msg": format!("provider {target_id} is managed by OAuth login; use POST /oauth/logout instead"),
                }),
            );
        }
        let api_key = body
            .get("api_key")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| existing.and_then(|provider| provider.api_key.clone()));
        let api_key_env = body
            .get("api_key_env")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| existing.and_then(|provider| provider.api_key_env.clone()));

        let first_alias = format!("{target_id}/{}", models[0].id);
        let updated =
            crate::config::write::update_config(self.config_write_path().as_deref(), |document| {
                crate::config::write::write_provider(
                    document,
                    target_id,
                    &crate::config::write::ProviderWrite {
                        provider_type: wire.clone(),
                        api_key: api_key.clone(),
                        api_key_env: api_key_env.clone(),
                        base_url: base_url.clone(),
                        default_model: None,
                        source: None,
                    },
                )?;
                crate::config::write::remove_model_aliases_of(document, target_id);
                for model in &models {
                    crate::config::write::write_model_alias(
                        document,
                        &models_dev::model_write(target_id, model),
                    )?;
                }
                // v2 seeds the global default from the first imported model
                // only when nothing is configured at all (fresh setup).
                let seeded = document
                    .get("default_model")
                    .and_then(|item| item.as_str())
                    .is_none_or(|model| model.trim().is_empty());
                if seeded {
                    crate::config::write::set_default_model(document, Some(&first_alias));
                }
                Ok(())
            });
        let updated = match updated {
            Ok(config) => config,
            Err(error) => {
                return HttpResponse::json(
                    500,
                    &json!({ "code": crate::server::envelope::error_codes::INTERNAL_ERROR, "msg": error }),
                );
            }
        };
        *self.config_override.lock().await = Some(updated.clone());
        self.publish_config_changed(&["providers", "models"]).await;

        let has_cached_token = |provider: &str| self.has_cached_token(provider);
        let provider =
            crate::server::model_catalog::provider_item(&updated, target_id, &has_cached_token)
                .unwrap_or(Value::Null);
        HttpResponse::json(
            201,
            &json!({ "provider": provider, "models_imported": models.len() }),
        )
    }

    /// `POST /api/v1/providers:import_registry` (v2 `importCustomRegistry`):
    /// import a models.dev-shaped private registry (an `api.json` URL plus an
    /// optional Bearer key) as configured providers. Every listed provider is
    /// written with a `source` record so refreshes rediscover it, and
    /// re-importing the same URL removes providers that disappeared upstream
    /// — the URL is the registry's stable identity.
    pub(super) async fn import_registry_provider(&self, body: &Value) -> HttpResponse {
        let invalid = |msg: String| {
            HttpResponse::json(
                400,
                &json!({ "code": crate::server::envelope::error_codes::VALIDATION_FAILED, "msg": msg }),
            )
        };
        let Some(url) = body
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
        else {
            return invalid("url is required for :import_registry".to_string());
        };

        // The pre-import config reads the same source the write builds on
        // (`provider_write.rs`): the staged override, else the pinned file,
        // else discovery.
        let before = self
            .config_override
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| match self.config_write_path() {
                Some(path) => crate::config::KimiConfig::from_file(&path).unwrap_or_default(),
                None => crate::config::KimiConfig::discover()
                    .map(|(config, _)| config)
                    .unwrap_or_default(),
            });
        // The key: the request's, else the one stored for the same URL (key
        // rotation keeps the URL as the identity), else none.
        let stored_key = before.providers.values().find_map(|provider| {
            custom_registry::RegistrySource::from_provider(provider)
                .filter(|source| source.url == url)
                .map(|source| source.api_key)
        });
        let source = custom_registry::RegistrySource {
            url: url.to_string(),
            api_key: body
                .get("api_key")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(stored_key)
                .unwrap_or_default(),
        };

        let client = match reqwest::Client::builder().build() {
            Ok(client) => client,
            Err(error) => {
                return HttpResponse::json(
                    500,
                    &json!({ "code": crate::server::envelope::error_codes::INTERNAL_ERROR, "msg": error.to_string() }),
                );
            }
        };
        let registry_invalid = |msg: String| {
            HttpResponse::json(
                400,
                &json!({ "code": crate::server::envelope::error_codes::REGISTRY_IMPORT_INVALID, "msg": msg }),
            )
        };
        let entries = match custom_registry::fetch_registry(&client, &source).await {
            Ok(entries) => entries,
            Err(error) => {
                return registry_invalid(format!(
                    "custom registry at {url} cannot be imported: {error}"
                ));
            }
        };
        if entries.is_empty() {
            return registry_invalid(format!(
                "custom registry at {url} has no importable providers"
            ));
        }
        for entry in entries.values() {
            if before
                .providers
                .get(&entry.id)
                .is_some_and(|provider| provider.oauth.is_some())
            {
                return HttpResponse::json(
                    400,
                    &json!({
                        "code": crate::server::envelope::error_codes::PROVIDER_OAUTH_MANAGED,
                        "msg": format!("provider {} is managed by OAuth login; use POST /oauth/logout instead", entry.id),
                    }),
                );
            }
        }

        let had_default = before
            .default_model
            .as_deref()
            .is_some_and(|model| !model.trim().is_empty());
        let updated =
            crate::config::write::update_config(self.config_write_path().as_deref(), |document| {
                custom_registry::apply_entries(document, &entries, &source)?;
                custom_registry::seed_default_when_unset(document, &entries, had_default);
                Ok(())
            });
        let updated = match updated {
            Ok(config) => config,
            Err(error) => {
                return HttpResponse::json(
                    500,
                    &json!({ "code": crate::server::envelope::error_codes::INTERNAL_ERROR, "msg": error }),
                );
            }
        };
        *self.config_override.lock().await = Some(updated.clone());
        self.publish_config_changed(&["providers", "models"]).await;

        let has_cached_token = |provider: &str| self.has_cached_token(provider);
        let providers: Vec<Value> = entries
            .values()
            .filter_map(|entry| {
                crate::server::model_catalog::provider_item(&updated, &entry.id, &has_cached_token)
            })
            .collect();
        HttpResponse::json(
            201,
            &json!({
                "providers": providers,
                "models_imported": custom_registry::model_count(&entries),
                "credential_env": custom_registry::credential_env_hints(&entries),
            }),
        )
    }
}

pub(super) fn format_wire_session(
    session: &crate::session::sqlite_store::SessionSummary,
    store: &SqliteSessionStore,
    engine: Option<&Arc<ServerEngine>>,
) -> Value {
    let session_id = &session.session_id;
    let busy = engine.map(|e| e.is_busy(session_id)).unwrap_or(false);
    let history = store.load_session_history(session_id).unwrap_or_default();
    let message_count = history.len();
    // Measured+estimated with an engine (v2 `statusSize`); the byte/4 floor is
    // only the engine-less fallback (a route test store has no engine).
    let context_tokens = engine.map_or_else(
        || history.iter().map(|m| m.content.len() / 4).sum::<usize>(),
        |engine| engine.session_context_tokens(session_id) as usize,
    );

    // Derive `pending_interaction` from the engine's InteractionManager
    // when an engine is attached: v2 surfaced a real value here instead of
    // the "none" stub the engine previously hardcoded, and the TUI status
    // panel relies on it to render the question / approval state.
    let pending_interaction = engine
        .and_then(|e| e.interaction_manager())
        .map(|mgr| {
            if !mgr.list_questions(session_id).is_empty() {
                "question"
            } else if !mgr.list_approvals(session_id).is_empty() {
                "approval"
            } else {
                "none"
            }
        })
        .unwrap_or("none");

    let model = store
        .get_state("agent_config", session_id)
        .ok()
        .flatten()
        .and_then(|c| {
            c.get("model")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| engine.map(|e| e.model_name().to_string()))
        .unwrap_or_else(|| "kimi-latest".to_string());

    let mut metadata_obj = store
        .get_state("metadata", session_id)
        .ok()
        .flatten()
        .and_then(|m| m.as_object().cloned())
        .unwrap_or_default();

    let cwd = metadata_obj
        .get("cwd")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        });

    metadata_obj
        .entry("cwd".to_string())
        .or_insert_with(|| json!(cwd));
    metadata_obj
        .entry("session_id".to_string())
        .or_insert_with(|| json!(session_id));

    let mut config_obj = store
        .get_state("agent_config", session_id)
        .ok()
        .flatten()
        .and_then(|c| c.as_object().cloned())
        .unwrap_or_default();
    config_obj
        .entry("model".to_string())
        .or_insert_with(|| json!(model));
    config_obj
        .entry("thinking".to_string())
        .or_insert_with(|| json!("medium"));

    let ws_id = session
        .workspace_id
        .clone()
        .unwrap_or_else(|| crate::session::sqlite_store::encode_workdir_key(&cwd));

    let created_iso = chrono::DateTime::from_timestamp_millis(session.created_at)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));

    let updated_iso = chrono::DateTime::from_timestamp_millis(session.updated_at)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));

    json!({
        "id": session_id,
        "session_id": session_id,
        "workspace_id": ws_id,
        "title": session.title.as_deref().unwrap_or(""),
        "created_at": created_iso,
        "updated_at": updated_iso,
        "busy": busy,
        "main_turn_active": busy,
        "pending_interaction": pending_interaction,
        "archived": session.archived,
        "parent_session_id": session.parent_session_id,
        "metadata": Value::Object(metadata_obj),
        "agent_config": Value::Object(config_obj),
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_read_tokens": 0,
            "cache_creation_tokens": 0,
            "total_cost_usd": 0,
            "context_tokens": context_tokens,
            "context_limit": 262144,
            "turn_count": message_count.max(1)
        },
        "permission_rules": [],
        "message_count": message_count,
        "last_seq": store.latest_wire_event_seq(session_id).unwrap_or(0),
        // The engine publishes this on `session.work_changed` as it happens;
        // persisting it as well is what lets a client that attaches later (or
        // after a restart) still see the last outcome. Absent, not null, when a
        // turn has not ended since activation — the wire schema marks it
        // optional and its own doc says it is "when the session is live".
        "last_turn_reason": store.last_turn_reason(session_id)
    })
}
