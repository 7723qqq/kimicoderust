use super::*;

impl HttpServer {
    pub(super) async fn catalog_payload(&self) -> Value {
        match self.models_dev_cache.catalog().await {
            Ok(payload) => payload,
            Err(_) => models_dev::builtin_catalog(),
        }
    }

    pub(super) async fn catalog_providers(&self) -> Value {
        let payload = self.catalog_payload().await;
        match models_dev::parse_catalog(&payload) {
            Ok(catalog) => models_dev::provider_items(&catalog),
            Err(_) => catalog_provider_items(),
        }
    }

    pub(super) async fn catalog_provider(&self, id: &str) -> Option<Value> {
        let payload = self.catalog_payload().await;
        let catalog = models_dev::parse_catalog(&payload).ok()?;
        let entry = catalog.get(id)?;
        Some(models_dev::provider_item(id, entry))
    }

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

#[allow(clippy::too_many_arguments)]
pub(super) fn format_wire_session(
    session: &crate::session::sqlite_store::SessionSummary,
    store: &SqliteSessionStore,
    engine: Option<&Arc<ServerEngine>>,

    default_root: Option<&Path>,
) -> Value {
    let session_id = &session.session_id;
    let busy = engine.map(|e| e.is_busy(session_id)).unwrap_or(false);
    let history = store.load_session_history(session_id).unwrap_or_default();
    let message_count = history.len();

    let context_tokens = engine.map_or_else(
        || history.iter().map(|m| m.content.len() / 4).sum::<usize>(),
        |engine| engine.session_context_tokens(session_id) as usize,
    );

    let pending_interaction = engine
        .and_then(|e| e.interaction_manager())
        .map(|mgr| {
            if !mgr.list_approvals(session_id).is_empty() {
                "approval"
            } else if !mgr.list_questions(session_id).is_empty() {
                "question"
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

    if !metadata_obj.contains_key("cwd")
        && let Some(work_dir) = metadata_obj.get("workDir").and_then(|v| v.as_str())
    {
        metadata_obj.insert("cwd".to_string(), json!(work_dir));
    }
    let legacy_custom = metadata_obj
        .get("isCustomTitle")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || metadata_obj
            .get("customTitle")
            .is_some_and(|v| !v.is_null());
    let title_kind = session
        .normalized_title_kind()
        .map(str::to_string)
        .or_else(|| {
            metadata_obj
                .get("titleKind")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| legacy_custom.then(|| "custom".to_string()));

    let cwd = session
        .cwd
        .clone()
        .or_else(|| {
            metadata_obj
                .get("cwd")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string())
        })
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

    if let Some(kind) = &title_kind {
        metadata_obj.insert("titleKind".to_string(), json!(kind));
    }
    metadata_obj.insert(
        "isCustomTitle".to_string(),
        json!(title_kind.as_deref() == Some("custom")),
    );
    if let Some(prompt) = &session.last_prompt {
        metadata_obj.insert("lastPrompt".to_string(), json!(prompt));
    }
    if let Some(archived_at) = session.archived_at {
        metadata_obj.insert("archivedAt".to_string(), json!(archived_at));
    }
    if let Some(forked_from) = &session.forked_from {
        metadata_obj.insert("forkedFrom".to_string(), json!(forked_from));
    }
    if let Some(agents) = &session.agents {
        metadata_obj.insert("agents".to_string(), agents.clone());
    }
    if let Some(custom) = &session.custom {
        metadata_obj.insert("custom".to_string(), custom.clone());
    }
    if let Some(environment_id) = &session.environment_id {
        metadata_obj.insert("environmentId".to_string(), json!(environment_id));
    }
    if let Some(environment_cwd) = &session.environment_cwd {
        metadata_obj.insert("environmentCwd".to_string(), json!(environment_cwd));
    }
    metadata_obj.insert(
        "version".to_string(),
        json!(
            session
                .version
                .unwrap_or(crate::session::sqlite_store::SESSION_META_VERSION)
        ),
    );

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

    let is_default = default_root
        .map(|root| {
            crate::default_workspace::is_default_root_resolved(
                &ws_id_root(&ws_id, store).unwrap_or_else(|| cwd.clone()),
                root,
            )
        })
        .unwrap_or(false);

    let mut last_turn_reason = store.last_turn_reason(session_id);
    if last_turn_reason.is_none() && message_count > 0 {
        last_turn_reason = store.reconcile_last_turn_reason(session_id).ok().flatten();
    }

    json!({
        "id": session_id,
        "session_id": session_id,
        "workspace_id": ws_id,
        "is_default": is_default,
        "title": session
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(crate::session::sqlite_store::DEFAULT_SESSION_TITLE),
        "title_kind": title_kind,
        "titleKind": title_kind,
        "is_custom_title": title_kind.as_deref() == Some("custom"),
        "isCustomTitle": title_kind.as_deref() == Some("custom"),
        "last_prompt": session.last_prompt,
        "lastPrompt": session.last_prompt,
        "archived_at": session.archived_at,
        "archivedAt": session.archived_at,
        "forked_from": session.forked_from,
        "forkedFrom": session.forked_from,
        "version": session.version.unwrap_or(crate::session::sqlite_store::SESSION_META_VERSION),
        "cwd": cwd,
        "environment_id": session.environment_id,
        "environmentId": session.environment_id,
        "environment_cwd": session.environment_cwd,
        "environmentCwd": session.environment_cwd,
        "agents": session.agents,
        "custom": session.custom,
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





        "last_turn_reason": last_turn_reason
    })
}

fn ws_id_root(ws_id: &str, store: &SqliteSessionStore) -> Option<String> {
    store.get_workspace(ws_id).ok().flatten().map(|w| w.root)
}
