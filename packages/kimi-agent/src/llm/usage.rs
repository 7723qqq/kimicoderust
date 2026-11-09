//! Cache-token accounting shared by the OpenAI and Responses usage parsers
//! (v2 `requester/bases/openai/usage.ts`).
//!
//! Providers report prompt-cache accounting in three incompatible shapes: a
//! generic `cached_tokens` (sometimes nested), and DeepSeek's explicit
//! hit/miss pair. When the pair contradicts the prompt total the reported
//! numbers are unusable, so the split collapses to the generic cached count.

/// The resolved cache accounting of one usage object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSplit {
    /// DeepSeek reported a usable hit/miss pair.
    Split { cached: u32, miss: Option<u32> },
    /// DeepSeek's pair disagrees with `prompt_tokens`; ignore it.
    Contradiction { cached: u32, reported_miss: u32 },
    /// No DeepSeek pair; only the generic cached count is known.
    Absent { cached: u32 },
}

impl CacheSplit {
    pub fn cached(self) -> u32 {
        match self {
            CacheSplit::Split { cached, .. }
            | CacheSplit::Contradiction { cached, .. }
            | CacheSplit::Absent { cached } => cached,
        }
    }

    /// The uncached input count, from the split when usable.
    pub fn input_other(self, prompt_tokens: u32) -> u32 {
        match self {
            CacheSplit::Split {
                miss: Some(miss), ..
            } => miss,
            _ => prompt_tokens.saturating_sub(self.cached()),
        }
    }
}

/// v2 `classifyDeepSeekCacheSplit`.
pub fn classify_deepseek_cache_split(
    hit: Option<u32>,
    miss: Option<u32>,
    prompt_tokens: u32,
    fallback_cached: u32,
) -> CacheSplit {
    let Some(hit) = hit else {
        return CacheSplit::Absent {
            cached: fallback_cached,
        };
    };
    if let Some(miss) = miss
        && hit.saturating_add(miss) > prompt_tokens
    {
        return CacheSplit::Contradiction {
            cached: fallback_cached,
            reported_miss: miss,
        };
    }
    CacheSplit::Split { cached: hit, miss }
}

/// v2 `readOpenAICacheFields`: the generic cached count falls back from the
/// flat `cached_tokens` through both nested detail objects.
pub fn read_openai_cache_fields(
    cached_tokens: Option<u32>,
    prompt_tokens_details_cached: Option<u32>,
    input_tokens_details_cached: Option<u32>,
    hit: Option<u32>,
    miss: Option<u32>,
    prompt_tokens: u32,
) -> CacheSplit {
    let fallback_cached = cached_tokens
        .or(prompt_tokens_details_cached)
        .or(input_tokens_details_cached)
        .unwrap_or(0);
    classify_deepseek_cache_split(hit, miss, prompt_tokens, fallback_cached)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_the_flat_cached_count_then_the_nested_ones() {
        assert_eq!(
            read_openai_cache_fields(Some(10), Some(20), Some(30), None, None, 100),
            CacheSplit::Absent { cached: 10 }
        );
        assert_eq!(
            read_openai_cache_fields(None, Some(20), Some(30), None, None, 100),
            CacheSplit::Absent { cached: 20 }
        );
        assert_eq!(
            read_openai_cache_fields(None, None, Some(30), None, None, 100),
            CacheSplit::Absent { cached: 30 }
        );
        assert_eq!(
            read_openai_cache_fields(None, None, None, None, None, 100),
            CacheSplit::Absent { cached: 0 }
        );
    }

    #[test]
    fn deepseek_pair_wins_when_it_adds_up() {
        let split = classify_deepseek_cache_split(Some(80), Some(20), 100, 5);
        assert_eq!(
            split,
            CacheSplit::Split {
                cached: 80,
                miss: Some(20)
            }
        );
        assert_eq!(split.cached(), 80);
        assert_eq!(split.input_other(100), 20);
    }

    #[test]
    fn contradictory_pair_falls_back_to_the_generic_count() {
        let split = classify_deepseek_cache_split(Some(90), Some(20), 100, 5);
        assert_eq!(
            split,
            CacheSplit::Contradiction {
                cached: 5,
                reported_miss: 20
            }
        );
        assert_eq!(split.cached(), 5);
        assert_eq!(split.input_other(100), 95);
    }

    #[test]
    fn hit_without_miss_derives_the_remainder_from_the_prompt_total() {
        let split = classify_deepseek_cache_split(Some(80), None, 100, 0);
        assert_eq!(
            split,
            CacheSplit::Split {
                cached: 80,
                miss: None
            }
        );
        assert_eq!(split.input_other(100), 20);
    }
}
