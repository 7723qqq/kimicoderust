use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use serde_json::{Map, Value, json};

use crate::human::llm::media::image_formats::provider_image_policy_for;
use crate::human::llm::message::{ContentPart, Message, ToolDescription};
use crate::human::llm::model::LlmModel;
use crate::human::llm::protocol::connection::{ProtocolEndpoint, ProviderConnection};
use crate::human::llm::protocol::thinking::ThinkingContribution;
use crate::human::llm::requester::bases::anthropic::contract::CONTEXT_MANAGEMENT_BETA;
use crate::human::llm::requester::bases::anthropic::r#trait::AnthropicTrait;
use crate::human::llm::requester::bases::openai::contract::{
    OpenAIContent, OpenAIRawUsage, OpenAIWireMessage,
};
use crate::human::llm::requester::bases::openai::openai_trait::{
    CompletionTokensWhenUnsetInput, OpenAITrait,
};
use crate::human::llm::requester::bases::openai_responses::openai_responses_trait::OpenAIResponsesTrait;
use crate::human::llm::requester::requester::ToolMessageConversion;
use crate::human::llm_kimi::schema::normalize_kimi_tool_schema;

pub const KIMI_API_KEY_ENV: &str = "KIMI_API_KEY";
pub const KIMI_BASE_URL_ENV: &str = "KIMI_BASE_URL";
pub const KIMI_DEFAULT_BASE_URL: &str = "https://api.moonshot.ai/v1";

pub static KIMI_CONNECTION: LazyLock<ProviderConnection> = LazyLock::new(|| {
    ProviderConnection::new(|_| {
        Some(ProtocolEndpoint {
            api_key_env: Some(KIMI_API_KEY_ENV.to_string()),
            base_url_env: Some(KIMI_BASE_URL_ENV.to_string()),
            default_base_url: Some(KIMI_DEFAULT_BASE_URL.to_string()),
        })
    })
});

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'..='\u{000d}'
                | ' '
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

fn is_effectively_empty_content(parts: &[ContentPart]) -> bool {
    parts.iter().all(|part| {
        matches!(part, ContentPart::Text { text, .. } if js_trim(text).is_empty())
    })
}

fn convert_kimi_tool(tool: &ToolDescription) -> Map<String, Value> {
    if tool.name.starts_with('$') {
        return Map::from_iter([
            ("type".to_string(), json!("builtin_function")),
            ("function".to_string(), json!({ "name": tool.name })),
        ]);
    }
    Map::from_iter([
        ("type".to_string(), json!("function")),
        (
            "function".to_string(),
            json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": normalize_kimi_tool_schema(tool.parameters.clone()),
            }),
        ),
    ])
}

fn kimi_accepted_image_mimes() -> HashSet<String> {
    provider_image_policy_for(Some("kimi")).accepted_mimes
}

fn unset_completion_tokens(model: &LlmModel, used_context_tokens: Option<u64>) -> Option<u64> {
    let window = model.max_context_size.filter(|window| *window > 0)?;
    let remaining = window.checked_sub(used_context_tokens?)?;
    (remaining > 0).then_some(remaining)
}

pub fn kimi_unset_completion_tokens(input: &CompletionTokensWhenUnsetInput) -> Option<u64> {
    unset_completion_tokens(&input.model, input.used_context_tokens)
}

fn convert_kimi_message(
    message: &Message,
    converted: &OpenAIWireMessage,
) -> Option<Option<OpenAIWireMessage>> {
    let mut converted = converted.clone();
    if let Message::Assistant(source) = message
        && let OpenAIWireMessage::Assistant {
            content,
            tool_calls,
            extra,
        } = &mut converted
    {
        if !source.tool_calls.is_empty() {
            let non_think_parts = source
                .content
                .iter()
                .filter(|part| !matches!(part, ContentPart::Think { .. }))
                .cloned()
                .collect::<Vec<_>>();
            if is_effectively_empty_content(&non_think_parts) {
                *content = None;
            }
        }
        if let Some(calls) = tool_calls.take() {
            let mut encoded = serde_json::to_value(&calls).unwrap_or_else(|_| json!([]));
            let mut has_extras = false;
            if let Some(encoded_calls) = encoded.as_array_mut() {
                for (index, source_call) in source.tool_calls.iter().enumerate() {
                    if let Some(extras) = &source_call.extras
                        && let Some(out) = encoded_calls.get_mut(index)
                    {
                        out["extras"] = Value::Object(extras.clone());
                        has_extras = true;
                    }
                }
            }
            if has_extras {
                extra.insert("tool_calls".to_string(), encoded);
            } else {
                *tool_calls = Some(calls);
            }
        }
    }
    if let Message::System(source) = message
        && let OpenAIWireMessage::System { extra, .. } = &mut converted
        && let Some(tools) = source.tools.as_ref().filter(|tools| !tools.is_empty())
    {
        extra.insert(
            "tools".to_string(),
            Value::Array(
                tools
                    .iter()
                    .map(|tool| Value::Object(convert_kimi_tool(tool)))
                    .collect(),
            ),
        );
    }
    Some(Some(converted))
}

pub static KIMI_RESPONSES_TRAIT: LazyLock<OpenAIResponsesTrait> = LazyLock::new(|| {
    OpenAIResponsesTrait {
        completion_tokens_when_unset: Some(Arc::new(|input| {
            unset_completion_tokens(&input.model, input.used_context_tokens)
        })),
        ..Default::default()
    }
});

pub static KIMI_OPEN_AI_TRAIT: LazyLock<OpenAITrait> = LazyLock::new(|| OpenAITrait {
    strict_thinking_validation: Some(true),
    tool_message_conversion: Some(ToolMessageConversion::KeepParts),
    encode_cache_key: Some(Arc::new(|key, _| {
        Some(Map::from_iter([(
            "prompt_cache_key".to_string(),
            json!(key),
        )]))
    })),
    thinking: Some(Arc::new(|thinking, _| {
        let mut config = Map::new();
        config.insert(
            "type".to_string(),
            json!(if thinking.effort == "off" { "disabled" } else { "enabled" }),
        );
        if thinking.effort != "off" && thinking.effort != "on" {
            config.insert("effort".to_string(), json!(thinking.effort));
        }
        if let Some(keep) = &thinking.keep {
            config.insert("keep".to_string(), json!(keep));
        }
        Some(ThinkingContribution {
            kwargs: Map::from_iter([("thinking".to_string(), Value::Object(config))]),
            preserve_thinking: (thinking.keep.as_deref() == Some("all")
                && thinking.effort != "off")
                .then_some(true),
        })
    })),
    encode_max_completion_tokens: Some(Arc::new(|max_completion_tokens, _| {
        Some(Map::from_iter([(
            "max_completion_tokens".to_string(),
            json!(max_completion_tokens.min(128 * 1024)),
        )]))
    })),
    completion_tokens_when_unset: Some(Arc::new(kimi_unset_completion_tokens)),
    build_params: Some(Arc::new(|mut params, _| {
        let Some(extra_body) = params.get("extra_body") else {
            return Some(params);
        };
        if extra_body.is_null() {
            return Some(params);
        }
        let extra_body = params.remove("extra_body").unwrap();
        if let Value::Object(extra_body) = extra_body {
            params.extend(extra_body);
        }
        Some(params)
    })),
    convert_tool: Some(Arc::new(|tool, _| Some(convert_kimi_tool(tool)))),
    convert_message: Some(Arc::new(|message, converted, _| {
        convert_kimi_message(message, converted)
    })),
    extract_usage: Some(Arc::new(|chunk| {
        if let Some(usage) = &chunk.usage {
            return Some(usage.clone());
        }
        chunk.choices.first().and_then(|choice| {
            choice
                .extra
                .get("usage")
                .filter(|usage| usage.is_object())
                .and_then(|usage| serde_json::from_value::<OpenAIRawUsage>(usage.clone()).ok())
        })
    })),
    ..Default::default()
});

pub static KIMI_ANTHROPIC_TRAIT: LazyLock<AnthropicTrait> = LazyLock::new(|| AnthropicTrait {
    accepted_image_mimes: Some(Arc::new(|_| Some(kimi_accepted_image_mimes()))),
    thinking: Some(Arc::new(|thinking, _| {
        let mut kwargs = Map::new();
        if thinking.effort == "off" {
            kwargs.insert("thinking".to_string(), json!({ "type": "disabled" }));
        } else {
            kwargs.insert("thinking".to_string(), json!({ "type": "enabled" }));
            if thinking.effort != "on" {
                kwargs.insert(
                    "output_config".to_string(),
                    json!({ "effort": thinking.effort }),
                );
            }
        }
        kwargs.insert("betaFeatures".to_string(), json!([CONTEXT_MANAGEMENT_BETA]));
        Some(ThinkingContribution {
            kwargs,
            preserve_thinking: None,
        })
    })),
    ..Default::default()
});
