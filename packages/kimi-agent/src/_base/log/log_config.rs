use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use once_cell::sync::Lazy;

use super::log::LogLevel;
use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::scope::ScopeSeed;
use crate::_base::di::service_collection::ServiceValue;

pub const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Info;
pub const DEFAULT_GLOBAL_MAX_BYTES: u64 = 6 * 1024 * 1024;
pub const DEFAULT_GLOBAL_FILES: u64 = 5;
pub const DEFAULT_SESSION_MAX_BYTES: u64 = 5 * 1024 * 1024;
pub const DEFAULT_SESSION_FILES: u64 = 3;

#[derive(Clone)]
pub struct LoggingConfig {
    pub level: LogLevel,
    pub global_log_path: String,
    pub global_max_bytes: u64,
    pub global_files: u64,
    pub session_max_bytes: u64,
    pub session_files: u64,
}

pub type ILogOptions = LoggingConfig;

pub static I_LOG_OPTIONS: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("logOptions"));

pub struct ResolveLoggingInput {
    pub home_dir: String,
    pub env: HashMap<String, String>,
}

pub fn resolve_global_log_path(home_dir: &str) -> String {
    Path::new(home_dir)
        .join("logs")
        .join("kimi-code.log")
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn resolve_session_log_path(session_dir: &str) -> String {
    Path::new(session_dir)
        .join("logs")
        .join("kimi-code.log")
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn resolve_logging_config(input: ResolveLoggingInput) -> LoggingConfig {
    LoggingConfig {
        level: input
            .env
            .get("KIMI_LOG_LEVEL")
            .and_then(|value| parse_level(value))
            .unwrap_or(DEFAULT_LOG_LEVEL),
        global_log_path: resolve_global_log_path(&input.home_dir),
        global_max_bytes: input
            .env
            .get("KIMI_LOG_GLOBAL_MAX_BYTES")
            .and_then(|value| parse_positive_int(value))
            .unwrap_or(DEFAULT_GLOBAL_MAX_BYTES),
        global_files: input
            .env
            .get("KIMI_LOG_GLOBAL_FILES")
            .and_then(|value| parse_positive_int(value))
            .unwrap_or(DEFAULT_GLOBAL_FILES),
        session_max_bytes: input
            .env
            .get("KIMI_LOG_SESSION_MAX_BYTES")
            .and_then(|value| parse_positive_int(value))
            .unwrap_or(DEFAULT_SESSION_MAX_BYTES),
        session_files: input
            .env
            .get("KIMI_LOG_SESSION_FILES")
            .and_then(|value| parse_positive_int(value))
            .unwrap_or(DEFAULT_SESSION_FILES),
    }
}

pub fn log_seed(config: LoggingConfig) -> ScopeSeed {
    vec![(
        I_LOG_OPTIONS.clone(),
        ServiceValue::Instance(Arc::new(config) as DynamicValue),
    )]
}

fn parse_level(value: &str) -> Option<LogLevel> {
    match value.trim().to_lowercase().as_str() {
        "off" => Some(LogLevel::Off),
        "error" => Some(LogLevel::Error),
        "warn" => Some(LogLevel::Warn),
        "info" => Some(LogLevel::Info),
        "debug" => Some(LogLevel::Debug),
        _ => None,
    }
}

fn parse_positive_int(value: &str) -> Option<u64> {
    let value = value.trim_start();
    let value = value.strip_prefix('+').unwrap_or(value);
    let digits = value
        .char_indices()
        .take_while(|(_, character)| character.is_ascii_digit())
        .last()
        .map(|(index, character)| &value[..=index]);
    digits?.parse::<u64>().ok().filter(|value| *value > 0)
}
