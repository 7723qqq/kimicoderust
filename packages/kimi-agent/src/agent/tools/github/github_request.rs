use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use crate::subagent::types::ParentCancel;

pub const GITHUB_DEFAULT_BASE_URL: &str = "https://api.github.com";
pub const GITHUB_API_VERSION: &str = "2022-11-28";
pub const GITHUB_USER_AGENT: &str = "kimi-code";
pub const GITHUB_DEFAULT_ACCEPT: &str = "application/vnd.github+json";
pub const GITHUB_REQUEST_TIMEOUT_MS: u64 = 30_000;
pub const GITHUB_MAX_RESPONSE_BODY_BYTES: usize = 5 * 1024 * 1024;
pub const GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES: usize = 200 * 1024;
pub const GITHUB_NO_TOKEN_ERROR: &str = "No GitHub token configured. Set token in the [github] section of config.toml, or set the GITHUB_TOKEN (or GH_TOKEN) environment variable.";

#[derive(Clone, Debug, Default)]
pub struct GitHubRequestOptions {
    pub query: Option<Vec<(String, Value)>>,
    pub body: Option<Value>,
    pub accept: Option<String>,
    pub token: Option<String>,
    pub base_url: Option<String>,
    pub max_bytes: Option<usize>,
}

#[derive(Clone, Default)]
pub struct GitHubRequestDeps {
    pub client: Option<reqwest::Client>,
    pub signal: Option<ParentCancel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubRequestResult {
    pub status: u16,
    pub ok: bool,
    pub body: String,
    pub error: Option<String>,
    pub rate_remaining: Option<i64>,
    pub next_page: bool,
    pub truncated_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitHubRequestAborted;

impl std::fmt::Display for GitHubRequestAborted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GitHub request aborted")
    }
}

impl std::error::Error for GitHubRequestAborted {}

fn network_error(error: impl std::fmt::Display) -> GitHubRequestResult {
    GitHubRequestResult {
        status: 0,
        ok: false,
        body: String::new(),
        error: Some(format!("network error: {error}")),
        rate_remaining: None,
        next_page: false,
        truncated_bytes: 0,
    }
}

pub async fn github_request(
    method: impl AsRef<str> + Send,
    path: impl AsRef<str> + Send,
    options: GitHubRequestOptions,
    deps: GitHubRequestDeps,
) -> Result<GitHubRequestResult, GitHubRequestAborted> {
    let Some(token) = first_non_empty([options.token.as_deref()]) else {
        return Ok(GitHubRequestResult {
            status: 0,
            ok: false,
            body: String::new(),
            error: Some(GITHUB_NO_TOKEN_ERROR.to_string()),
            rate_remaining: None,
            next_page: false,
            truncated_bytes: 0,
        });
    };
    let base_url = first_non_empty([options.base_url.as_deref(), Some(GITHUB_DEFAULT_BASE_URL)])
        .unwrap_or(GITHUB_DEFAULT_BASE_URL);
    let max_bytes = response_body_limit(options.max_bytes);
    let url = format!(
        "{}{}",
        build_github_url(base_url, path.as_ref()),
        build_github_query(options.query.as_deref())
    );
    let client = match deps.client {
        Some(client) => client,
        None => match reqwest::Client::builder().build() {
            Ok(client) => client,
            Err(error) => return Ok(network_error(error)),
        },
    };
    let method = match reqwest::Method::from_bytes(method.as_ref().as_bytes()) {
        Ok(method) => method,
        Err(error) => return Ok(network_error(error)),
    };
    let mut request = client
        .request(method, &url)
        .header("Authorization", format!("Bearer {token}"))
        .header(
            "Accept",
            options.accept.as_deref().unwrap_or(GITHUB_DEFAULT_ACCEPT),
        )
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .header("User-Agent", GITHUB_USER_AGENT);
    if let Some(body) = options.body {
        request = request
            .header("Content-Type", "application/json")
            .body(js_json_stringify(&body));
    }
    let operation = async move {
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => return network_error(error),
        };
        let status = response.status().as_u16();
        let rate_remaining = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_rate_limit_remaining);
        let next_page = response
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|value| value.to_str().ok())
            .is_some_and(has_next_page);
        let mut bytes = Vec::new();
        let mut truncated_bytes = 0;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    truncated_bytes += append_response_chunk(&mut bytes, &chunk, max_bytes);
                    if truncated_bytes > 0 {
                        break;
                    }
                }
                Err(error) => return network_error(error),
            }
        }
        let body = render_response_body(&bytes, truncated_bytes);
        if !(200..300).contains(&status) {
            return GitHubRequestResult {
                status,
                ok: false,
                body,
                error: Some(format!("GitHub API error {status}")),
                rate_remaining,
                next_page,
                truncated_bytes,
            };
        }
        GitHubRequestResult {
            status,
            ok: true,
            body,
            error: None,
            rate_remaining,
            next_page,
            truncated_bytes,
        }
    };
    let timeout = Duration::from_millis(GITHUB_REQUEST_TIMEOUT_MS);
    let outcome = match deps.signal {
        Some(signal) => {
            if signal.triggered() {
                return Err(GitHubRequestAborted);
            }
            tokio::select! {
                outcome = tokio::time::timeout(timeout, operation) => outcome,
                () = signal.wait() => return Err(GitHubRequestAborted),
            }
        }
        None => tokio::time::timeout(timeout, operation).await,
    };
    match outcome {
        Ok(result) => Ok(result),
        Err(_) => Ok(network_error(format!(
            "GitHub request timed out after {}s",
            GITHUB_REQUEST_TIMEOUT_MS / 1000
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn response_headers_and_diff_size_are_preserved_on_a_local_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 512];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before sending complete headers");
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            let accept = request
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("accept"))
                .map(|(_, value)| value.trim());
            assert_eq!(accept, Some("application/vnd.github.diff"));
            let body = "x".repeat(GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES + 7);
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nLink: <http://{address}/next>; rel=\"next\"\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let result = github_request(
            "GET",
            "/repos/example/project/pulls/1",
            GitHubRequestOptions {
                accept: Some("application/vnd.github.diff".into()),
                token: Some("YOUR_API_KEY".into()),
                base_url: Some(format!("http://{address}")),
                max_bytes: Some(GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES),
                ..GitHubRequestOptions::default()
            },
            GitHubRequestDeps::default(),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert!(result.ok);
        assert!(result.next_page);
        assert_eq!(result.truncated_bytes, 7);
        assert!(
            result
                .body
                .starts_with(&"x".repeat(GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES))
        );
        assert!(result.body.ends_with("[...truncated 7+ bytes]"));
    }

    #[test]
    fn pagination_detects_next_relation_across_link_parameters() {
        assert!(has_next_page(
            "<https://example.test/?page=2>; rel=\"next\""
        ));
        assert!(has_next_page(
            "<https://example.test/?page=2>; type=\"application/json\"; rel=\"next\", <https://example.test/?page=1>; rel=\"prev\""
        ));
        assert!(!has_next_page(
            "<https://example.test/?page=1>; rel=\"prev\""
        ));
        assert!(!has_next_page(
            "<https://example.test/?page=2>; title=\"next\""
        ));
    }

    #[test]
    fn requested_body_limit_never_exceeds_the_global_limit() {
        assert_eq!(GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES, 200_000);
        assert_eq!(
            response_body_limit(Some(GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES)), 
            200 * 1024
        );
        assert_eq!(
            response_body_limit(Some(GITHUB_MAX_RESPONSE_BODY_BYTES * 2)),
            GITHUB_MAX_RESPONSE_BODY_BYTES
        );
        assert_eq!(response_body_limit(None), GITHUB_MAX_RESPONSE_BODY_BYTES);
    }

    #[test]
    fn a_large_diff_is_truncated_at_its_request_limit() {
        let mut bytes = Vec::new();
        let max_bytes = GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES;
        let truncated = append_response_chunk(&mut bytes, &vec![b'x'; max_bytes + 5], max_bytes);
        let body = render_response_body(&bytes, truncated);
        assert_eq!(bytes.len(), max_bytes);
        assert_eq!(truncated, 5);
        assert!(body.ends_with("[...truncated 5+ bytes]"));
    }

    #[test]
    fn a_multibyte_chunk_is_bounded_in_bytes() {
        let mut bytes = Vec::new();
        let truncated = append_response_chunk(&mut bytes, "éé".as_bytes(), 3);
        assert_eq!(bytes, "éé".as_bytes()[..3]);
        assert_eq!(truncated, 1);
        assert!(render_response_body(&bytes, truncated).contains("truncated 1+ bytes"));
    }
}

fn response_body_limit(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(GITHUB_MAX_RESPONSE_BODY_BYTES)
        .min(GITHUB_MAX_RESPONSE_BODY_BYTES)
}

fn append_response_chunk(bytes: &mut Vec<u8>, chunk: &[u8], max_bytes: usize) -> usize {
    let remaining = max_bytes.saturating_sub(bytes.len());
    let retained = chunk.len().min(remaining);
    bytes.extend_from_slice(&chunk[..retained]);
    chunk.len() - retained
}

fn render_response_body(bytes: &[u8], truncated_bytes: usize) -> String {
    let mut body = String::from_utf8_lossy(bytes).into_owned();
    if truncated_bytes > 0 {
        body.push_str(&format!("\n[...truncated {truncated_bytes}+ bytes]"));
    }
    body
}

fn has_next_page(link: &str) -> bool {
    link.split(',').any(|entry| {
        entry.split(';').any(|parameter| {
            parameter
                .trim()
                .strip_prefix("rel=")
                .is_some_and(|relation| relation.trim_matches(['"', '\'']) == "next")
        })
    })
}

fn first_non_empty<const N: usize>(values: [Option<&str>; N]) -> Option<&str> {
    values.into_iter().flatten().find(|value| !value.is_empty())
}

fn parse_rate_limit_remaining(raw: &str) -> Option<i64> {
    let value = parse_js_number(raw)?;
    if !value.is_finite()
        || value.fract() != 0.0
        || value < i64::MIN as f64
        || value >= i64::MAX as f64
    {
        return None;
    }
    Some(value as i64)
}

fn parse_js_number(raw: &str) -> Option<f64> {
    let raw = raw.trim_matches(|character| {
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
    });
    if raw.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
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

fn build_github_url(base: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    let base = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

fn build_github_query(query: Option<&[(String, Value)]>) -> String {
    let pairs = query
        .unwrap_or_default()
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| {
            format!(
                "{}={}",
                encode_uri_component(key),
                encode_uri_component(&js_string(value))
            )
        })
        .collect::<Vec<_>>();
    if pairs.is_empty() {
        String::new()
    } else {
        format!("?{}", pairs.join("&"))
    }
}

fn js_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => js_number_to_string(value),
        Value::Bool(value) => value.to_string(),
        Value::Array(values) => values
            .iter()
            .map(|value| match value {
                Value::Null => String::new(),
                value => js_string(value),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
        Value::Null => String::new(),
    }
}

fn js_json_stringify(value: &Value) -> String {
    match value {
        Value::Number(value) => js_number_to_string(value),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(js_json_stringify)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()),
                        js_json_stringify(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
    }
}

fn js_number_to_string(value: &serde_json::Number) -> String {
    let Some(value) = value.as_f64() else {
        return value.to_string();
    };
    if value == 0.0 {
        return "0".to_string();
    }
    let negative = value.is_sign_negative();
    let Some(value) = serde_json::Number::from_f64(value.abs()) else {
        return "null".to_string();
    };
    let raw = value.to_string();
    let exponent_at = raw.find('e').or_else(|| raw.find('E'));
    let (mantissa, exponent) = match exponent_at {
        Some(index) => (&raw[..index], raw[index + 1..].parse::<i32>().unwrap_or(0)),
        None => (raw.as_str(), 0),
    };
    let mut decimal_position = mantissa.find('.').unwrap_or(mantissa.len()) as i32 + exponent;
    let mut digits = mantissa.replace('.', "");
    let leading_zeros = digits.bytes().take_while(|byte| *byte == b'0').count();
    if leading_zeros > 0 {
        digits.drain(..leading_zeros);
        decimal_position -= leading_zeros as i32;
    }
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let length = digits.len() as i32;
    let mut output = String::new();
    if negative {
        output.push('-');
    }
    if length <= decimal_position && decimal_position <= 21 {
        output.push_str(&digits);
        output.extend(std::iter::repeat_n(
            '0',
            (decimal_position - length) as usize,
        ));
    } else if decimal_position > 0 && decimal_position <= 21 {
        let split = decimal_position as usize;
        output.push_str(&digits[..split]);
        output.push('.');
        output.push_str(&digits[split..]);
    } else if decimal_position <= 0 && decimal_position > -6 {
        output.push_str("0.");
        output.extend(std::iter::repeat_n('0', (-decimal_position) as usize));
        output.push_str(&digits);
    } else {
        output.push_str(&digits[..1]);
        if digits.len() > 1 {
            output.push('.');
            output.push_str(&digits[1..]);
        }
        let exponent = decimal_position - 1;
        if exponent >= 0 {
            output.push_str(&format!("e+{exponent}"));
        } else {
            output.push_str(&format!("e{exponent}"));
        }
    }
    output
}

fn encode_uri_component(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.' | b'_' | b'~' | b'!' | b'*' | b'\'' | b'(' | b')'
            )
        {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}
