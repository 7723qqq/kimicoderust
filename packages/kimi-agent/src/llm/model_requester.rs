use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SamplingOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(rename = "topP", default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

#[derive(Clone, Default)]
pub struct ModelRequestParams {
    pub cache_key: Option<String>,
    pub sampling: Option<SamplingOptions>,
    pub thinking_effort: Option<String>,
    pub thinking_keep: Option<String>,
    pub max_completion_tokens: Option<u32>,
    pub used_context_tokens: Option<u32>,
    pub max_context_tokens: Option<u32>,
    pub on_trace_id: Option<Arc<dyn Fn(Option<String>) + Send + Sync>>,
}

pub fn effective_max_completion_tokens(params: Option<&ModelRequestParams>) -> Option<u32> {
    params.and_then(|params| params.max_completion_tokens)
}
