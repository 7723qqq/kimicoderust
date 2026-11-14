use serde_json::json;

use crate::human::llm::requester::bases::openai::contract::OpenAIRawUsage;
use crate::human::llm::usage::TokenUsage;
use crate::human::log::log::LlmLogger;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAICacheSplit {
    Split {
        cached: u64,
        miss: Option<u64>,
    },
    Contradiction {
        cached: u64,
        reported_miss: u64,
    },
    Absent {
        cached: u64,
    },
}

pub fn classify_deep_seek_cache_split(
    hit: Option<u64>,
    miss: Option<u64>,
    prompt_tokens: u64,
    fallback_cached: u64,
) -> OpenAICacheSplit {
    let Some(hit) = hit else {
        return OpenAICacheSplit::Absent {
            cached: fallback_cached,
        };
    };
    if miss.is_some_and(|miss| hit + miss > prompt_tokens) {
        return OpenAICacheSplit::Contradiction {
            cached: fallback_cached,
            reported_miss: miss.unwrap(),
        };
    }
    OpenAICacheSplit::Split {
        cached: hit,
        miss,
    }
}

pub fn read_open_ai_cache_fields(usage: &OpenAIRawUsage) -> OpenAICacheSplit {
    let cached_tokens = usage
        .cached_tokens
        .or_else(|| usage.prompt_tokens_details.as_ref().and_then(|details| details.cached_tokens))
        .or_else(|| usage.input_tokens_details.as_ref().and_then(|details| details.cached_tokens))
        .unwrap_or(0);
    classify_deep_seek_cache_split(
        usage.prompt_cache_hit_tokens,
        usage.prompt_cache_miss_tokens,
        usage.prompt_tokens.unwrap_or(0),
        cached_tokens,
    )
}

pub fn parse_open_ai_usage(
    usage: Option<&OpenAIRawUsage>,
    log: Option<&LlmLogger>,
) -> Option<TokenUsage> {
    let usage = usage?;
    let prompt_tokens = usage.prompt_tokens.unwrap_or(0);
    let split = read_open_ai_cache_fields(usage);
    if let OpenAICacheSplit::Contradiction { reported_miss, .. } = &split
        && let Some(log) = log
    {
        log.warn(
            "openai usage cache fields disagree; falling back to the cached count",
            Some(json!({
                "promptTokens": prompt_tokens,
                "promptCacheHitTokens": usage.prompt_cache_hit_tokens,
                "promptCacheMissTokens": reported_miss,
            })),
        );
    }
    let (cached, miss) = match split {
        OpenAICacheSplit::Split { cached, miss } => (cached, miss),
        OpenAICacheSplit::Contradiction { cached, .. } => (cached, None),
        OpenAICacheSplit::Absent { cached } => (cached, None),
    };
    Some(TokenUsage {
        input_other: miss.unwrap_or_else(|| prompt_tokens.saturating_sub(cached)),
        output: usage.completion_tokens.unwrap_or(0),
        input_cache_read: cached,
        input_cache_creation: 0,
        raw: serde_json::to_value(usage).ok().and_then(|value| value.as_object().cloned()),
    })
}
