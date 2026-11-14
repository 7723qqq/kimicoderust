use std::collections::HashMap;
use std::sync::LazyLock;

use parking_lot::Mutex;
use regex::Regex;

use crate::human::llm::requester::undici_npm::{Agent, EnvHttpProxyAgent};
pub use crate::human::llm::requester::undici_npm::Dispatcher;
use crate::wire::record::WireAsyncError;

pub const LLM_HEADERS_TIMEOUT_ENV: &str = "KIMI_CODE_LLM_HEADERS_TIMEOUT_MS";
pub const LLM_BODY_TIMEOUT_ENV: &str = "KIMI_CODE_LLM_BODY_TIMEOUT_MS";

const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
const NO_TIMEOUT_MS: u64 = 0;

pub type Env = HashMap<String, String>;

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'..='\u{000d}'
                | ' '
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

fn parse_js_number(raw: &str) -> Option<f64> {
    let raw = js_trim(raw);
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0b", 2), ("0B", 2), ("0o", 8), ("0O", 8)] {
        if let Some(digits) = raw.strip_prefix(prefix) {
            if digits.is_empty() {
                return None;
            }
            let mut value = 0.0;
            for digit in digits.chars() {
                value = value * f64::from(radix) + f64::from(digit.to_digit(radix)?);
            }
            return Some(value);
        }
    }
    raw.parse::<f64>().ok()
}

pub fn resolve_llm_headers_timeout_ms(env: &Env) -> Result<Option<u64>, WireAsyncError> {
    let Some(raw) = env.get(LLM_HEADERS_TIMEOUT_ENV) else {
        return Ok(None);
    };
    if js_trim(raw).is_empty() {
        return Ok(None);
    }
    let value = parse_js_number(raw).filter(|value| {
        value.is_finite()
            && value.fract() == 0.0
            && *value > 0.0
            && *value <= MAX_TIMEOUT_MS as f64
    });
    let Some(value) = value else {
        return Err(format!(
            "{LLM_HEADERS_TIMEOUT_ENV} must be a positive integer no greater than {MAX_TIMEOUT_MS}, got {}.",
            serde_json::to_string(raw)?
        )
        .into());
    };
    Ok(Some(value as u64))
}

pub fn resolve_llm_body_timeout_ms(env: &Env) -> Result<Option<u64>, WireAsyncError> {
    let Some(raw) = env.get(LLM_BODY_TIMEOUT_ENV) else {
        return Ok(None);
    };
    if js_trim(raw).is_empty() {
        return Ok(None);
    }
    let value = parse_js_number(raw).filter(|value| {
        value.is_finite()
            && value.fract() == 0.0
            && *value >= NO_TIMEOUT_MS as f64
            && *value <= MAX_TIMEOUT_MS as f64
    });
    let Some(value) = value else {
        return Err(format!(
            "{LLM_BODY_TIMEOUT_ENV} must be an integer between {NO_TIMEOUT_MS} and {MAX_TIMEOUT_MS}, got {}.",
            serde_json::to_string(raw)?
        )
        .into());
    };
    Ok(Some(value as u64))
}

fn scheme_of(value: &str) -> Option<String> {
    Regex::new(r"(?i)^([a-z][a-z0-9+.-]*):")
        .unwrap()
        .captures(value)
        .and_then(|captures| captures.get(1).map(|scheme| scheme.as_str().to_lowercase()))
}

fn first_non_blank(env: &Env, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = env.get(*key)
            && !js_trim(value).is_empty()
        {
            return Some(js_trim(value).to_string());
        }
    }
    None
}

fn http_scheme_value(value: Option<String>) -> Option<String> {
    value.filter(|value| {
        !["socks", "socks4", "socks4a", "socks5", "socks5h"]
            .contains(&scheme_of(value).unwrap_or_default().as_str())
    })
}

fn resolve_http_proxy_urls(env: &Env) -> Option<(Option<String>, Option<String>)> {
    let all_proxy = http_scheme_value(first_non_blank(env, &["all_proxy", "ALL_PROXY"]));
    let http_proxy = http_scheme_value(first_non_blank(env, &["http_proxy", "HTTP_PROXY"]))
        .or_else(|| all_proxy.clone());
    let https_proxy = http_scheme_value(first_non_blank(env, &["https_proxy", "HTTPS_PROXY"]))
        .or(all_proxy);
    if http_proxy.is_none() && https_proxy.is_none() {
        return None;
    }
    Some((http_proxy, https_proxy))
}

fn has_socks_proxy(env: &Env) -> bool {
    [
        first_non_blank(env, &["all_proxy", "ALL_PROXY"]),
        first_non_blank(env, &["https_proxy", "HTTPS_PROXY"]),
        first_non_blank(env, &["http_proxy", "HTTP_PROXY"]),
    ]
    .into_iter()
    .flatten()
    .any(|value| {
        ["socks", "socks4", "socks4a", "socks5", "socks5h"]
            .contains(&scheme_of(&value).unwrap_or_default().as_str())
    })
}

fn resolve_no_proxy(env: &Env) -> String {
    let raw = [env.get("no_proxy"), env.get("NO_PROXY")]
        .into_iter()
        .find(|value| value.is_some_and(|value| !js_trim(value).is_empty()))
        .map(String::as_str)
        .unwrap_or("");
    let mut hosts = raw
        .split(',')
        .map(js_trim)
        .filter(|host| !host.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if hosts.contains(&"*".to_string()) {
        return "*".to_string();
    }
    for loopback in ["localhost", "127.0.0.1", "::1", "[::1]"] {
        if !hosts.contains(&loopback.to_string()) {
            hosts.push(loopback.to_string());
        }
    }
    hosts.join(",")
}

static CACHE: LazyLock<Mutex<Option<(String, Option<Dispatcher>)>>> =
    LazyLock::new(|| Mutex::new(None));
static WARNED_SOCKS_PROXY: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));
static WARNED_INVALID_PROXY: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));

pub fn get_llm_dispatcher(env: &Env) -> Result<Option<Dispatcher>, WireAsyncError> {
    let headers_timeout_ms = resolve_llm_headers_timeout_ms(env)?;
    let body_timeout_ms = resolve_llm_body_timeout_ms(env)?;
    if headers_timeout_ms.is_none() && body_timeout_ms.is_none() {
        return Ok(None);
    }
    let key = serde_json::to_string(&json_proxy_key(env, headers_timeout_ms, body_timeout_ms))?;
    if let Some((cached_key, dispatcher)) = CACHE.lock().as_ref()
        && cached_key == &key
    {
        return Ok(dispatcher.clone());
    }
    let mut dispatcher = None;
    if let Some((http_proxy, https_proxy)) = resolve_http_proxy_urls(env) {
        match EnvHttpProxyAgent::new(
            http_proxy.unwrap_or_default(),
            https_proxy.unwrap_or_default(),
            resolve_no_proxy(env),
            headers_timeout_ms,
            body_timeout_ms,
        ) {
            Ok(agent) => dispatcher = Some(Dispatcher::EnvHttpProxyAgent(agent)),
            Err(error) => {
                let mut warned = WARNED_INVALID_PROXY.lock();
                if !*warned {
                    *warned = true;
                    eprintln!(
                        "kimi: {LLM_HEADERS_TIMEOUT_ENV} / {LLM_BODY_TIMEOUT_ENV}: ignoring invalid proxy configuration ({error}); requests keep the default timeouts"
                    );
                }
            }
        }
    } else if has_socks_proxy(env) {
        let mut warned = WARNED_SOCKS_PROXY.lock();
        if !*warned {
            *warned = true;
            eprintln!(
                "kimi: {LLM_HEADERS_TIMEOUT_ENV} / {LLM_BODY_TIMEOUT_ENV} is not supported with SOCKS proxies; requests keep the default timeouts"
            );
        }
    } else {
        dispatcher = Some(Dispatcher::Agent(Agent::new(
            headers_timeout_ms,
            body_timeout_ms,
        )));
    }
    *CACHE.lock() = Some((key, dispatcher.clone()));
    Ok(dispatcher)
}

fn json_proxy_key(
    env: &Env,
    headers_timeout_ms: Option<u64>,
    body_timeout_ms: Option<u64>,
) -> serde_json::Value {
    serde_json::json!([
        headers_timeout_ms,
        body_timeout_ms,
        env.get("http_proxy"),
        env.get("HTTP_PROXY"),
        env.get("https_proxy"),
        env.get("HTTPS_PROXY"),
        env.get("all_proxy"),
        env.get("ALL_PROXY"),
        env.get("no_proxy"),
        env.get("NO_PROXY"),
    ])
}
