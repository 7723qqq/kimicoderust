use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use kimi_core::types::TokenUsage;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsagePatch {
    pub input_other: Option<u64>,
    pub output: Option<u64>,
    pub input_cache_read: Option<u64>,
    pub input_cache_creation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Map<String, Value>>,
}

pub fn empty_usage() -> TokenUsage {
    TokenUsage::default()
}

pub fn input_total(usage: &TokenUsage) -> u64 {
    usage.input_other + usage.input_cache_read + usage.input_cache_creation
}

pub fn grand_total(usage: &TokenUsage) -> u64 {
    input_total(usage) + usage.output
}

pub fn chargeable_tokens(usage: &TokenUsage) -> u64 {
    usage.input_other + usage.input_cache_creation + usage.output
}

pub fn add_usage(a: &TokenUsage, b: &TokenUsage) -> TokenUsage {
    TokenUsage {
        input_other: a.input_other + b.input_other,
        output: a.output + b.output,
        input_cache_read: a.input_cache_read + b.input_cache_read,
        input_cache_creation: a.input_cache_creation + b.input_cache_creation,
        raw: None,
    }
}

pub fn merge_usage_patch(base: Option<&TokenUsage>, patch: TokenUsagePatch) -> TokenUsage {
    TokenUsage {
        input_other: patch
            .input_other
            .or_else(|| base.map(|base| base.input_other))
            .unwrap_or(0),
        output: patch
            .output
            .or_else(|| base.map(|base| base.output))
            .unwrap_or(0),
        input_cache_read: patch
            .input_cache_read
            .or_else(|| base.map(|base| base.input_cache_read))
            .unwrap_or(0),
        input_cache_creation: patch
            .input_cache_creation
            .or_else(|| base.map(|base| base.input_cache_creation))
            .unwrap_or(0),
        raw: None,
    }
}
