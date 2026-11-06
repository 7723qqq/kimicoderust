//! Argument validation and URL/query/body building (split out of `github.rs`).

use serde_json::{Value, json};

use super::specs::{FieldKind, GitHubToolSpec};
use super::{GITHUB_DEFAULT_BASE_URL, GitHubCredentials};

/// Token resolution mirrors v2 `envOverlay.ts`: the config `[github].token`
/// wins, then `GITHUB_TOKEN`, then `GH_TOKEN`; empty/whitespace-only values
/// count as unset.
pub(super) fn resolve_token(credentials: Option<&GitHubCredentials>) -> Option<String> {
    if let Some(value) = credentials.and_then(|c| c.token.as_deref()) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    for name in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Base URL mirrors v2 `envOverlay.ts`: the config `[github].baseUrl` wins,
/// then `GITHUB_API_URL`, then the default. Trailing slashes are stripped
/// like v2 `buildGitHubUrl`.
pub(super) fn resolve_base_url(credentials: Option<&GitHubCredentials>) -> String {
    let mut configured: Option<String> = None;
    if let Some(value) = credentials.and_then(|c| c.base_url.as_deref()) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            configured = Some(trimmed.to_string());
        }
    }
    if configured.is_none()
        && let Ok(value) = std::env::var("GITHUB_API_URL")
    {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            configured = Some(trimmed.to_string());
        }
    }
    configured
        .unwrap_or_else(|| GITHUB_DEFAULT_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

// ── Argument validation (v2 zod `safeParse` semantics) ───────────────────────

/// Returns the v2-style `Invalid arguments for {name}: ...` payload, or
/// `None` when the args are valid. Issues mirror zod v4's shape
/// (`code`/`expected`/`received`/`path`/`message`).
pub(super) fn validate_args(spec: &GitHubToolSpec, args: &Value) -> Option<String> {
    let mut issues: Vec<Value> = Vec::new();
    for field in spec.fields {
        let Some(value) = args.get(field.name) else {
            if field.required {
                issues.push(invalid_type_issue(
                    field.name,
                    expected_type(field.kind),
                    "undefined",
                ));
            }
            continue;
        };
        if value.is_null() {
            if field.required {
                issues.push(invalid_type_issue(
                    field.name,
                    expected_type(field.kind),
                    "null",
                ));
            }
            continue;
        }
        match field.kind {
            FieldKind::Str | FieldKind::StrAny => {
                if let Value::String(s) = value {
                    if field.kind == FieldKind::Str && s.is_empty() {
                        issues.push(too_small_issue(field.name, 1));
                    }
                } else {
                    issues.push(invalid_type_issue(
                        field.name,
                        "string",
                        received_type(value),
                    ));
                }
            }
            FieldKind::Int => {
                if value.as_i64().is_none() {
                    issues.push(invalid_type_issue(
                        field.name,
                        "number",
                        received_type(value),
                    ));
                }
            }
            FieldKind::Bool => {
                if value.as_bool().is_none() {
                    issues.push(invalid_type_issue(
                        field.name,
                        "boolean",
                        received_type(value),
                    ));
                }
            }
            FieldKind::StrArray => {
                let valid = value
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string));
                if !valid {
                    issues.push(invalid_type_issue(
                        field.name,
                        "array",
                        received_type(value),
                    ));
                }
            }
            FieldKind::TreeArray => {
                if !value.is_array() {
                    issues.push(invalid_type_issue(
                        field.name,
                        "array",
                        received_type(value),
                    ));
                }
            }
        }
    }
    if issues.is_empty() {
        None
    } else {
        Some(serde_json::to_string_pretty(&issues).unwrap_or_default())
    }
}

fn expected_type(kind: FieldKind) -> &'static str {
    match kind {
        FieldKind::Str | FieldKind::StrAny => "string",
        FieldKind::Int => "number",
        FieldKind::Bool => "boolean",
        FieldKind::StrArray | FieldKind::TreeArray => "array",
    }
}

fn received_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn invalid_type_issue(path: &str, expected: &str, received: &str) -> Value {
    json!({
        "code": "invalid_type",
        "expected": expected,
        "received": received,
        "path": [path],
        "message": format!("Invalid input: expected {expected}, received {received}"),
    })
}

fn too_small_issue(path: &str, minimum: i64) -> Value {
    json!({
        "code": "too_small",
        "minimum": minimum,
        "inclusive": true,
        "path": [path],
        "message": format!("Too small: expected string to have >={minimum} characters"),
    })
}

// ── URL / query / body building (v2 path/query/body fns) ────────────────────

pub(super) fn build_url(spec: &GitHubToolSpec, args: &Value, base_url: &str) -> String {
    let mut path = String::new();
    let mut rest = spec.path;
    while let Some(start) = rest.find('{') {
        path.push_str(&rest[..start]);
        let end = rest[start..]
            .find('}')
            .map(|offset| start + offset)
            .expect("unterminated path placeholder");
        let key = &rest[start + 1..end];
        path.push_str(&encode_path_component(
            &arg_string(args, key).unwrap_or_default(),
        ));
        rest = &rest[end + 1..];
    }
    path.push_str(rest);
    format!("{base_url}{path}")
}

pub(super) fn build_query(spec: &GitHubToolSpec, args: &Value) -> Option<String> {
    let mut pairs: Vec<String> = Vec::new();
    for (query_key, arg_key) in spec.query {
        let Some(value) = arg_string(args, arg_key) else {
            continue;
        };
        pairs.push(format!(
            "{}={}",
            encode_uri_component(query_key),
            encode_uri_component(&value)
        ));
    }
    if pairs.is_empty() {
        None
    } else {
        Some(format!("?{}", pairs.join("&")))
    }
}

pub(super) fn build_body(spec: &GitHubToolSpec, args: &Value) -> Option<Value> {
    let mut map = serde_json::Map::new();
    for (body_key, arg_key) in spec.body {
        let Some(value) = args.get(*arg_key) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        map.insert((*body_key).to_string(), value.clone());
    }
    for (body_key, arg_key) in spec.body_base64 {
        let Some(value) = arg_string(args, arg_key) else {
            continue;
        };
        map.insert(
            (*body_key).to_string(),
            Value::String(base64_encode(value.as_bytes())),
        );
    }
    for (body_key, arg_key, prefix) in spec.body_prefix {
        let Some(value) = arg_string(args, arg_key) else {
            continue;
        };
        map.insert(
            (*body_key).to_string(),
            Value::String(format!("{prefix}{value}")),
        );
    }
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}

pub(super) fn arg_string(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

/// JS `encodeURIComponent`: encodes everything except `A-Za-z0-9-_.!~*'()`.
pub(super) fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.' | b'_' | b'~' | b'!' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// WHATWG path percent-encode set — what fetch applies to the interpolated
/// path in v2. Encodes space, `"`, `#`, `<`, `>`, `?`, backtick, `{`, `}`,
/// and non-ASCII/control bytes; existing `%` escapes pass through.
pub(super) fn encode_path_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
                    | b'/'
                    | b'%'
            )
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Standard base64 (RFC 4648) — v2 `Buffer.from(content, 'utf8').toString('base64')`.
pub(super) fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}
