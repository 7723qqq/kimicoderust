#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSplit {
    Split { cached: u32, miss: Option<u32> },

    Contradiction { cached: u32, reported_miss: u32 },

    Absent { cached: u32 },
}

pub fn chargeable_tokens(usage: &serde_json::Value) -> i64 {
    ["inputOther", "inputCacheCreation", "output"]
        .into_iter()
        .map(|key| {
            usage
                .get(key)
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
        })
        .sum()
}

impl CacheSplit {
    pub fn cached(self) -> u32 {
        match self {
            CacheSplit::Split { cached, .. }
            | CacheSplit::Contradiction { cached, .. }
            | CacheSplit::Absent { cached } => cached,
        }
    }

    pub fn input_other(self, prompt_tokens: u32) -> u32 {
        match self {
            CacheSplit::Split {
                miss: Some(miss), ..
            } => miss,
            _ => prompt_tokens.saturating_sub(self.cached()),
        }
    }
}

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
    fn chargeable_tokens_exclude_cache_reads() {
        let usage = serde_json::json!({
            "inputOther": 10,
            "inputCacheRead": 20,
            "inputCacheCreation": 3,
            "output": 7,
        });
        assert_eq!(chargeable_tokens(&usage), 20);
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
