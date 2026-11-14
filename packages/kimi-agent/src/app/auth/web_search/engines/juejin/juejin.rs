use serde_json::Value;
use url::Url;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::abort::{AbortError, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};
use crate::app::auth::web_search::web_search::WebSearchResult;

use super::super::engine_http::{EngineRequestOptions, debug_log, engine_fetch};
use super::super::types::{SearchEngineFuture, SearchEngineOptions};

const JUEJIN_SEARCH_URL: &str = "https://api.juejin.cn/search_api/v1/search";
const REQUEST_TIMEOUT_MS: u64 = 30_000;
const FALLBACK_HEADERS: &[(&str, &str)] = &[
    ("pragma", "no-cache"),
    ("priority", "u=1, i"),
    ("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/112.0.0.0 Safari/537.36"),
    ("content-type", "application/json"),
    ("Accept", "*/*"),
    ("Host", "api.juejin.cn"),
    ("Connection", "keep-alive"),
];

fn fallback_headers() -> Vec<(String, String)> {
    FALLBACK_HEADERS
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn status_error(status: u16) -> FetchError {
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Juejin search request failed: HTTP {status}."),
        Error2Options {
            details: Some(serde_json::Map::from_iter([(
                "status".to_string(),
                serde_json::Value::from(status),
            )])),
            ..Default::default()
        },
    ))
}

fn invalid_json_error(error: serde_json::Error) -> FetchError {
    let message = error.to_string();
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Juejin search request returned invalid JSON: {message}"),
        Error2Options {
            cause: Some(Box::new(error)),
            ..Default::default()
        },
    ))
}

async fn fetch_search_page(
    query: &str,
    cursor: &str,
    limit: usize,
    options: &SearchEngineOptions,
) -> Result<String, FetchError> {
    let mut search_url = Url::parse(JUEJIN_SEARCH_URL).expect("valid Juejin URL");
    search_url
        .query_pairs_mut()
        .append_pair("aid", "2608")
        .append_pair("uuid", "7259393293459605051")
        .append_pair("spider", "0")
        .append_pair("query", query)
        .append_pair("id_type", "0")
        .append_pair("cursor", cursor)
        .append_pair("limit", &limit.to_string())
        .append_pair("search_type", "0")
        .append_pair("sort_type", "0")
        .append_pair("version", "1");
    let url = search_url.to_string();
    let response_result: Result<FetchResponse, FetchError> = if let Some(fetch) = &options.fetch_impl {
        fetch
            .fetch(
                &url,
                FetchInit {
                    headers: fallback_headers(),
                    signal: options.signal.clone(),
                    ..Default::default()
                },
            )
            .await
    } else if options.signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
        Err(Box::new(AbortError::new("The operation was aborted.")))
    } else {
        engine_fetch(
            &url,
            EngineRequestOptions {
                headers: fallback_headers(),
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
                format!("Juejin search request to {url} failed: {message}"),
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
    response.text().await
}

fn parse_juejin_results(data: &[Value]) -> Vec<WebSearchResult> {
    let mut results = Vec::new();
    for item in data {
        let Some(result_model) = item.get("result_model") else {
            continue;
        };
        let Some(article_id) = result_model.get("article_id").and_then(Value::as_str) else {
            continue;
        };
        let clean_title = item
            .get("title_highlight")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .replace("<em>", "")
            .replace("</em>", "");
        let clean_content = item
            .get("content_highlight")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .replace("<em>", "")
            .replace("</em>", "");
        let tag_names = result_model
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .map(|tag| {
                        tag.get("tag_name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                    })
                    .filter(|tag| !tag.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let category_name = result_model
            .get("category")
            .and_then(|category| category.get("category_name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let article_info = result_model.get("article_info");
        let digg_count = article_info
            .and_then(|info| info.get("digg_count"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let view_count = article_info
            .and_then(|info| info.get("view_count"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let description = format!(
            "{clean_content} | 分类: {category_name} | 标签: {tag_names} | 👍 {digg_count} | 👀 {view_count}"
        );
        results.push(WebSearchResult {
            title: if clean_title.is_empty() {
                "No title".to_string()
            } else {
                clean_title
            },
            url: format!("https://juejin.cn/post/{article_id}"),
            snippet: description,
            site_name: result_model
                .get("author_user_info")
                .and_then(|author| author.get("user_name"))
                .and_then(Value::as_str)
                .map(str::to_string),
            date: None,
        });
    }
    results
}

pub fn search_juejin(
    query: String,
    limit: usize,
    options: SearchEngineOptions,
) -> SearchEngineFuture {
    Box::pin(async move {
        let mut all_results = Vec::new();
        let mut cursor = "0".to_string();
        while all_results.len() < limit {
            let page_limit = 20.min(limit - all_results.len());
            let body = fetch_search_page(&query, &cursor, page_limit, &options).await?;
            let parsed: Value = match serde_json::from_str(&body) {
                Ok(parsed) => parsed,
                Err(error) => return Err(invalid_json_error(error)),
            };
            if parsed.get("err_no").and_then(Value::as_i64) != Some(0) {
                debug_log(&format!(
                    "Juejin API error: {}",
                    parsed
                        .get("err_msg")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ));
                break;
            }
            let Some(data) = parsed.get("data").and_then(Value::as_array) else {
                break;
            };
            let results = parse_juejin_results(data);
            let result_count = results.len();
            all_results.extend(results);
            let has_more = parsed
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let next_cursor = parsed.get("cursor").and_then(Value::as_str);
            if !has_more
                || next_cursor.is_none_or(str::is_empty)
                || result_count == 0
            {
                break;
            }
            cursor = next_cursor.unwrap_or_default().to_string();
        }
        all_results.truncate(limit);
        Ok(all_results)
    })
}
