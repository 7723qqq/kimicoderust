use std::time::Duration;

pub const DEFAULT_MAX_RETRY_ATTEMPTS: u32 = 10;

pub const DEFAULT_BASE_DELAY_MS: u64 = 500;

pub const DEFAULT_MAX_DELAY_MS: u64 = 32_000;

pub fn parse_truthy_env(name: &str) -> bool {
    crate::env::env_switch_default_off(name)
}

pub fn infinite_retry_enabled() -> bool {
    parse_truthy_env("KIMI_CODE_INFINITE_RETRY")
}

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,

    pub base_delay_ms: u64,

    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_RETRY_ATTEMPTS,
            base_delay_ms: DEFAULT_BASE_DELAY_MS,
            max_delay_ms: DEFAULT_MAX_DELAY_MS,
        }
    }
}

pub fn retry_delay(attempt: u32, config: &RetryConfig) -> Duration {
    let delay = config
        .base_delay_ms
        .saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)))
        .min(config.max_delay_ms);
    let jitter = fastrand::u64(0..=(delay / 4));
    Duration::from_millis(delay) + Duration::from_millis(jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2_range(config: &RetryConfig, attempt: u32) -> (u64, u64) {
        let nominal =
            (config.base_delay_ms * 2u64.pow(attempt.saturating_sub(1))).min(config.max_delay_ms);
        (nominal, nominal + nominal / 4)
    }

    fn assert_within_v2_range(config: &RetryConfig, attempt: u32) -> u64 {
        let (min, max) = v2_range(config, attempt);
        let ms = retry_delay(attempt, config).as_millis() as u64;
        assert!(
            (min..=max).contains(&ms),
            "attempt {attempt}: delay {ms} outside v2 range [{min}, {max}]"
        );
        ms
    }

    #[test]
    fn test_retry_config_defaults() {
        let config = RetryConfig::default();
        assert_eq!(config.max_attempts, DEFAULT_MAX_RETRY_ATTEMPTS);
        assert_eq!(config.max_attempts, 10);
        assert_eq!(config.base_delay_ms, DEFAULT_BASE_DELAY_MS);
        assert_eq!(config.base_delay_ms, 500);
        assert_eq!(config.max_delay_ms, DEFAULT_MAX_DELAY_MS);
        assert_eq!(config.max_delay_ms, 32_000);
    }

    #[test]
    fn parse_truthy_env_matches_v2_boolean_parsing() {
        let name = "KIMI_TEST_TRUTHY_ENV_PROBE";
        for value in ["1", "true", "TRUE", "yes", "on", " on "] {
            unsafe { std::env::set_var(name, value) };
            assert!(parse_truthy_env(name), "{value:?} must be truthy");
        }
        for value in ["0", "false", "no", "off", "", "maybe"] {
            unsafe { std::env::set_var(name, value) };
            assert!(!parse_truthy_env(name), "{value:?} must be falsy");
        }
        unsafe { std::env::remove_var(name) };
        assert!(!parse_truthy_env(name), "unset must be falsy");
    }

    #[test]
    fn test_retry_delay_first_attempt() {
        let config = RetryConfig::default();

        assert_eq!(v2_range(&config, 1), (500, 625));
        assert_within_v2_range(&config, 1);
    }

    #[test]
    fn test_retry_delay_second_attempt() {
        let config = RetryConfig::default();
        assert_eq!(v2_range(&config, 2), (1000, 1250));
        assert_within_v2_range(&config, 2);
    }

    #[test]
    fn test_retry_delay_third_attempt() {
        let config = RetryConfig::default();
        assert_eq!(v2_range(&config, 3), (2000, 2500));
        assert_within_v2_range(&config, 3);
    }

    #[test]
    fn test_retry_delay_caps_at_max() {
        let config = RetryConfig {
            max_attempts: 10,
            base_delay_ms: 1000,
            max_delay_ms: 5000,
        };

        assert_eq!(v2_range(&config, 5), (5000, 6250));
        assert_within_v2_range(&config, 5);
    }

    #[test]
    fn test_retry_delay_zero_base_delay_matches_v2() {
        let config = RetryConfig {
            max_attempts: 3,
            base_delay_ms: 0,
            max_delay_ms: 1000,
        };
        assert_eq!(retry_delay(1, &config).as_millis(), 0);
    }

    #[test]
    fn test_retry_delay_high_attempt_respects_max() {
        let config = RetryConfig {
            max_attempts: 20,
            base_delay_ms: 100,
            max_delay_ms: 2000,
        };
        for attempt in 1..=10 {
            let (min, max) = v2_range(&config, attempt);
            assert!(
                max <= 2500,
                "attempt {attempt}: the 25% jitter must not push past max + 25%"
            );
            let ms = assert_within_v2_range(&config, attempt);
            assert!(ms >= min);
        }
    }

    #[test]
    fn high_retry_counts_keep_the_capped_backoff() {
        let config = RetryConfig::default();
        for attempt in [7, 56, 57, 63, 64, 65, 128, 1024, u32::MAX] {
            let delay = retry_delay(attempt, &config).as_millis();
            assert!(
                (32_000..=40_000).contains(&delay),
                "attempt {attempt}: {delay}"
            );
        }
    }

    #[test]
    fn high_retry_counts_preserve_zero_and_custom_delays() {
        for (base_delay_ms, max_delay_ms, min, max) in [
            (0, 32_000, 0, 0),
            (500, 0, 0, 0),
            (1, 2000, 2000, 2500),
            (u64::MAX, 2000, 2000, 2500),
        ] {
            let config = RetryConfig {
                base_delay_ms,
                max_delay_ms,
                ..RetryConfig::default()
            };
            for attempt in [63, 65, u32::MAX] {
                let delay = retry_delay(attempt, &config).as_millis();
                assert!(
                    (min..=max).contains(&delay),
                    "base {base_delay_ms}, attempt {attempt}: {delay}"
                );
            }
        }
    }

    #[test]
    fn jitter_does_not_overflow_at_the_largest_delay() {
        let config = RetryConfig {
            base_delay_ms: u64::MAX,
            max_delay_ms: u64::MAX,
            ..RetryConfig::default()
        };
        let min = u128::from(u64::MAX);
        let max = min + min / 4;
        for attempt in [1, 2, 65, u32::MAX] {
            let delay = retry_delay(attempt, &config).as_millis();
            assert!((min..=max).contains(&delay), "attempt {attempt}: {delay}");
        }
    }

    #[test]
    fn test_retry_delay_jitter_variation() {
        let config = RetryConfig::default();
        let mut delays = std::collections::HashSet::new();
        for _ in 0..50 {
            delays.insert(retry_delay(1, &config));
        }
        assert!(delays.len() >= 2, "jitter should produce varied delays");
    }

    #[test]
    fn test_retry_delay_never_undercuts_the_nominal_backoff() {
        let config = RetryConfig::default();
        for attempt in 1..=12 {
            let nominal = (config.base_delay_ms * 2u64.pow(attempt - 1)).min(config.max_delay_ms);
            let ms = retry_delay(attempt, &config).as_millis() as u64;
            assert!(
                ms >= nominal,
                "attempt {attempt}: {ms} undercuts the nominal {nominal}; \
                 v2 jitters upward only"
            );
        }
    }

    #[test]
    fn test_retry_delay_non_zero() {
        let config = RetryConfig::default();
        for attempt in 1..=10 {
            assert!(
                retry_delay(attempt, &config).as_millis() >= 500,
                "attempt {attempt}: the default config never waits less than the 500ms base"
            );
        }
    }

    #[test]
    fn test_retry_delay_custom_config() {
        let config = RetryConfig {
            max_attempts: 5,
            base_delay_ms: 500,
            max_delay_ms: 10000,
        };

        assert_eq!(v2_range(&config, 3), (2000, 2500));
        assert_within_v2_range(&config, 3);
    }

    #[test]
    fn test_retry_delay_attempt_zero() {
        let config = RetryConfig::default();

        assert_eq!(v2_range(&config, 0), (500, 625));
        assert_within_v2_range(&config, 0);
    }

    #[test]
    fn test_retry_delay_small_base() {
        let config = RetryConfig {
            max_attempts: 3,
            base_delay_ms: 50,
            max_delay_ms: 1000,
        };
        assert_eq!(v2_range(&config, 1), (50, 62));
        assert_within_v2_range(&config, 1);
    }

    #[test]
    fn test_retry_delay_exponential_growth() {
        let config = RetryConfig {
            max_attempts: 10,
            base_delay_ms: 200,
            max_delay_ms: 100000,
        };
        let mut prev_max = 0;
        for attempt in 1..=5 {
            let ms = retry_delay(attempt, &config).as_millis() as u64;
            let (min_expected, max_expected) = v2_range(&config, attempt);
            assert!(
                ms >= min_expected && ms <= max_expected,
                "attempt {attempt}: delay {ms} outside [{min_expected}, {max_expected}]"
            );
            if attempt > 1 {
                assert!(
                    min_expected > prev_max,
                    "attempt {attempt} minimum {min_expected} must strictly exceed previous max {prev_max}"
                );
            }
            prev_max = max_expected;
        }
    }

    #[test]
    fn test_retry_delay_max_attempt_delays() {
        let config = RetryConfig {
            max_attempts: 3,
            base_delay_ms: 1000,
            max_delay_ms: 32_000,
        };
        let d1 = retry_delay(1, &config).as_millis();
        let d3 = retry_delay(3, &config).as_millis();

        assert!(d3 > d1, "d3={d3} should exceed d1={d1}");
    }

    #[test]
    fn test_retry_delay_consistent_type() {
        let config = RetryConfig {
            max_attempts: 10,
            base_delay_ms: 1000,
            max_delay_ms: 8000,
        };
        for attempt in 1..=10 {
            let (min, max) = v2_range(&config, attempt);
            let ms = retry_delay(attempt, &config).as_millis() as u64;
            assert!((min..=max).contains(&ms), "attempt {attempt}: {ms}");
            assert!(ms <= 10_000, "attempt {attempt}: {ms} past max + 25%");
        }
    }

    #[test]
    fn test_default_backoff_saturates_at_v2_max() {
        let config = RetryConfig::default();

        assert_eq!(v2_range(&config, 7), (32_000, 40_000));
        assert_eq!(v2_range(&config, 12).0, 32_000);
    }
}
