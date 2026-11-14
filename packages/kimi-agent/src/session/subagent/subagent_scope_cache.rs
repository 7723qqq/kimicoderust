use std::collections::HashMap;

use kimi_core::primitives::errors::domains::core::VALIDATION_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde_json::{Map, Value};

use crate::wire::record::WireAsyncError;

pub const SUBAGENT_SCOPE_CACHE_SIZE_ENV: &str = "KIMI_CODE_SUBAGENT_SCOPE_CACHE_SIZE";
pub const SUBAGENT_SCOPE_EVICT_TIMEOUT_ENV: &str = "KIMI_CODE_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS";
pub const DEFAULT_SUBAGENT_SCOPE_CACHE_SIZE: usize = 32;
pub const DEFAULT_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS: u64 = 15_000;

pub fn resolve_subagent_scope_cache_size(
    env: Option<&HashMap<String, String>>,
) -> Result<usize, WireAsyncError> {
    let raw = env_value(env, SUBAGENT_SCOPE_CACHE_SIZE_ENV);
    let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) else {
        return Ok(DEFAULT_SUBAGENT_SCOPE_CACHE_SIZE);
    };
    let Some(value) = parse_js_integer(&raw) else {
        return Err(validation_error(
            SUBAGENT_SCOPE_CACHE_SIZE_ENV,
            "an integer",
            &raw,
        ));
    };
    Ok(value.max(0) as usize)
}

pub fn resolve_subagent_scope_evict_timeout_ms(
    env: Option<&HashMap<String, String>>,
) -> Result<u64, WireAsyncError> {
    let raw = env_value(env, SUBAGENT_SCOPE_EVICT_TIMEOUT_ENV);
    let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) else {
        return Ok(DEFAULT_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS);
    };
    let Some(value) = parse_js_integer(&raw).filter(|value| *value > 0) else {
        return Err(validation_error(
            SUBAGENT_SCOPE_EVICT_TIMEOUT_ENV,
            "a positive integer",
            &raw,
        ));
    };
    Ok(value as u64)
}

fn env_value(env: Option<&HashMap<String, String>>, key: &str) -> Option<String> {
    match env {
        Some(env) => env.get(key).cloned(),
        None => std::env::var(key).ok(),
    }
}

fn parse_js_integer(raw: &str) -> Option<i128> {
    let raw = raw.trim();
    let (negative, unsigned) = raw
        .strip_prefix('-')
        .map_or((false, raw), |unsigned| (true, unsigned));
    let unsigned = unsigned.strip_prefix('+').unwrap_or(unsigned);
    let radix = if unsigned.starts_with("0x") || unsigned.starts_with("0X") {
        16
    } else if unsigned.starts_with("0b") || unsigned.starts_with("0B") {
        2
    } else if unsigned.starts_with("0o") || unsigned.starts_with("0O") {
        8
    } else {
        10
    };
    if radix != 10 {
        let value = i128::from_str_radix(&unsigned[2..], radix).ok()?;
        return Some(if negative { -value } else { value });
    }
    let value = unsigned.parse::<f64>().ok()?;
    if !value.is_finite() || value.fract() != 0.0 || value > i128::MAX as f64 {
        return None;
    }
    let value = value as i128;
    Some(if negative { -value } else { value })
}

fn validation_error(env: &str, expectation: &str, raw: &str) -> WireAsyncError {
    let rendered = serde_json::to_string(raw).unwrap_or_else(|_| format!("\"{raw}\""));
    let mut details = Map::new();
    details.insert("value".to_string(), Value::String(raw.to_string()));
    Box::new(Error2::with_options(
        VALIDATION_FAILED,
        format!("{env} must be {expectation}, got {rendered}."),
        Error2Options {
            details: Some(details),
            ..Default::default()
        },
    ))
}

pub trait ISessionSubagentScopeCacheService: Send + Sync {}
