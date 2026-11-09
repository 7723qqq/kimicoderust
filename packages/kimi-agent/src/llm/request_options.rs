//! Optional request-body knobs shared by the protocol writers (v2
//! `human/llm/response-format.ts` + `requester/bases/*/requester.ts`).
//!
//! These are capabilities of the request layer, not of the engine's turn
//! config: the transport can be handed a `RequestOptions` value and each
//! writer maps the parts its protocol understands. An empty value is inert and
//! leaves the wire body byte-identical to a call without options.

use serde_json::{Map, Value};

use super::wire::WireMessage;
use kimi_core::types::ContentBlock;

/// v2 `ResponseFormat`.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseFormat {
    JsonObject,
    JsonSchema {
        name: String,
        schema: Value,
        strict: Option<bool>,
        description: Option<String>,
    },
}

impl ResponseFormat {
    /// v2 `responseFormatToOpenAI` / v2 `responseFormatToResponsesText`.
    pub fn to_openai_value(&self) -> Value {
        match self {
            ResponseFormat::JsonObject => serde_json::json!({ "type": "json_object" }),
            ResponseFormat::JsonSchema {
                name,
                schema,
                strict,
                description,
            } => {
                let mut json_schema = Map::new();
                json_schema.insert("name".to_string(), Value::String(name.clone()));
                json_schema.insert("schema".to_string(), schema.clone());
                insert_optional(&mut json_schema, "strict", strict.map(Value::Bool));
                insert_optional(
                    &mut json_schema,
                    "description",
                    description.clone().map(Value::String),
                );
                let mut format = Map::new();
                format.insert("type".to_string(), Value::String("json_schema".to_string()));
                format.insert("json_schema".to_string(), Value::Object(json_schema));
                Value::Object(format)
            }
        }
    }

    /// v2 `responseFormatToResponsesText`: the Responses body flattens the
    /// schema onto `text.format` instead of nesting it under `json_schema`.
    pub fn to_responses_text_value(&self) -> Value {
        match self {
            ResponseFormat::JsonObject => serde_json::json!({ "type": "json_object" }),
            ResponseFormat::JsonSchema {
                name,
                schema,
                strict,
                description,
            } => {
                let mut format = Map::new();
                format.insert("type".to_string(), Value::String("json_schema".to_string()));
                format.insert("name".to_string(), Value::String(name.clone()));
                format.insert("schema".to_string(), schema.clone());
                insert_optional(&mut format, "strict", strict.map(Value::Bool));
                insert_optional(
                    &mut format,
                    "description",
                    description.clone().map(Value::String),
                );
                Value::Object(format)
            }
        }
    }

    /// v2 `applyAnthropicResponseFormat`: Anthropic has no `json_object` mode.
    pub fn to_anthropic_output_config(&self) -> Result<Value, String> {
        match self {
            ResponseFormat::JsonObject => {
                Err("Anthropic requires a JSON schema for structured response output.".to_string())
            }
            ResponseFormat::JsonSchema { schema, .. } => {
                Ok(serde_json::json!({ "format": { "type": "json_schema", "schema": schema } }))
            }
        }
    }
}

fn insert_optional(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.to_string(), value);
    }
}

/// v2 `ExtraParams`, split per protocol.
#[derive(Debug, Clone, Default)]
pub struct ExtraParams {
    pub openai: Map<String, Value>,
    pub responses: Map<String, Value>,
    pub anthropic: Map<String, Value>,
}

/// The optional request-body knobs a caller may attach to a request.
#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub response_format: Option<ResponseFormat>,
    /// Explicit determinism seed; takes precedence over the derived one.
    pub seed: Option<i64>,
    /// Derive a stable seed from model + first user text when no explicit
    /// seed is given (v2 `determinism.seed ?? deriveDeterminismSeed(...)`).
    pub derive_seed: bool,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// Protocol-specific raw parameters merged last; they may override any
    /// field the writer produced.
    pub extra_params: ExtraParams,
}

impl RequestOptions {
    pub fn is_inert(&self) -> bool {
        self.response_format.is_none()
            && self.seed.is_none()
            && !self.derive_seed
            && self.temperature.is_none()
            && self.top_p.is_none()
            && self.extra_params.openai.is_empty()
            && self.extra_params.responses.is_empty()
            && self.extra_params.anthropic.is_empty()
    }

    /// The seed to send, if determinism was requested.
    pub fn seed_for(&self, model: &str, messages: &[WireMessage]) -> Option<i64> {
        if let Some(seed) = self.seed {
            return Some(seed);
        }
        self.derive_seed
            .then(|| derive_determinism_seed(model, messages))
    }

    /// Sampling fields that belong in the OpenAI-family bodies.
    pub fn openai_sampling(&self) -> Map<String, Value> {
        sampling_map(self.temperature, self.top_p)
    }
}

impl From<&crate::rpc::types::NativeLlmConfig> for RequestOptions {
    /// Project the transport config's optional request knobs onto the writer
    /// options. Every field is absent by default, so a config that declares
    /// none yields [`RequestOptions::is_inert`].
    fn from(config: &crate::rpc::types::NativeLlmConfig) -> Self {
        Self {
            response_format: config.response_format.as_ref().map(ResponseFormat::from),
            seed: config.determinism.as_ref().and_then(|value| value.seed),
            // v2: `determinism` present without an explicit seed still derives
            // one, so presence — not a set seed — arms derivation.
            derive_seed: config.determinism.is_some(),
            temperature: config.sampling.as_ref().and_then(|value| value.temperature),
            top_p: config.sampling.as_ref().and_then(|value| value.top_p),
            extra_params: config
                .extra_params
                .as_ref()
                .map(ExtraParams::from)
                .unwrap_or_default(),
        }
    }
}

impl From<&crate::rpc::types::NativeResponseFormat> for ResponseFormat {
    fn from(value: &crate::rpc::types::NativeResponseFormat) -> Self {
        match value {
            crate::rpc::types::NativeResponseFormat::JsonObject => ResponseFormat::JsonObject,
            crate::rpc::types::NativeResponseFormat::JsonSchema { json_schema } => {
                ResponseFormat::JsonSchema {
                    name: json_schema.name.clone(),
                    schema: json_schema.schema.clone(),
                    strict: json_schema.strict,
                    description: json_schema.description.clone(),
                }
            }
        }
    }
}

impl From<&crate::rpc::types::NativeExtraParams> for ExtraParams {
    fn from(value: &crate::rpc::types::NativeExtraParams) -> Self {
        Self {
            openai: value.openai.clone(),
            responses: value.responses.clone(),
            anthropic: value.anthropic.clone(),
        }
    }
}

fn sampling_map(temperature: Option<f64>, top_p: Option<f64>) -> Map<String, Value> {
    let mut map = Map::new();
    if let Some(temperature) = temperature {
        map.insert("temperature".to_string(), Value::from(temperature));
    }
    if let Some(top_p) = top_p {
        map.insert("top_p".to_string(), Value::from(top_p));
    }
    map
}

/// v2 `stripIgnoredSamplingParams`: remove the fields the family rejects while
/// reasoning is on.
pub fn strip_ignored_sampling(kwargs: &mut Map<String, Value>, ignored: &[&str]) {
    for key in ignored {
        kwargs.remove(*key);
    }
}

/// v2 `deriveDeterminismSeed`: FNV-1a (32-bit) over `model\0<first user text>`.
pub fn derive_determinism_seed(model: &str, messages: &[WireMessage]) -> i64 {
    let source = format!("{model}\u{0}{}", first_user_text(messages));
    let mut hash: u32 = 2_166_136_261;
    for unit in source.encode_utf16() {
        hash ^= u32::from(unit);
        hash = hash.wrapping_mul(16_777_619);
    }
    i64::from(hash)
}

/// v2 `firstUserText`: the first non-empty user text turn. v2's messages are
/// part lists, so text parts are concatenated; a text-only `WireMessage` keeps
/// its text in `content` and lands on the same string.
fn first_user_text(messages: &[WireMessage]) -> String {
    for message in messages {
        if message.role != "user" {
            continue;
        }
        let from_blocks: String = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let text = if from_blocks.is_empty() {
            message.content.clone()
        } else {
            from_blocks
        };
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_format_shapes_match_each_protocol() {
        let format = ResponseFormat::JsonSchema {
            name: "out".to_string(),
            schema: serde_json::json!({ "type": "object" }),
            strict: Some(true),
            description: None,
        };
        assert_eq!(
            format.to_openai_value(),
            serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "out",
                    "schema": { "type": "object" },
                    "strict": true,
                },
            })
        );
        assert_eq!(
            format.to_responses_text_value(),
            serde_json::json!({
                "type": "json_schema",
                "name": "out",
                "schema": { "type": "object" },
                "strict": true,
            })
        );
        assert_eq!(
            format.to_anthropic_output_config().unwrap(),
            serde_json::json!({ "format": { "type": "json_schema", "schema": { "type": "object" } } })
        );
    }

    #[test]
    fn anthropic_rejects_json_object() {
        let error = ResponseFormat::JsonObject
            .to_anthropic_output_config()
            .unwrap_err();
        assert!(error.contains("JSON schema"));
    }

    #[test]
    fn determinism_seed_is_stable_and_matches_fnv1a() {
        let messages = vec![
            WireMessage::text("system", "ignored"),
            WireMessage::text("user", "hi"),
        ];
        let seed = derive_determinism_seed("deepseek-chat", &messages);
        assert_eq!(seed, derive_determinism_seed("deepseek-chat", &messages));
        assert_ne!(
            seed,
            derive_determinism_seed("deepseek-reasoner", &messages)
        );
        assert_eq!(seed, 2_666_160_327);
    }

    #[test]
    fn explicit_seed_wins_over_derivation() {
        let options = RequestOptions {
            seed: Some(7),
            derive_seed: true,
            ..RequestOptions::default()
        };
        let messages = vec![WireMessage::text("user", "hi")];
        assert_eq!(options.seed_for("m", &messages), Some(7));
    }

    #[test]
    fn default_options_are_inert() {
        assert!(RequestOptions::default().is_inert());
    }

    #[test]
    fn an_unset_native_config_projects_onto_inert_options() {
        let config = crate::rpc::types::NativeLlmConfig {
            protocol: "openai".into(),
            model: "m".into(),
            ..crate::rpc::types::NativeLlmConfig::default()
        };
        assert!(RequestOptions::from(&config).is_inert());
    }

    #[test]
    fn native_config_knobs_project_onto_the_writer_options() {
        let config = crate::rpc::types::NativeLlmConfig {
            response_format: Some(crate::rpc::types::NativeResponseFormat::JsonSchema {
                json_schema: crate::rpc::types::NativeJsonSchema {
                    name: "out".into(),
                    schema: serde_json::json!({ "type": "object" }),
                    strict: Some(true),
                    description: None,
                },
            }),
            sampling: Some(crate::rpc::types::NativeSampling {
                temperature: Some(0.2),
                top_p: Some(0.9),
            }),
            determinism: Some(crate::rpc::types::NativeDeterminism { seed: None }),
            extra_params: Some(crate::rpc::types::NativeExtraParams {
                openai: [("logit_bias".to_string(), serde_json::json!({ "1": 2 }))]
                    .into_iter()
                    .collect(),
                ..crate::rpc::types::NativeExtraParams::default()
            }),
            ..crate::rpc::types::NativeLlmConfig::default()
        };
        let options = RequestOptions::from(&config);
        assert_eq!(
            options.response_format,
            Some(ResponseFormat::JsonSchema {
                name: "out".into(),
                schema: serde_json::json!({ "type": "object" }),
                strict: Some(true),
                description: None,
            })
        );
        assert_eq!(options.temperature, Some(0.2));
        assert_eq!(options.top_p, Some(0.9));
        // Presence of `determinism` without a seed still arms derivation.
        assert_eq!(options.seed, None);
        assert!(options.derive_seed);
        assert_eq!(
            options.extra_params.openai.get("logit_bias"),
            Some(&serde_json::json!({ "1": 2 }))
        );
        assert!(!options.is_inert());
    }
}
