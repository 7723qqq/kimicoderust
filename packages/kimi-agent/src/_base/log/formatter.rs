use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;

use super::log::{LogContext, LogEntry, LogEntryError, LogEntryLevel};

pub const MSG_MAX_CHARS: usize = 200;
pub const CTX_VALUE_MAX_CHARS: usize = 2048;
pub const STACK_MAX_BYTES: usize = 2048;
pub const ENTRY_MAX_BYTES: usize = 4096;
pub const REDACT_MAX_DEPTH: usize = 10;

const TRUNCATED_TAIL: &str = " …truncated";
const REDACTED: &str = "[REDACTED]";

static REDACTED_KEYS: Lazy<HashSet<String>> = Lazy::new(|| {
    [
        "authorization",
        "apikey",
        "token",
        "refreshtoken",
        "accesstoken",
        "idtoken",
        "password",
        "secret",
        "clientsecret",
        "apisecret",
        "cookie",
        "setcookie",
        "bearer",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
});

static SAFE_KEY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z0-9_.\-]+$").unwrap());
static QUOTE_REQUIRED_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"[\s="\\]"#).unwrap());
static RAW_SECRET_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r#"(?i)\b(authorization\s*[:=]\s*bearer\s+)[^\s"'`]+"#).unwrap(),
        Regex::new(r#"(?i)\b((?:api[_-]?key|access[_-]?token|refresh[_-]?token|id[_-]?token|token|password|secret)\s*[:=]\s*)[^\s"'`]+"#).unwrap(),
        Regex::new(r"(?i)\b(cookie\s*[:=]\s*)[^\r\n]+").unwrap(),
    ]
});

fn normalize_key(key: &str) -> String {
    key.to_lowercase()
        .chars()
        .filter(|character| !matches!(character, '_' | '-' | '.'))
        .collect()
}

pub fn redact_ctx(ctx: &LogContext) -> LogContext {
    ctx.iter()
        .map(|(key, value)| {
            (
                key.clone(),
                if REDACTED_KEYS.contains(&normalize_key(key)) {
                    serde_json::Value::String(REDACTED.to_string())
                } else {
                    redact_value(value, 1)
                },
            )
        })
        .collect()
}

fn redact_value(value: &serde_json::Value, depth: usize) -> serde_json::Value {
    if depth > REDACT_MAX_DEPTH {
        return serde_json::Value::String("[REDACTED:depth]".to_string());
    }
    match value {
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| redact_value(value, depth + 1))
                .collect(),
        ),
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        if REDACTED_KEYS.contains(&normalize_key(key)) {
                            serde_json::Value::String(REDACTED.to_string())
                        } else {
                            redact_value(value, depth + 1)
                        },
                    )
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}

#[derive(Default)]
pub struct FormatOptions {
    pub ansi: bool,
    pub omit_context_keys: Vec<String>,
}

pub struct FormattedEntry {
    pub text: String,
    pub dropped: bool,
}

fn truncate(value: &str, max: usize) -> String {
    let units = value.encode_utf16().collect::<Vec<_>>();
    if units.len() <= max {
        value.to_string()
    } else {
        format!("{}…", String::from_utf16_lossy(&units[..max - 1]))
    }
}

fn serialize_value(raw: &serde_json::Value) -> String {
    match raw {
        serde_json::Value::String(value) => redact_string(value),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        _ => serde_json::to_string(raw).unwrap_or_else(|_| "[object Object]".to_string()),
    }
}

fn redact_string(value: &str) -> String {
    let mut out = value.to_string();
    for pattern in RAW_SECRET_PATTERNS.iter() {
        out = pattern
            .replace_all(&out, format!("${{1}}{REDACTED}"))
            .into_owned();
    }
    out
}

fn quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

fn format_pair(key: &str, raw: &serde_json::Value) -> String {
    let limited = truncate(&serialize_value(raw), CTX_VALUE_MAX_CHARS);
    let rendered_key = if SAFE_KEY_RE.is_match(key) {
        key.to_string()
    } else {
        quote(key)
    };
    let rendered_value = if QUOTE_REQUIRED_RE.is_match(&limited) || limited.is_empty() {
        quote(&limited)
    } else {
        limited
    };
    format!("{rendered_key}={rendered_value}")
}

fn clip_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let units = text.encode_utf16().collect::<Vec<_>>();
    let budget = max_bytes.saturating_sub(TRUNCATED_TAIL.len());
    let mut low = 0;
    let mut high = units.len();
    while low < high {
        let middle = (low + high + 1) >> 1;
        if String::from_utf16_lossy(&units[..middle]).len() <= budget {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    format!("{}{TRUNCATED_TAIL}", String::from_utf16_lossy(&units[..low]))
}

fn indent_stack(stack: &str) -> String {
    stack
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                format!("  {line}")
            } else {
                format!("    {}", line.trim_start())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_entry(entry: &LogEntry, options: FormatOptions) -> FormattedEntry {
    let ctx = entry.ctx.as_ref().map(redact_ctx);
    let omit = options.omit_context_keys.into_iter().collect::<HashSet<_>>();
    let msg = truncate(&entry.msg, MSG_MAX_CHARS);
    let mut pairs = Vec::new();
    if let Some(ctx) = ctx {
        for (key, value) in ctx {
            if !omit.contains(&key) {
                pairs.push(format_pair(&key, &value));
            }
        }
    }
    let timestamp = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(entry.t as i64)
        .expect("log timestamp is outside the supported range")
        .format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let label = match entry.level {
        LogEntryLevel::Error => "ERROR",
        LogEntryLevel::Warn => "WARN ",
        LogEntryLevel::Info => "INFO ",
        LogEntryLevel::Debug => "DEBUG",
    };
    let rendered = if pairs.is_empty() {
        format!("{timestamp} {label} {msg}")
    } else {
        format!("{timestamp} {label} {msg}  {}", pairs.join(" "))
    };
    let mut head = clip_bytes(&rendered, ENTRY_MAX_BYTES);
    if options.ansi {
        let color = match entry.level {
            LogEntryLevel::Error => "\u{1b}[31m",
            LogEntryLevel::Warn => "\u{1b}[33m",
            LogEntryLevel::Info => "\u{1b}[36m",
            LogEntryLevel::Debug => "\u{1b}[90m",
        };
        head = format!("{color}{head}\u{1b}[0m");
    }
    if let Some(error) = &entry.error {
        if let Some(stack) = error.stack.as_ref().filter(|stack| !stack.is_empty()) {
            head = format!(
                "{head}\n{}",
                indent_stack(&clip_bytes(&redact_string(stack), STACK_MAX_BYTES))
            );
        } else if !error.message.is_empty() {
            head = format!("{head}\n  Error: {}", redact_string(&error.message));
        }
    }
    FormattedEntry {
        text: head,
        dropped: false,
    }
}

pub fn extract_error(error: &dyn std::error::Error) -> LogEntryError {
    LogEntryError {
        message: error.to_string(),
        stack: Some(std::backtrace::Backtrace::capture().to_string()),
    }
}

pub fn extract_error_with_stack(error: &dyn std::error::Error, stack: Option<String>) -> LogEntryError {
    LogEntryError {
        message: error.to_string(),
        stack,
    }
}
