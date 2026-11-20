use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use indexmap::IndexMap;
use serde_json::Value;

use crate::subagent::types::ParentCancel;

pub const GITHUB_DEFAULT_BASE_URL: &str = "https://api.github.com";
pub const GITHUB_API_VERSION: &str = "2022-11-28";
pub const GITHUB_USER_AGENT: &str = "kimi-code";
pub const GITHUB_DEFAULT_ACCEPT: &str = "application/vnd.github+json";
pub const GITHUB_REQUEST_TIMEOUT_MS: u64 = 30_000;
pub const GITHUB_MAX_RESPONSE_BODY_BYTES: usize = 5 * 1024 * 1024;
pub const GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES: usize = 200_000;
pub const GITHUB_NO_TOKEN_ERROR: &str = "No GitHub token configured. Set token in the [github] section of config.toml, or set the GITHUB_TOKEN (or GH_TOKEN) environment variable.";

#[derive(Clone)]
struct CachedResponse {
    etag: String,
    body: String,
    next_page: bool,
}

static ETAG_CACHE: LazyLock<Mutex<IndexMap<String, CachedResponse>>> = LazyLock::new(|| Mutex::new(IndexMap::new()));

fn cache_key_for(token: &str, url: &str, accept: &str) -> String {
    let mut hash = 2_166_136_261_u32;
    let units = token.encode_utf16().collect::<Vec<_>>();
    for index in 0..units.len() {
        let unit = units[index];
        let point = if (0xd800..=0xdbff).contains(&unit) && units.get(index + 1).is_some_and(|next| (0xdc00..=0xdfff).contains(next)) {
            0x10000 + ((u32::from(unit) - 0xd800) << 10) + (u32::from(units[index + 1]) - 0xdc00)
        } else {
            u32::from(unit)
        };
        hash = (hash ^ point).wrapping_mul(16_777_619);
    }
    let mut value = (hash as i32).unsigned_abs();
    let mut digits = Vec::new();
    loop {
        digits.push(char::from_digit(value % 36, 36).unwrap());
        value /= 36;
        if value == 0 { break; }
    }
    let token_hash = digits.into_iter().rev().collect::<String>();
    format!("{token_hash}\0{accept}\0{url}")
}

fn remember_etag(key: String, cached: CachedResponse) {
    let mut cache = ETAG_CACHE.lock().unwrap();
    cache.shift_remove(&key);
    cache.insert(key, cached);
    while cache.len() > 64 {
        cache.shift_remove_index(0);
    }
}

fn retry_delay_ms(headers: &reqwest::header::HeaderMap, now: i64) -> Option<u64> {
    if let Some(raw) = headers.get("retry-after").and_then(|value| value.to_str().ok()) {
        if let Some(seconds) = parse_js_number(raw).filter(|value| value.is_finite() && *value >= 0.0) {
            let delay = (seconds * 1000.0 + 0.5).floor();
            return (delay <= 30_000.0).then_some(delay as u64);
        }
        if let Ok(at) = chrono::DateTime::parse_from_rfc2822(raw) {
            let delay = at.timestamp_millis().saturating_sub(now).max(0);
            return (delay <= 30_000).then_some(delay as u64);
        }
    }
    Some(1000)
}

#[derive(Clone, Debug, Default)]
pub struct GitHubRequestOptions {
    pub query: Option<Vec<(String, Value)>>,
    pub body: Option<Vec<(String, Value)>>,
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
    let cache_key = (method.as_str() == "GET").then(|| cache_key_for(token, &url, options.accept.as_deref().unwrap_or(GITHUB_DEFAULT_ACCEPT)));
    let cached = cache_key.as_ref().and_then(|key| ETAG_CACHE.lock().unwrap().get(key).cloned());
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
            .body(js_json_stringify_pairs(&body));
    }
    if let Some(cached) = &cached {
        request = request.header("If-None-Match", &cached.etag);
    }
    let operation = async move {
        let retry_request = request.try_clone();
        let mut response = match request.send().await {
            Ok(response) => response,
            Err(error) => return network_error(error),
        };
        let retry = matches!(response.status().as_u16(), 429 | 502 | 503 | 504)
            || (response.status().as_u16() == 403 && response.headers().contains_key("retry-after"));
        if retry {
            if let (Some(delay), Some(request)) = (retry_delay_ms(response.headers(), chrono::Utc::now().timestamp_millis()), retry_request) {
                drop(response);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                response = match request.send().await {
                    Ok(response) => response,
                    Err(error) => return network_error(error),
                };
            }
        }
        let status = response.status().as_u16();
        let rate_remaining = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_rate_limit_remaining);
        if status == 304 {
            if let (Some(key), Some(cached)) = (&cache_key, &cached) {
                remember_etag(key.clone(), cached.clone());
                return GitHubRequestResult { status:200, ok:true, body:cached.body.clone(), error:None, rate_remaining, next_page:cached.next_page, truncated_bytes:0 };
            }
        }
        let etag = response.headers().get("etag").and_then(|value| value.to_str().ok()).map(str::to_string);
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
        if let (Some(key), Some(etag)) = (cache_key, etag) {
            if body.encode_utf16().count() <= 200_000 {
                remember_etag(key, CachedResponse { etag, body:body.clone(), next_page });
            }
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
    fn etag_identity_matches_v2_utf16_code_point_iteration() {
        for (token, expected) in [
            ("YOUR_API_KEY", "u92egl"),
            ("token-a", "lbbjyc"),
            ("😀", "4cn9hp"),
            ("x😀y", "e40rvy"),
        ] {
            assert_eq!(cache_key_for(token, "url", "accept"), format!("{expected}\0accept\0url"));
        }
    }

    #[test]
    fn retry_after_obeys_v2_rounding_and_thirty_second_cap() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-19T00:00:00Z").unwrap().timestamp_millis();
        for (raw, expected) in [
            ("0", Some(0)),
            ("0.0005", Some(1)),
            ("30", Some(30_000)),
            ("30.0005", None),
            ("not-a-date", Some(1000)),
            ("Thu, 19 Nov 2026 00:00:25 GMT", Some(25_000)),
            ("Thu, 19 Nov 2026 00:01:00 GMT", None),
            ("Wed, 18 Nov 2026 23:59:00 GMT", Some(0)),
        ] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("retry-after", raw.parse().unwrap());
            assert_eq!(retry_delay_ms(&headers, now), expected, "{raw}");
        }
    }

    #[tokio::test]
    async fn retries_once_then_revalidates_the_cached_get_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let replies = [
                "HTTP/1.1 503 Test\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nLink: <https://example.test/next>; rel=\"next\"\r\nContent-Length: 6\r\nConnection: close\r\n\r\ncached",
                "HTTP/1.1 304 Not Modified\r\nX-RateLimit-Remaining: 98\r\nConnection: close\r\n\r\n",
            ];
            for (index, reply) in replies.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                }
                assert_eq!(String::from_utf8_lossy(&request).to_ascii_lowercase().contains("if-none-match: \"v1\""), index == 2);
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let options = GitHubRequestOptions { token: Some("YOUR_API_KEY".into()), base_url: Some(base_url), ..Default::default() };
        let deps = GitHubRequestDeps { client:Some(reqwest::Client::builder().no_proxy().build().unwrap()), signal:None };
        let first = tokio::time::timeout(Duration::from_secs(3), github_request("GET", "/user", options.clone(), deps.clone())).await.unwrap().unwrap();
        assert_eq!((first.status, first.body.as_str(), first.next_page), (200, "cached", true));
        let second = tokio::time::timeout(Duration::from_secs(3), github_request("GET", "/user", options, deps)).await.unwrap().unwrap();
        assert_eq!((second.status, second.body.as_str(), second.next_page, second.rate_remaining), (200, "cached", true, Some(98)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn etag_cache_is_partitioned_by_token_and_accept_header() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (index, response) in [
                "HTTP/1.1 200 OK\r\nETag: \"one\"\r\nContent-Length: 3\r\nConnection: close\r\n\r\none",
                "HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nETag: \"two\"\r\nContent-Length: 3\r\nConnection: close\r\n\r\ntwo",
                "HTTP/1.1 200 OK\r\nETag: \"three\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\nthree",
            ].into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                }
                let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                assert_eq!(request.contains("if-none-match:"), index == 1);
                if index == 1 { assert!(request.contains("if-none-match: \"one\"")); }
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let call = |token:&str, accept:&str| GitHubRequestOptions { token:Some(token.into()),base_url:Some(base_url.clone()),accept:Some(accept.into()),..Default::default() };
        for (token, accept, body) in [("token-a","application/vnd.github+json","one"),("token-a","application/vnd.github+json","one"),("token-b","application/vnd.github+json","two"),("token-a","application/vnd.github.diff","three")] {
            let result = github_request("GET","/cache",call(token,accept),GitHubRequestDeps{client:Some(client.clone()),signal:None}).await.unwrap();
            assert_eq!(result.body,body);
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn etag_cache_uses_the_decoded_body_utf16_length_limit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (units, cache_expected) in [(200_001, false), (200_000, true)] {
            let body = "é".repeat(units);
            let served_body = body.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let body = served_body;
                for index in 0..2 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let mut chunk = [0; 1024];
                        let count = socket.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        request.extend_from_slice(&chunk[..count]);
                    }
                    let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    assert_eq!(request.contains("if-none-match:"), cache_expected && index == 1);
                    if index == 0 {
                        socket.write_all(format!("HTTP/1.1 200 OK\r\nETag: \"limit\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                    } else if cache_expected {
                        socket.write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n").await.unwrap();
                    } else {
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew").await.unwrap();
                    }
                }
            });
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let options = GitHubRequestOptions { token:Some("YOUR_API_KEY".into()),base_url:Some(base_url),..Default::default() };
            let first = github_request("GET","/bounded",options.clone(),GitHubRequestDeps{client:Some(client.clone()),signal:None}).await.unwrap();
            assert!(first.ok);
            let second = github_request("GET","/bounded",options,GitHubRequestDeps{client:Some(client),signal:None}).await.unwrap();
            if cache_expected { assert_eq!(second.body, body); } else { assert_eq!(second.body, "new"); }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn overflow_closes_a_stream_without_waiting_for_the_remainder() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\nabcdef\r\n").await.unwrap();
            let mut byte = [0];
            match tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte)).await.unwrap() {
                Ok(0) | Err(_) => {},
                other => panic!("overflow must close the reader: {other:?}"),
            }
        });
        let result = tokio::time::timeout(Duration::from_secs(2), github_request("GET", "/stream", GitHubRequestOptions { token:Some("YOUR_API_KEY".into()), base_url:Some(base_url), max_bytes:Some(3), ..Default::default() }, GitHubRequestDeps { client:Some(reqwest::Client::builder().no_proxy().build().unwrap()), signal:None })).await.unwrap().unwrap();
        assert_eq!(result.body, "abc\n[...truncated 3+ bytes]");
        server.await.unwrap();
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
    fn pagination_uses_the_v2_literal_next_relation() {
        assert!(!has_next_page("<https://example.test/page/2>; rel='next'"));
        assert!(!has_next_page("<https://example.test/page/2>; rel=next"));
        assert!(has_next_page("<https://example.test/page/2>; rel=\"next\"; type=\"application/json\""));
    }

    #[tokio::test]
    async fn a_preaborted_request_does_not_connect() {
        let cancel = ParentCancel::new();
        cancel.trigger();
        let result = github_request("GET", "/user", GitHubRequestOptions {
            token: Some("YOUR_API_KEY".into()),
            base_url: Some("http://127.0.0.1:1".into()),
            ..Default::default()
        }, GitHubRequestDeps { client: Some(reqwest::Client::builder().no_proxy().build().unwrap()), signal: Some(cancel) }).await;
        assert_eq!(result, Err(GitHubRequestAborted));
    }

    #[test]
    fn requested_body_limit_never_exceeds_the_global_limit() {
        assert_eq!(GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES, 200_000);
        assert_eq!(
            response_body_limit(Some(GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES)),
            200_000
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
    link.contains("rel=\"next\"")
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

pub(super) fn js_json_stringify_pairs(values: &[(String, Value)]) -> String {
    let fields = values.iter().map(|(key, value)| {
        let value = if key == "tree" {
            if let Some(entries) = value.as_array() {
                let entries = entries.iter().map(|entry| {
                    let fields = ["path", "mode", "type", "content", "sha"].into_iter()
                        .filter_map(|name| entry.get(name).map(|value| (name.to_string(), value.clone())))
                        .collect::<Vec<_>>();
                    js_json_stringify_pairs(&fields)
                }).collect::<Vec<_>>();
                format!("[{}]", entries.join(","))
            } else {
                js_json_stringify(value)
            }
        } else {
            js_json_stringify(value)
        };
        format!("{}:{value}", serde_json::to_string(key).unwrap())
    }).collect::<Vec<_>>();
    format!("{{{}}}", fields.join(","))
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

pub(super) fn js_json_stringify(value: &Value) -> String {
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
