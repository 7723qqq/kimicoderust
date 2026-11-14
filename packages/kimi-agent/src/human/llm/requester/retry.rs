use crate::human::llm::errors::{LlmErrorKind, LlmErrorMessage, llm_status_error_message};

pub const DEFAULT_MAX_RETRY_ATTEMPTS: u32 = 10;
const BASE_DELAY_MS: u64 = 500;
const MAX_DELAY_MS: u64 = 32_000;
const RETRY_FACTOR: u32 = 2;
const JITTER_FACTOR: f64 = 0.25;

#[derive(Debug, Clone, Default)]
pub struct LlmRetryOptions {
    pub max_attempts_per_step: Option<u32>,
    pub infinite_retry: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct LlmRetryErrorFields {
    pub error_name: String,
    pub error_message: String,
    pub status_code: Option<u16>,
}

pub fn resolve_max_attempts(options: Option<&LlmRetryOptions>) -> u32 {
    options
        .and_then(|options| options.max_attempts_per_step)
        .unwrap_or(DEFAULT_MAX_RETRY_ATTEMPTS)
        .max(1)
}

pub fn retry_backoff_delay(attempt_index: u32) -> u64 {
    let base = BASE_DELAY_MS
        .saturating_mul(RETRY_FACTOR.saturating_pow(attempt_index))
        .min(MAX_DELAY_MS);
    base + (fastrand::f64() * JITTER_FACTOR * base as f64) as u64
}

pub fn read_retry_after_ms(error: &LlmErrorMessage) -> Option<i64> {
    llm_status_error_message(error)?
        .retry_after_ms
        .filter(|delay| *delay > 0)
}

pub fn is_retryable_error(error: &LlmErrorMessage) -> bool {
    if error.kind == LlmErrorKind::Syntax {
        return false;
    }
    error.retryable.unwrap_or(false)
}

pub fn should_retry(
    options: Option<&LlmRetryOptions>,
    attempt: u32,
    error: &LlmErrorMessage,
) -> bool {
    if options.and_then(|options| options.infinite_retry) == Some(true) {
        return true;
    }
    is_retryable_error(error) && attempt < resolve_max_attempts(options)
}

pub fn retry_error_fields(error: &LlmErrorMessage) -> LlmRetryErrorFields {
    LlmRetryErrorFields {
        error_name: error.kind.as_str().to_string(),
        error_message: error.message.clone(),
        status_code: llm_status_error_message(error)
            .and_then(|status| status.status_code),
    }
}
