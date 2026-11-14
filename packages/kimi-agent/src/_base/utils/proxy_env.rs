use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;

pub type Env = HashMap<String, String>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SocksProxyConfig {
    pub proxy_type: u8,
    pub host: String,
    pub port: u16,
    pub user_id: Option<String>,
    pub password: Option<String>,
}

static SCHEME_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^([a-z][a-z0-9+.-]*):").unwrap());

fn scheme_of(value: &str) -> Option<String> {
    SCHEME_RE
        .captures(value)
        .and_then(|captures| captures.get(1))
        .map(|scheme| scheme.as_str().to_lowercase())
}

fn first_non_blank(env: &Env, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = env.get(*key)
            && !value.trim().is_empty()
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn is_socks_scheme(scheme: &str) -> bool {
    matches!(scheme, "socks" | "socks4" | "socks4a" | "socks5" | "socks5h")
}

fn http_scheme_value(value: Option<String>) -> Option<String> {
    value.filter(|value| {
        !scheme_of(value).is_some_and(|scheme| is_socks_scheme(&scheme))
    })
}

pub fn has_http_proxy(env: &Env) -> bool {
    [
        first_non_blank(env, &["http_proxy", "HTTP_PROXY"]),
        first_non_blank(env, &["https_proxy", "HTTPS_PROXY"]),
        first_non_blank(env, &["all_proxy", "ALL_PROXY"]),
    ]
    .into_iter()
    .any(|value| http_scheme_value(value).is_some())
}

#[derive(Clone, Debug, Default)]
pub struct HttpProxyUrls {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
}

pub fn resolve_http_proxy_urls(env: &Env) -> HttpProxyUrls {
    let all_proxy = http_scheme_value(first_non_blank(env, &["all_proxy", "ALL_PROXY"]));
    HttpProxyUrls {
        http_proxy: http_scheme_value(first_non_blank(env, &["http_proxy", "HTTP_PROXY"]))
            .or_else(|| all_proxy.clone()),
        https_proxy: http_scheme_value(first_non_blank(env, &["https_proxy", "HTTPS_PROXY"]))
            .or(all_proxy),
    }
}

pub fn resolve_socks_proxy(env: &Env) -> Option<SocksProxyConfig> {
    let candidates = [
        first_non_blank(env, &["all_proxy", "ALL_PROXY"]),
        first_non_blank(env, &["https_proxy", "HTTPS_PROXY"]),
        first_non_blank(env, &["http_proxy", "HTTP_PROXY"]),
    ];
    for value in candidates.into_iter().flatten() {
        let Some(scheme) = scheme_of(&value) else {
            continue;
        };
        if !is_socks_scheme(&scheme) {
            continue;
        }
        let Ok(url) = url::Url::parse(&value) else {
            continue;
        };
        return Some(SocksProxyConfig {
            proxy_type: if matches!(scheme.as_str(), "socks4" | "socks4a") {
                4
            } else {
                5
            },
            host: url
                .host_str()
                .unwrap_or_default()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            port: url.port().unwrap_or(1080),
            user_id: (!url.username().is_empty()).then(|| decode_uri_component(url.username())),
            password: url
                .password()
                .filter(|password| !password.is_empty())
                .map(decode_uri_component),
        });
    }
    None
}

fn decode_uri_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                output.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub fn is_proxy_configured(env: &Env) -> bool {
    has_http_proxy(env) || resolve_socks_proxy(env).is_some()
}
