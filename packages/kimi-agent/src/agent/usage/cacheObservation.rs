use serde::{Deserialize, Serialize};

use crate::agent::llm_requester::llm_request_ops::LlmRequestProjection;
use crate::human::llm::thinking::ThinkingEffort;
use crate::human::llm::usage::{TokenUsage, input_total};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheObservation {
    pub model_alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    pub message_count: usize,
    pub history_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<LlmRequestProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<ThinkingEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub output_tokens: u64,
}

pub struct CacheObservationInput<'a> {
    pub model_alias: &'a str,
    pub protocol: Option<&'a str>,
    pub provider_type: Option<&'a str>,
    pub message_count: usize,
    pub history_hash: &'a str,
    pub projection: Option<LlmRequestProjection>,
    pub thinking_effort: Option<&'a str>,
    pub usage: Option<&'a TokenUsage>,
}

pub fn build_cache_observation(input: CacheObservationInput<'_>) -> CacheObservation {
    let usage = input.usage;
    CacheObservation {
        model_alias: input.model_alias.to_string(),
        protocol: input.protocol.map(str::to_string),
        provider_type: input.provider_type.map(str::to_string),
        message_count: input.message_count,
        history_hash: input.history_hash.to_string(),
        projection: input.projection,
        thinking_effort: input.thinking_effort.map(str::to_string),
        input_tokens: usage.map(input_total),
        cache_read: usage.map_or(0, |usage| usage.input_cache_read),
        cache_creation: usage.map_or(0, |usage| usage.input_cache_creation),
        output_tokens: usage.map_or(0, |usage| usage.output),
    }
}
