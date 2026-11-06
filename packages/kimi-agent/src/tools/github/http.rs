//! HTTP transport for the GitHub tools (split out of `github.rs`).

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use super::super::{err_result, ok_result};
use super::specs::GitHubToolSpec;
use crate::turn_loop::types::ExecutableToolResult;

const GITHUB_API_VERSION: &str = "2022-11-28";
const GITHUB_USER_AGENT: &str = "kimi-code";
const GITHUB_DEFAULT_ACCEPT: &str = "application/vnd.github+json";
const GITHUB_REQUEST_TIMEOUT_SECS: u64 = 30;
const GITHUB_MAX_RESPONSE_BODY_BYTES: usize = 5 * 1024 * 1024;

// ── HTTP request (the fork's own v1 transport semantics) ────────────────────

pub(super) struct GitHubResponse {
    status: u16,
    ok: bool,
    body: String,
    error: Option<String>,
    rate_remaining: Option<i64>,
}

pub(super) async fn github_request(
    spec: &GitHubToolSpec,
    url: String,
    query: Option<String>,
    body: Option<Value>,
    token: &str,
) -> GitHubResponse {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(GITHUB_REQUEST_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return GitHubResponse {
                status: 0,
                ok: false,
                body: String::new(),
                error: Some(format!("network error: {e}")),
                rate_remaining: None,
            };
        }
    };
    let full_url = format!("{url}{}", query.unwrap_or_default());
    let method =
        reqwest::Method::from_bytes(spec.method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut request = client.request(method, &full_url);
    request = request
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", spec.accept.unwrap_or(GITHUB_DEFAULT_ACCEPT))
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .header("User-Agent", GITHUB_USER_AGENT);
    if let Some(body) = body {
        request = request
            .header("Content-Type", "application/json")
            .body(body.to_string());
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            let error = if e.is_timeout() {
                format!(
                    "network error: GitHub request timed out after {GITHUB_REQUEST_TIMEOUT_SECS}s"
                )
            } else {
                format!("network error: {e}")
            };
            return GitHubResponse {
                status: 0,
                ok: false,
                body: String::new(),
                error: Some(error),
                rate_remaining: None,
            };
        }
    };
    let status = response.status().as_u16();
    let rate_remaining = response
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    if let Some(content_length) = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        && content_length > GITHUB_MAX_RESPONSE_BODY_BYTES
    {
        return GitHubResponse {
            status,
            ok: false,
            body: String::new(),
            error: Some(format!(
                "GitHub API response exceeds the {GITHUB_MAX_RESPONSE_BODY_BYTES} byte limit"
            )),
            rate_remaining,
        };
    }
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > GITHUB_MAX_RESPONSE_BODY_BYTES {
                    return GitHubResponse {
                        status,
                        ok: false,
                        body: String::new(),
                        error: Some(format!(
                            "GitHub API response exceeds the {GITHUB_MAX_RESPONSE_BODY_BYTES} byte limit"
                        )),
                        rate_remaining,
                    };
                }
            }
            Err(e) => {
                return GitHubResponse {
                    status: 0,
                    ok: false,
                    body: String::new(),
                    error: Some(format!("network error: {e}")),
                    rate_remaining: None,
                };
            }
        }
    }
    let body_text = String::from_utf8_lossy(&bytes).into_owned();
    if !(200..300).contains(&status) {
        return GitHubResponse {
            status,
            ok: false,
            body: body_text,
            error: Some(format!("GitHub API error {status}")),
            rate_remaining,
        };
    }
    GitHubResponse {
        status,
        ok: true,
        body: body_text,
        error: None,
        rate_remaining,
    }
}

/// Result shaping mirrors v2 `GitHubToolBase.execute`: errors are
/// `{error} (status {code})\n{body前4000字符}`, successes are
/// `{body}\n\n(GitHub rate limit remaining: {n})` with `(empty response)`
/// for an empty body.
pub(super) fn format_result(response: GitHubResponse) -> ExecutableToolResult {
    if !response.ok {
        let detail = if response.body.is_empty() {
            String::new()
        } else {
            format!("\n{}", truncate_chars(&response.body, 4000))
        };
        let status = if response.status > 0 {
            format!(" (status {})", response.status)
        } else {
            String::new()
        };
        return err_result(format!(
            "{}{status}{detail}",
            response
                .error
                .unwrap_or_else(|| "GitHub request failed".to_string())
        ));
    }
    let rate = match response.rate_remaining {
        Some(n) => format!("\n\n(GitHub rate limit remaining: {n})"),
        None => String::new(),
    };
    let body = if response.body.is_empty() {
        "(empty response)".to_string()
    } else {
        response.body
    };
    ok_result(format!("{body}{rate}"))
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
