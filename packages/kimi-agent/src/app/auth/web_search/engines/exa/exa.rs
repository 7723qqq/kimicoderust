use serde::Serialize;
use serde_json::Value;
use url::Url;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::abort::{AbortError, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};
use crate::app::auth::web_search::web_search::WebSearchResult;

use super::super::engine_http::{EngineRequestOptions, engine_fetch};
use super::super::types::{SearchEngineFuture, SearchEngineOptions};

const EXA_SEARCH_URL: &str = "https://exa.ai/search/api/search-fast";
const REQUEST_TIMEOUT_MS: u64 = 30_000;
const FALLBACK_HEADERS: &[(&str, &str)] = &[
    (
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/112.0.0.0 Safari/537.36",
    ),
    ("Connection", "keep-alive"),
    ("Accept", "*/*"),
    ("Accept-Encoding", "gzip, deflate, br"),
    (
        "sec-ch-ua",
        "\"Chromium\";v=\"112\", \"Google Chrome\";v=\"112\", \"Not:A-Brand\";v=\"99\"",
    ),
    ("content-type", "text/plain;charset=UTF-8"),
    ("sec-ch-ua-mobile", "?0"),
    ("sec-ch-ua-platform", "\"Windows\""),
    ("origin", "https://exa.ai"),
    ("sec-fetch-site", "same-origin"),
    ("sec-fetch-mode", "cors"),
    ("sec-fetch-dest", "empty"),
    ("accept-language", "zh-CN,zh;q=0.9,en;q=0.8"),
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExaRequest<'a> {
    num_results: usize,
    query: &'a str,
    #[serde(rename = "type")]
    search_type: &'static str,
    use_autoprompt: bool,
    domain_filter_type: &'static str,
    text: bool,
    density: &'static str,
    resolved_search_type: &'static str,
    moderation: bool,
    fast_mode: bool,
    reranker_type: &'static str,
}

fn fallback_headers() -> Vec<(String, String)> {
    FALLBACK_HEADERS
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn status_error(status: u16) -> FetchError {
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Exa request failed: HTTP {status}."),
        Error2Options {
            details: Some(serde_json::Map::from_iter([(
                "status".to_string(),
                serde_json::Value::from(status),
            )])),
            ..Default::default()
        },
    ))
}

fn invalid_json_error(error: FetchError) -> FetchError {
    let message = error.to_string();
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Exa request returned invalid JSON: {message}"),
        Error2Options {
            cause: Some(error),
            ..Default::default()
        },
    ))
}

fn locale_date(raw: &str) -> String {
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(raw) {
        let local = date.with_timezone(&chrono::Local);
        return format!(
            "{}/{}/{}",
            local.format("%-m"),
            local.format("%-d"),
            local.format("%Y")
        );
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return format!(
            "{}/{}/{}",
            date.format("%-m"),
            date.format("%-d"),
            date.format("%Y")
        );
    }
    "Invalid Date".to_string()
}

pub fn search_exa(
    query: String,
    limit: usize,
    options: SearchEngineOptions,
) -> SearchEngineFuture {
    Box::pin(async move {
        let body = serde_json::to_string(&ExaRequest {
            num_results: limit,
            query: &query,
            search_type: "auto",
            use_autoprompt: true,
            domain_filter_type: "include",
            text: true,
            density: "compact",
            resolved_search_type: "neural",
            moderation: true,
            fast_mode: false,
            reranker_type: "default",
        })
        .expect("Exa request serialization cannot fail");
        let response_result: Result<FetchResponse, FetchError> = if let Some(fetch) = &options.fetch_impl {
            fetch
                .fetch(
                    EXA_SEARCH_URL,
                    FetchInit {
                        method: Some("POST".to_string()),
                        headers: fallback_headers(),
                        body: Some(body.clone()),
                        signal: options.signal.clone(),
                        ..Default::default()
                    },
                )
                .await
        } else if options.signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
            Err(Box::new(AbortError::new("The operation was aborted.")))
        } else {
            engine_fetch(
                EXA_SEARCH_URL,
                EngineRequestOptions {
                    method: Some("POST".to_string()),
                    headers: fallback_headers(),
                    body: Some(body),
                    timeout_ms: Some(REQUEST_TIMEOUT_MS),
                    ..Default::default()
                },
            )
            .await
        };
        let response = match response_result {
            Ok(response) => response,
            Err(error) if is_abort_error(error.as_ref()) => return Err(error),
            Err(error) => {
                let message = error.to_string();
                return Err(Box::new(Error2::with_options(
                    WEB_FETCH_FAILED,
                    format!("Exa request to {EXA_SEARCH_URL} failed: {message}"),
                    Error2Options {
                        cause: Some(error),
                        ..Default::default()
                    },
                )));
            }
        };
        if !response.ok {
            return Err(status_error(response.status));
        }
        let api_response: Value = match response.text().await {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(error) => return Err(invalid_json_error(Box::new(error))),
            },
            Err(error) => return Err(invalid_json_error(error)),
        };
        let Some(api_results) = api_response.get("results").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        if api_results.is_empty() {
            return Ok(Vec::new());
        }
        Ok(api_results
            .iter()
            .take(limit)
            .filter_map(|item| {
                let url = item.get("url")?.as_str()?;
                let hostname = Url::parse(url)
                    .ok()?
                    .host_str()
                    .unwrap_or_default()
                    .to_string();
                let author = item
                    .get("author")
                    .and_then(Value::as_str)
                    .unwrap_or("N/A");
                let published = item
                    .get("publishedDate")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(locale_date)
                    .unwrap_or_else(|| "N/A".to_string());
                Some(WebSearchResult {
                    title: item
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("No title")
                        .to_string(),
                    url: url.to_string(),
                    snippet: format!("Author: {author}. Published: {published}"),
                    site_name: Some(hostname),
                    date: None,
                })
            })
            .collect())
    })
}
