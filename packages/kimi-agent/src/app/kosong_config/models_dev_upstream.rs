use std::sync::{Arc, LazyLock};

use kimi_core::primitives::errors::{Error2, domains::core::INTERNAL};
use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::utils::fetch_like::{FetchInit, FetchLike};
use crate::llm_adapter::contract::capability::ModelCapability;
use crate::mcp_core::client_shared::ReqwestFetch;

use super::built_in_models_dev::BUILT_IN_MODELS_DEV_JSON;
use super::config_section::{ModelRecord, Protocol};
use super::errors::CATALOG_UNAVAILABLE;
use super::models_dev::{
    ModelsDevCatalog, ModelsDevModel, ModelsDevProviderEntry,
    models_dev_provider_models, resolve_models_dev_import, ModelsDevImportResolution,
};
use super::models_dev_import::{ModelsDevModelItem, ModelsDevProviderItem};

pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const CACHE_TTL_MS: i64 = 10 * 60 * 1000;
pub const UPSTREAM_FETCH_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone)]
struct ModelsDevCacheEntry {
    catalog: ModelsDevCatalog,
    fetched_at: i64,
}

enum BuiltInMemo {
    Uninitialized,
    Loaded(Option<ModelsDevCatalog>),
}

struct UpstreamState {
    cache: Option<ModelsDevCacheEntry>,
    built_in_memo: BuiltInMemo,
    fetch_impl: Arc<dyn FetchLike>,
    now_impl: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl Default for UpstreamState {
    fn default() -> Self {
        Self {
            cache: None,
            built_in_memo: BuiltInMemo::Uninitialized,
            fetch_impl: Arc::new(ReqwestFetch::new()),
            now_impl: Arc::new(|| chrono::Utc::now().timestamp_millis()),
        }
    }
}

static STATE: LazyLock<Mutex<UpstreamState>> = LazyLock::new(|| Mutex::new(UpstreamState::default()));

struct InFlight {
    cell: tokio::sync::OnceCell<Result<ModelsDevCatalog, String>>,
}

static IN_FLIGHT: LazyLock<Mutex<Option<Arc<InFlight>>>> = LazyLock::new(|| Mutex::new(None));

pub fn load_built_in_models_dev_catalog(text: Option<&str>) -> Option<ModelsDevCatalog> {
    let text = text.filter(|text| !text.is_empty())?;
    serde_json::from_str(text).ok()
}

#[derive(Default)]
pub struct ModelsDevUpstreamTestOptions {
    pub fetch_impl: Option<Arc<dyn FetchLike>>,
    pub now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
}

pub fn set_models_dev_upstream_for_test(options: ModelsDevUpstreamTestOptions) {
    let mut state = STATE.lock();
    if let Some(fetch_impl) = options.fetch_impl {
        state.fetch_impl = fetch_impl;
    }
    if let Some(now) = options.now {
        state.now_impl = now;
    }
}

pub fn reset_models_dev_upstream_for_test() {
    *STATE.lock() = UpstreamState::default();
    *IN_FLIGHT.lock() = None;
}

pub fn upstream_fetch() -> Arc<dyn FetchLike> {
    STATE.lock().fetch_impl.clone()
}

fn now() -> i64 {
    let now = STATE.lock().now_impl.clone();
    now()
}

pub async fn get_models_dev_catalog(user_agent: &str) -> Result<ModelsDevCatalog, Error2> {
    let current_time = now();
    if let Some(catalog) = STATE.lock().cache.as_ref().and_then(|cache| {
        (current_time.saturating_sub(cache.fetched_at) < CACHE_TTL_MS)
            .then(|| cache.catalog.clone())
    }) {
        return Ok(catalog);
    }
    let flight = {
        let mut in_flight = IN_FLIGHT.lock();
        in_flight
            .get_or_insert_with(|| Arc::new(InFlight { cell: tokio::sync::OnceCell::new() }))
            .clone()
    };
    let result = flight
        .cell
        .get_or_init(|| fetch_and_cache(user_agent.to_string()))
        .await;
    let mut in_flight = IN_FLIGHT.lock();
    if in_flight.as_ref().is_some_and(|current| Arc::ptr_eq(current, &flight)) {
        *in_flight = None;
    }
    drop(in_flight);
    result
        .clone()
        .map_err(|error| Error2::new(CATALOG_UNAVAILABLE, error))
}

async fn fetch_and_cache(user_agent: String) -> Result<ModelsDevCatalog, String> {
    let fetched_at = now();
    let fetch_impl = upstream_fetch();
    let result = async {
        let response = fetch_impl
            .fetch(
                MODELS_DEV_URL,
                FetchInit {
                    headers: vec![
                        ("Accept".to_string(), "application/json".to_string()),
                        ("User-Agent".to_string(), user_agent),
                    ],
                    timeout_ms: Some(UPSTREAM_FETCH_TIMEOUT_MS),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        if !response.ok {
            let status = response.status;
            let error = Error2::new(INTERNAL, format!("HTTP {status}"))
                .with_details(serde_json::Map::from_iter([("status".to_string(), Value::from(status))]));
            return Err(error.to_string());
        }
        let text = response.text().await.map_err(|error| error.to_string())?;
        let payload: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if !payload.is_object() {
            return Err(Error2::new(INTERNAL, "unexpected catalog payload shape").to_string());
        }
        serde_json::from_value::<ModelsDevCatalog>(payload).map_err(|error| error.to_string())
    }
    .await;
    match result {
        Ok(catalog) => {
            STATE.lock().cache = Some(ModelsDevCacheEntry {
                catalog: catalog.clone(),
                fetched_at,
            });
            Ok(catalog)
        }
        Err(error) => {
            if let Some(cached) = STATE.lock().cache.as_ref() {
                return Ok(cached.catalog.clone());
            }
            if let Some(built_in) = built_in_catalog() {
                STATE.lock().cache = Some(ModelsDevCacheEntry {
                    catalog: built_in.clone(),
                    fetched_at,
                });
                return Ok(built_in);
            }
            Err(format!("models.dev catalog unavailable: {error}"))
        }
    }
}

fn built_in_catalog() -> Option<ModelsDevCatalog> {
    let mut state = STATE.lock();
    if matches!(state.built_in_memo, BuiltInMemo::Uninitialized) {
        state.built_in_memo = BuiltInMemo::Loaded(load_built_in_models_dev_catalog(
            BUILT_IN_MODELS_DEV_JSON,
        ));
    }
    match &state.built_in_memo {
        BuiltInMemo::Loaded(catalog) => catalog.clone(),
        BuiltInMemo::Uninitialized => None,
    }
}

pub fn models_dev_entry<'a>(catalog: &'a ModelsDevCatalog, id: &str) -> Option<&'a ModelsDevProviderEntry> {
    catalog.get(id)
}

fn capability_to_strings(capability: &ModelCapability) -> Option<Vec<String>> {
    let mut capabilities = Vec::new();
    if capability.image_in {
        capabilities.push("image_in".to_string());
    }
    if capability.video_in {
        capabilities.push("video_in".to_string());
    }
    if capability.audio_in {
        capabilities.push("audio_in".to_string());
    }
    if capability.thinking {
        capabilities.push("thinking".to_string());
    }
    if capability.tool_use {
        capabilities.push("tool_use".to_string());
    }
    if capability.dynamically_loaded_tools == Some(true) {
        capabilities.push("dynamically_loaded_tools".to_string());
    }
    (!capabilities.is_empty()).then_some(capabilities)
}

fn to_model_item(model: &ModelsDevModel) -> ModelsDevModelItem {
    ModelsDevModelItem {
        name: model.name.clone(),
        capabilities: capability_to_strings(&model.capability),
        id: model.id.clone(),
        max_context_size: model.capability.max_context_tokens,
        reasoning: model.capability.thinking,
    }
}

pub fn to_models_dev_provider_item(id: &str, entry: &ModelsDevProviderEntry) -> ModelsDevProviderItem {
    let resolution = resolve_models_dev_import(entry, None);
    let models = models_dev_provider_models(entry)
        .iter()
        .map(to_model_item)
        .collect();
    let base = ModelsDevProviderItem {
        id: id.to_string(),
        name: entry
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(id)
            .to_string(),
        env_key: entry.env.as_ref().and_then(|env| env.first()).cloned(),
        models,
        wire_type: None,
        base_url: None,
        guessed: false,
        needs_base_url: false,
        rejected: false,
        reject_reason: None,
    };
    match resolution {
        ModelsDevImportResolution::Ok {
            wire,
            guessed,
            base_url,
        } => ModelsDevProviderItem {
            wire_type: Some(wire),
            base_url,
            guessed,
            ..base
        },
        ModelsDevImportResolution::NeedsBaseUrl { wire, guessed } => ModelsDevProviderItem {
            wire_type: Some(wire),
            guessed,
            needs_base_url: true,
            ..base
        },
        ModelsDevImportResolution::Invalid { reason } => ModelsDevProviderItem {
            rejected: true,
            reject_reason: Some(reason.as_str().to_string()),
            ..base
        },
    }
}

pub fn models_dev_model_to_record(provider_id: &str, model: &ModelsDevModel) -> ModelRecord {
    let capabilities = capability_to_strings(&model.capability).map(|capabilities| {
        if model.always_thinking == Some(true) {
            capabilities
                .into_iter()
                .map(|capability| {
                    if capability == "thinking" {
                        "always_thinking".to_string()
                    } else {
                        capability
                    }
                })
                .collect()
        } else {
            capabilities
        }
    });
    ModelRecord {
        provider: Some(provider_id.to_string()),
        model: Some(model.id.clone()),
        max_context_size: Some(model.capability.max_context_tokens),
        max_input_size: model.capability.max_input_tokens,
        max_output_size: model.max_output_size,
        capabilities,
        display_name: model.name.clone(),
        reasoning_key: model.reasoning_key.clone(),
        support_efforts: model.support_efforts.clone(),
        off_effort: model.off_effort.clone(),
        protocol: model.protocol.as_deref().and_then(|protocol| match protocol {
            "anthropic" => Some(Protocol::Anthropic),
            "openai" => Some(Protocol::Openai),
            "openai_responses" => Some(Protocol::OpenaiResponses),
            "google-genai" => Some(Protocol::GoogleGenai),
            "antigravity" => Some(Protocol::Antigravity),
            _ => None,
        }),
        base_url: model.base_url.clone(),
        ..Default::default()
    }
}

