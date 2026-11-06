//! Provider create / replace / delete / get, porting kap-server's
//! `protocol/rest-modelCatalog.ts` write half (the write routes live in
//! `routes/modelCatalog.ts`; the schemas they validate against are the ones
//! re-exported from `protocol/`).
//!
//! Writes mutate `config.toml` through [`crate::config::write`] (toml_edit,
//! format-preserving) and refresh the server's cached config, so the next
//! `/models` / `/providers` read reflects the change. `default_provider` and
//! `default_model` are only migrated on a rename; plain create `default_model`
//! seeding happens once, when no default is set.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::config::KimiConfig;
use crate::config::write::{
    ModelAliasWrite, ProviderWrite, remove_model_aliases_of, remove_provider, set_default_model,
    set_default_provider, update_config, write_model_alias, write_provider,
};
use crate::server::envelope::error_codes;
use crate::server::model_catalog::{self, non_empty};
use crate::server::models_dev;

/// One protocol error: HTTP status, kap-server code, message.
pub type WriteError = (u16, u32, String);

/// `providerWireTypeSchema` (protocol/rest-modelCatalog.ts:24-31) — the six
/// values, in the same order.
const WIRE_TYPES: [&str; 6] = [
    "kimi",
    "openai",
    "openai_responses",
    "anthropic",
    "google-genai",
    "vertexai",
];

fn validation(message: impl Into<String>) -> WriteError {
    (400, error_codes::VALIDATION_FAILED, message.into())
}

fn not_found(provider_id: &str) -> WriteError {
    (
        404,
        error_codes::PROVIDER_NOT_FOUND,
        format!("provider {provider_id} does not exist"),
    )
}

fn oauth_managed(provider_id: &str) -> WriteError {
    (
        400,
        error_codes::PROVIDER_OAUTH_MANAGED,
        format!("provider {provider_id} is managed by OAuth login; use POST /oauth/logout instead"),
    )
}

fn already_exists(provider_id: &str) -> WriteError {
    (
        409,
        error_codes::PROVIDER_ALREADY_EXISTS,
        format!("provider {provider_id} already exists"),
    )
}

fn persistence(error: String) -> WriteError {
    (500, error_codes::INTERNAL_ERROR, error)
}

/// One `models[]` entry (`createProviderModelSchema`).
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderModelForm {
    pub model: String,
    #[serde(rename = "max_context_size")]
    pub max_context_size: u32,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(rename = "max_output_size", default)]
    pub max_output_size: Option<u32>,
    #[serde(rename = "support_efforts", default)]
    pub support_efforts: Option<Vec<String>>,
    #[serde(rename = "adaptive_thinking", default)]
    pub adaptive_thinking: Option<bool>,
}

/// `POST /providers` body (`createProviderRequestSchema`).
#[derive(Debug, Clone, Deserialize)]
pub struct CreateProviderForm {
    pub id: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Name of an environment variable to read the key from instead of storing
    /// it inline (kap-server `routes/modelCatalog.ts:239`); mutually exclusive
    /// with `api_key`. Used to hardcode `None` here, which silently dropped
    /// the field clients sent.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    pub models: Vec<ProviderModelForm>,
}

/// `PUT /providers/{id}` body (`replaceProviderRequestSchema`).
#[derive(Debug, Clone, Deserialize)]
pub struct ReplaceProviderForm {
    #[serde(default)]
    pub new_id: Option<String>,
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Tri-state like `api_key` (kap-server `routes/modelCatalog.ts:328`):
    /// omitted keeps the stored name, `""` clears it, any other value replaces
    /// it; mutually exclusive with `api_key`.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    pub models: Vec<ProviderModelForm>,
}

/// `api_key` and `api_key_env` are mutually exclusive (kap-server
/// `routes/modelCatalog.ts:239,328`): submitting both non-empty fails
/// validation. Empty strings are the replace route's clear signals, not a
/// conflict.
fn validate_credentials(
    api_key: Option<&str>,
    api_key_env: Option<&str>,
) -> Result<(), WriteError> {
    if non_empty(api_key).is_some() && non_empty(api_key_env).is_some() {
        return Err(validation("api_key and api_key_env are mutually exclusive"));
    }
    Ok(())
}

/// Engine-generated OAuth provider ids carry this prefix
/// (`managed:kimi-code`, provisioned by the login flow — never submitted
/// through these routes). A user-submitted id may not claim it; the managed
/// entry itself is unaffected because config loads never validate ids.
const MANAGED_PROVIDER_PREFIX: &str = "managed:";

fn validate_provider_id(provider_id: &str) -> Result<(), WriteError> {
    if provider_id.starts_with(MANAGED_PROVIDER_PREFIX) {
        return Err(validation(
            "id must not start with \"managed:\": the prefix is reserved for OAuth-managed providers",
        ));
    }
    // v2 `PROVIDER_ID_PATTERN` (`^[\p{L}\p{N}][\p{L}\p{N}\-_ ]*$`) — the same
    // predicate the catalog-import route applies (`models_dev::is_provider_id`).
    if models_dev::is_provider_id(provider_id) {
        Ok(())
    } else {
        Err(validation(
            "id must start with a letter or digit and may only contain letters, digits, \"-\", \"_\" and spaces",
        ))
    }
}

fn validate_form(
    provider_type: &str,
    models: &[ProviderModelForm],
    default_model: Option<&str>,
    base_url: Option<&str>,
) -> Result<(), WriteError> {
    if !WIRE_TYPES.contains(&provider_type) {
        return Err(validation(format!(
            "type must be one of: {}",
            WIRE_TYPES.join(", ")
        )));
    }
    if let Some(base_url) = base_url
        && base_url.contains("${")
    {
        return Err(validation(
            "base_url must not contain an environment variable placeholder",
        ));
    }
    if models.is_empty() {
        return Err(validation("models must contain at least one entry"));
    }
    let mut seen = HashSet::new();
    for entry in models {
        if entry.model.trim().is_empty() {
            return Err(validation("model must be a non-empty string"));
        }
        if entry.max_context_size == 0 {
            return Err(validation(format!(
                "max_context_size must be at least 1 for model {}",
                entry.model
            )));
        }
        if !seen.insert(entry.model.as_str()) {
            return Err(validation(format!("duplicate model: {}", entry.model)));
        }
    }
    if let Some(default_model) = default_model
        && !models.iter().any(|entry| entry.model == default_model)
    {
        return Err(validation("default_model must be one of models[].model"));
    }
    Ok(())
}

fn alias_write(provider_id: &str, entry: &ProviderModelForm) -> ModelAliasWrite {
    ModelAliasWrite {
        alias_id: format!("{provider_id}/{}", entry.model),
        provider: provider_id.to_string(),
        model: entry.model.clone(),
        max_context_size: entry.max_context_size,
        display_name: entry.display_name.clone(),
        capabilities: entry.capabilities.clone(),
        max_output_size: entry.max_output_size,
        max_input_size: None,
        support_efforts: entry.support_efforts.clone(),
        default_effort: None,
        adaptive_thinking: entry.adaptive_thinking,
        protocol: None,
        beta_api: None,
        reasoning_key: None,
        off_effort: None,
        base_url: None,
    }
}

/// The config the next write builds on: the file being written when the
/// server pinned one, otherwise the discovered file.
fn effective_config(config_path: Option<&std::path::Path>) -> KimiConfig {
    match config_path {
        Some(path) => KimiConfig::from_file(path).unwrap_or_default(),
        None => KimiConfig::discover()
            .map(|(config, _)| config)
            .unwrap_or_default(),
    }
}

/// `POST /providers`: create the provider, write its model aliases, and seed
/// the global `default_model` when none is set yet.
pub async fn create(
    config_lock: &Mutex<Option<KimiConfig>>,
    config_path: Option<&std::path::Path>,
    has_cached_token: &(dyn Fn(&str) -> bool + Sync),
    form: CreateProviderForm,
) -> Result<Value, WriteError> {
    validate_provider_id(&form.id)?;
    validate_form(
        &form.provider_type,
        &form.models,
        form.default_model.as_deref(),
        form.base_url.as_deref(),
    )?;
    validate_credentials(form.api_key.as_deref(), form.api_key_env.as_deref())?;

    let mut guard = config_lock.lock().await;
    let before = guard
        .clone()
        .unwrap_or_else(|| effective_config(config_path));
    if before.providers.contains_key(&form.id) {
        return Err(already_exists(&form.id));
    }

    let default_model = form
        .default_model
        .as_ref()
        .map(|model| format!("{}/{}", form.id, model));
    let seed_default = non_empty(before.default_model.as_deref()).is_none();
    let seed_alias = default_model.clone().or_else(|| {
        form.models
            .first()
            .map(|entry| format!("{}/{}", form.id, entry.model))
    });

    let updated = update_config(config_path, |document| {
        write_provider(
            document,
            &form.id,
            &ProviderWrite {
                provider_type: form.provider_type.clone(),
                // Empty strings are meaningless on create: the provider
                // carries neither credential key (mirrors `import_catalog`'s
                // intake, and the replace route's `""`-clears semantics).
                api_key: non_empty(form.api_key.as_deref()).map(str::to_string),
                api_key_env: non_empty(form.api_key_env.as_deref()).map(str::to_string),
                base_url: form.base_url.clone(),
                default_model: default_model.clone(),
                source: None,
            },
        )?;
        for entry in &form.models {
            write_model_alias(document, &alias_write(&form.id, entry))?;
        }
        if seed_default && let Some(alias) = &seed_alias {
            set_default_model(document, Some(alias));
        }
        Ok(())
    })
    .map_err(persistence)?;
    *guard = Some(updated.clone());
    Ok(model_catalog::provider_item(&updated, &form.id, has_cached_token).unwrap_or(Value::Null))
}

/// `PUT /providers/{id}`: replace the provider and rebuild its aliases,
/// optionally renaming it (migrating `default_provider` / `default_model`).
pub async fn replace(
    config_lock: &Mutex<Option<KimiConfig>>,
    config_path: Option<&std::path::Path>,
    has_cached_token: &(dyn Fn(&str) -> bool + Sync),
    provider_id: &str,
    form: ReplaceProviderForm,
) -> Result<Value, WriteError> {
    validate_provider_id(provider_id)?;
    let new_id = form
        .new_id
        .clone()
        .unwrap_or_else(|| provider_id.to_string());
    validate_provider_id(&new_id)?;
    validate_form(
        &form.provider_type,
        &form.models,
        form.default_model.as_deref(),
        form.base_url.as_deref(),
    )?;
    validate_credentials(form.api_key.as_deref(), form.api_key_env.as_deref())?;

    let mut guard = config_lock.lock().await;
    let before = guard
        .clone()
        .unwrap_or_else(|| effective_config(config_path));
    let Some(target) = before.providers.get(provider_id) else {
        return Err(not_found(provider_id));
    };
    if target.oauth.is_some() {
        return Err(oauth_managed(provider_id));
    }
    let renamed = new_id != provider_id;
    if renamed && before.providers.contains_key(&new_id) {
        return Err(already_exists(&new_id));
    }

    // Aliases the new provider would take over must already belong to it.
    let new_aliases: Vec<String> = form
        .models
        .iter()
        .map(|entry| format!("{new_id}/{}", entry.model))
        .collect();
    let colliding: Vec<&str> = before
        .models
        .iter()
        .filter(|(alias_id, record)| {
            new_aliases.contains(alias_id) && record.provider.as_deref() != Some(provider_id)
        })
        .map(|(alias_id, _)| alias_id.as_str())
        .collect();
    if !colliding.is_empty() {
        return Err(validation(format!(
            "model alias key already owned by another provider: {}",
            colliding.join(", ")
        )));
    }

    // Rename migration for the global pointers (only when they point here).
    let default_provider_hit =
        renamed && non_empty(before.default_provider.as_deref()) == Some(provider_id);
    let renamed_default_model = if renamed {
        non_empty(before.default_model.as_deref())
            .and_then(|alias_id| before.models.get(alias_id))
            .filter(|record| record.provider.as_deref() == Some(provider_id))
            .and_then(|record| record.model.clone())
            .map(|model| format!("{new_id}/{model}"))
            .filter(|alias| new_aliases.contains(alias))
    } else {
        None
    };

    // `api_key` is tri-state (kap-server `routes/modelCatalog.ts:328`):
    // omitted keeps the stored key, `""` clears it, any other value replaces
    // it.
    let api_key = match form.api_key.as_deref() {
        Some("") => None,
        Some(key) => Some(key.to_string()),
        None => target.api_key.clone(),
    };
    // `api_key_env` follows the same tri-state: omitted keeps the stored
    // name, `""` clears it, any other value replaces it. Setting a fresh env
    // name clears the stored inline key, and submitting a fresh inline key
    // clears the stored env name — the two are mutually exclusive.
    let api_key_env = match form.api_key_env.as_deref() {
        Some("") => None,
        Some(env) => Some(env.to_string()),
        None => {
            if non_empty(form.api_key.as_deref()).is_some() {
                None
            } else {
                target.api_key_env.clone()
            }
        }
    };
    let api_key = if api_key_env.is_some() { None } else { api_key };
    let default_model = form
        .default_model
        .as_ref()
        .map(|model| format!("{new_id}/{model}"));

    let updated = update_config(config_path, |document| {
        if renamed {
            remove_provider(document, provider_id);
        }
        remove_model_aliases_of(document, provider_id);
        write_provider(
            document,
            &new_id,
            &ProviderWrite {
                provider_type: form.provider_type.clone(),
                api_key: api_key.clone(),
                api_key_env: api_key_env.clone(),
                base_url: form.base_url.clone(),
                default_model: default_model.clone(),
                source: None,
            },
        )?;
        for entry in &form.models {
            write_model_alias(document, &alias_write(&new_id, entry))?;
        }
        if default_provider_hit {
            set_default_provider(document, &new_id);
        }
        if let Some(alias) = &renamed_default_model {
            set_default_model(document, Some(alias));
        }
        Ok(())
    })
    .map_err(persistence)?;
    *guard = Some(updated.clone());
    Ok(json!({
        "provider": model_catalog::provider_item(&updated, &new_id, has_cached_token)
            .unwrap_or(Value::Null)
    }))
}

/// `DELETE /providers/{id}`: drop the provider and its aliases; the global
/// default pointers are the user's settings and stay untouched.
pub async fn delete(
    config_lock: &Mutex<Option<KimiConfig>>,
    config_path: Option<&std::path::Path>,
    provider_id: &str,
) -> Result<Value, WriteError> {
    let mut guard = config_lock.lock().await;
    let before = guard
        .clone()
        .unwrap_or_else(|| effective_config(config_path));
    let Some(target) = before.providers.get(provider_id) else {
        return Err(not_found(provider_id));
    };
    if target.oauth.is_some() {
        return Err(oauth_managed(provider_id));
    }
    let updated = update_config(config_path, |document| {
        remove_provider(document, provider_id);
        remove_model_aliases_of(document, provider_id);
        Ok(())
    })
    .map_err(persistence)?;
    *guard = Some(updated);
    Ok(json!({ "deleted": true }))
}

/// `GET /providers/{id}`: unlike the list route, the stored `api_key` is
/// revealed so clients can prefill an edit form.
pub async fn get(
    config_lock: &Mutex<Option<KimiConfig>>,
    config_path: Option<&std::path::Path>,
    has_cached_token: &(dyn Fn(&str) -> bool + Sync),
    provider_id: &str,
) -> Result<Value, WriteError> {
    let config = config_lock
        .lock()
        .await
        .clone()
        .unwrap_or_else(|| effective_config(config_path));
    let Some(mut item) = model_catalog::provider_item(&config, provider_id, has_cached_token)
    else {
        return Err(not_found(provider_id));
    };
    if let Some(api_key) = config
        .providers
        .get(provider_id)
        .and_then(|provider| non_empty(provider.api_key.as_deref()))
    {
        item["api_key"] = json!(api_key);
    }
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(model: &str, max_context_size: u32) -> ProviderModelForm {
        ProviderModelForm {
            model: model.into(),
            max_context_size,
            display_name: None,
            capabilities: None,
            max_output_size: None,
            support_efforts: None,
            adaptive_thinking: None,
        }
    }

    fn create_form() -> CreateProviderForm {
        CreateProviderForm {
            id: "kimi-code".into(),
            provider_type: "openai".into(),
            api_key: Some("sk-test".into()),
            api_key_env: None,
            base_url: Some("https://example.test/v1".into()),
            default_model: Some("k3".into()),
            models: vec![model("k3", 200_000), model("fast", 128_000)],
        }
    }

    #[test]
    fn validation_rejects_malformed_forms() {
        let mut bad_id = create_form();
        bad_id.id = "has space!".into();
        assert_eq!(
            validate_provider_id(&bad_id.id).unwrap_err().1,
            error_codes::VALIDATION_FAILED
        );

        let mut bad_type = create_form();
        bad_type.provider_type = "exotic".into();
        assert!(validate_form("exotic", &bad_type.models, None, None).is_err());

        assert!(validate_form("openai", &[], None, None).is_err());
        assert!(validate_form("openai", &[model("a", 0)], None, None).is_err());
        assert!(
            validate_form("openai", &[model("a", 1000), model("a", 1000)], None, None).is_err()
        );
        assert!(
            validate_form("openai", &[model("a", 1000)], Some("missing"), None).is_err(),
            "default_model must be one of models"
        );
        assert!(
            validate_form(
                "openai",
                &[model("a", 1000)],
                None,
                Some("https://${HOST}/v1")
            )
            .is_err()
        );
        assert!(validate_form("openai", &[model("a", 1000)], Some("a"), None).is_ok());
    }

    // ── provider id rules (v2 `PROVIDER_ID_PATTERN` + the managed prefix) ────

    #[test]
    fn provider_id_follows_the_upstream_pattern() {
        for valid in [
            "kimi",               // letters
            "1st",                // a digit may lead
            "MiniMax Token Plan", // spaces inside
            "阶跃",               // unicode letters
            "自定义",
            "美团longcat", // mixed unicode + ascii
            "my-provider_1 x",
        ] {
            assert!(
                validate_provider_id(valid).is_ok(),
                "{valid} must be accepted"
            );
        }
        for invalid in [
            "",               // empty
            "   ",            // whitespace only
            "-leading-dash",  // dash may not lead
            "_leading",       // underscore may not lead
            " leading-space", // space may not lead
            "has:colon",
            "line\nbreak",
            "tab\there",
            "has space!",
        ] {
            let Err((status, code, _)) = validate_provider_id(invalid) else {
                panic!("{invalid:?} must be rejected");
            };
            assert_eq!((status, code), (400, error_codes::VALIDATION_FAILED));
        }
    }

    #[test]
    fn user_submitted_ids_may_not_claim_the_managed_prefix() {
        for id in ["managed:kimi-code", "managed:anything"] {
            let (status, code, message) = validate_provider_id(id).unwrap_err();
            assert_eq!((status, code), (400, error_codes::VALIDATION_FAILED));
            assert!(
                message.contains("reserved for OAuth-managed providers"),
                "{message}"
            );
        }
    }

    #[tokio::test]
    async fn create_rejects_the_managed_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let lock = Mutex::new(None);
        let mut form = create_form();
        form.id = "managed:evil".into();
        let (status, code, message) = create(&lock, Some(&path), &no_cached_token(), form)
            .await
            .unwrap_err();
        assert_eq!((status, code), (400, error_codes::VALIDATION_FAILED));
        assert!(
            message.contains("reserved for OAuth-managed providers"),
            "{message}"
        );
        assert!(!path.exists(), "a rejected create writes nothing");
    }

    #[test]
    fn the_persisted_managed_provider_and_looser_user_ids_still_load() {
        // The OAuth provisioning flow writes `managed:kimi-code` (a colon —
        // outside the user-input pattern), and user ids the old ASCII rule
        // rejected are persisted in the wild. Config loads never validate
        // provider ids, so both keep loading.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[providers.\"managed:kimi-code\"]\ntype = \"kimi\"\n\n\
             [providers.\"managed:kimi-code\".oauth]\nkey = \"oauth/kimi-code\"\n\n\
             [providers.\"阶跃\"]\ntype = \"anthropic\"\n\n\
             [providers.\"MiniMax Token Plan\"]\ntype = \"anthropic\"\n",
        )
        .unwrap();
        let config = crate::config::KimiConfig::from_file(&path).unwrap();
        let managed = config.providers.get("managed:kimi-code").unwrap();
        assert!(managed.oauth.is_some());
        assert!(config.providers.contains_key("阶跃"));
        assert!(config.providers.contains_key("MiniMax Token Plan"));
    }

    // ── `api_key_env` modeling and write-through (kap-server
    // `routes/modelCatalog.ts:239,328`) ────────────────────────────────────

    fn no_cached_token() -> impl Fn(&str) -> bool + Sync {
        |_| false
    }

    fn create_env_form() -> CreateProviderForm {
        let mut form = create_form();
        form.api_key = None;
        form.api_key_env = Some("KIMI_TEST_ENV_KEY".into());
        form
    }

    fn replace_form() -> ReplaceProviderForm {
        ReplaceProviderForm {
            new_id: None,
            provider_type: "openai".into(),
            api_key: None,
            api_key_env: None,
            base_url: Some("https://example.test/v1".into()),
            default_model: Some("k3".into()),
            models: vec![model("k3", 200_000)],
        }
    }

    fn read_back(path: &std::path::Path) -> crate::config::KimiConfig {
        crate::config::KimiConfig::from_file(path).expect("the written config parses")
    }

    #[tokio::test]
    async fn create_persists_api_key_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let lock = Mutex::new(None);
        let form = create_env_form();
        create(&lock, Some(&path), &no_cached_token(), form)
            .await
            .unwrap();

        let written = read_back(&path);
        let provider = written.providers.get("kimi-code").unwrap();
        assert_eq!(provider.api_key_env.as_deref(), Some("KIMI_TEST_ENV_KEY"));
        assert_eq!(provider.api_key, None, "mutually exclusive: no inline key");
    }

    #[tokio::test]
    async fn create_rejects_api_key_and_env_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let lock = Mutex::new(None);
        let mut form = create_env_form();
        form.api_key = Some("sk-inline".into());
        let (status, code, _) = create(&lock, Some(&path), &no_cached_token(), form)
            .await
            .unwrap_err();
        assert_eq!((status, code), (400, error_codes::VALIDATION_FAILED));
    }

    #[tokio::test]
    async fn replace_keeps_the_stored_env_name_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let lock = Mutex::new(None);
        create(&lock, Some(&path), &no_cached_token(), create_env_form())
            .await
            .unwrap();

        replace(
            &lock,
            Some(&path),
            &no_cached_token(),
            "kimi-code",
            replace_form(),
        )
        .await
        .unwrap();

        let written = read_back(&path);
        let provider = written.providers.get("kimi-code").unwrap();
        assert_eq!(
            provider.api_key_env.as_deref(),
            Some("KIMI_TEST_ENV_KEY"),
            "an omitted api_key_env keeps the stored name"
        );
    }

    #[tokio::test]
    async fn replace_clears_the_inline_key_when_a_fresh_env_name_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let lock = Mutex::new(None);
        create(&lock, Some(&path), &no_cached_token(), create_form())
            .await
            .unwrap();

        let mut form = replace_form();
        form.api_key_env = Some("KIMI_FRESH_ENV".into());
        replace(&lock, Some(&path), &no_cached_token(), "kimi-code", form)
            .await
            .unwrap();

        let written = read_back(&path);
        let provider = written.providers.get("kimi-code").unwrap();
        assert_eq!(provider.api_key_env.as_deref(), Some("KIMI_FRESH_ENV"));
        assert_eq!(
            provider.api_key, None,
            "setting a fresh env name clears the stored inline key"
        );
    }

    #[tokio::test]
    async fn replace_clears_the_env_name_with_an_empty_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let lock = Mutex::new(None);
        create(&lock, Some(&path), &no_cached_token(), create_env_form())
            .await
            .unwrap();

        let mut form = replace_form();
        form.api_key_env = Some(String::new());
        replace(&lock, Some(&path), &no_cached_token(), "kimi-code", form)
            .await
            .unwrap();

        let written = read_back(&path);
        let provider = written.providers.get("kimi-code").unwrap();
        assert_eq!(provider.api_key_env, None, "\"\" is the clear signal");
    }

    #[tokio::test]
    async fn replace_clears_the_stored_env_name_when_a_fresh_inline_key_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let lock = Mutex::new(None);
        create(&lock, Some(&path), &no_cached_token(), create_env_form())
            .await
            .unwrap();

        let mut form = replace_form();
        form.api_key = Some("sk-fresh".into());
        replace(&lock, Some(&path), &no_cached_token(), "kimi-code", form)
            .await
            .unwrap();

        let written = read_back(&path);
        let provider = written.providers.get("kimi-code").unwrap();
        assert_eq!(provider.api_key.as_deref(), Some("sk-fresh"));
        assert_eq!(
            provider.api_key_env, None,
            "a fresh inline key clears the stored env name"
        );
    }

    #[tokio::test]
    async fn replace_clears_the_inline_key_with_an_empty_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let lock = Mutex::new(None);
        create(&lock, Some(&path), &no_cached_token(), create_form())
            .await
            .unwrap();

        let mut form = replace_form();
        form.api_key = Some(String::new());
        replace(&lock, Some(&path), &no_cached_token(), "kimi-code", form)
            .await
            .unwrap();

        let written = read_back(&path);
        let provider = written.providers.get("kimi-code").unwrap();
        assert_eq!(
            provider.api_key, None,
            "\"\" is the tri-state clear signal (kap-server routes/modelCatalog.ts:328)"
        );
    }

    #[tokio::test]
    async fn create_normalizes_an_empty_api_key_to_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let lock = Mutex::new(None);
        let mut form = create_form();
        form.api_key = Some(String::new());
        create(&lock, Some(&path), &no_cached_token(), form)
            .await
            .unwrap();

        let written = read_back(&path);
        let provider = written.providers.get("kimi-code").unwrap();
        assert_eq!(
            provider.api_key, None,
            "an empty api_key is meaningless on create: the provider carries no inline key"
        );
    }
}
