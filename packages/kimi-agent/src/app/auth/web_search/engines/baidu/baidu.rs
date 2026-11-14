use url::Url;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::abort::{AbortError, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit};
use crate::app::auth::web_search::web_search::WebSearchResult;

use super::super::engine_http::{EngineRequestOptions, engine_fetch};
use super::super::types::{SearchEngineFuture, SearchEngineOptions};
use super::parser::{BaiduSearchResult, parse_baidu_search_results};

const BAIDU_BASE_URL: &str = "https://www.baidu.com/s";
const REQUEST_TIMEOUT_MS: u64 = 30_000;
const FALLBACK_HEADERS: &[(&str, &str)] = &[(
    "User-Agent",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36",
)];

fn fallback_headers() -> Vec<(String, String)> {
    FALLBACK_HEADERS
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn build_baidu_search_url(query: &str, page_number: usize) -> String {
    let mut url = Url::parse(BAIDU_BASE_URL).expect("valid Baidu URL");
    url.query_pairs_mut()
        .append_pair("wd", query)
        .append_pair("pn", &(page_number * 10).to_string())
        .append_pair("ie", "utf-8")
        .append_pair("mod", "1")
        .append_pair("isbd", "1")
        .append_pair("isid", "f7ba1776007bcf9e")
        .append_pair("oq", query)
        .append_pair("tn", "88093251_62_hao_pg")
        .append_pair("usm", "1")
        .append_pair("fenlei", "256")
        .append_pair("rsv_idx", "1")
        .append_pair("rsv_pq", "f7ba1776007bcf9e")
        .append_pair(
            "rsv_t",
            "8179fxGiNMUh/0dXHrLsJXPlKYbkj9S5QH6rOLHY6pG6OGQ81YqzRTIGjjeMwEfiYQTSiTQIhCJj",
        )
        .append_pair("bs", query)
        .append_pair("_ss", "1")
        .append_pair("f4s", "1")
        .append_pair("csor", "5")
        .append_pair("_cr1", "30385");
    url.to_string()
}

fn status_error(status: u16) -> FetchError {
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Baidu search request failed: HTTP {status}."),
        Error2Options {
            details: Some(serde_json::Map::from_iter([(
                "status".to_string(),
                serde_json::Value::from(status),
            )])),
            ..Default::default()
        },
    ))
}

async fn fetch_search_page(
    url: &str,
    options: &SearchEngineOptions,
) -> Result<String, FetchError> {
    let response = if let Some(fetch) = &options.fetch_impl {
        fetch
            .fetch(
                url,
                FetchInit {
                    headers: fallback_headers(),
                    signal: options.signal.clone(),
                    ..Default::default()
                },
            )
            .await?
    } else {
        if options.signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
            return Err(Box::new(AbortError::new("The operation was aborted.")));
        }
        engine_fetch(
            url,
            EngineRequestOptions {
                headers: fallback_headers(),
                timeout_ms: Some(REQUEST_TIMEOUT_MS),
                ..Default::default()
            },
        )
        .await?
    };
    if !response.ok {
        return Err(status_error(response.status));
    }
    response.text().await
}

fn to_web_search_result(result: BaiduSearchResult) -> WebSearchResult {
    WebSearchResult {
        title: result.title,
        url: result.url,
        snippet: result.description,
        site_name: if result.source.is_empty() {
            None
        } else {
            Some(result.source)
        },
        date: None,
    }
}

pub fn search_baidu(
    query: String,
    limit: usize,
    options: SearchEngineOptions,
) -> SearchEngineFuture {
    Box::pin(async move {
        let mut all_results = Vec::new();
        let mut page_number = 0usize;
        while all_results.len() < limit {
            let html = match fetch_search_page(
                &build_baidu_search_url(&query, page_number),
                &options,
            )
            .await
            {
                Ok(html) => html,
                Err(error) if error.is::<Error2>() || is_abort_error(error.as_ref()) => {
                    return Err(error);
                }
                Err(error) => {
                    let message = error.to_string();
                    return Err(Box::new(Error2::with_options(
                        WEB_FETCH_FAILED,
                        format!("Baidu search request failed: {message}"),
                        Error2Options {
                            cause: Some(error),
                            ..Default::default()
                        },
                    )));
                }
            };
            let results = parse_baidu_search_results(&html);
            let has_results = !results.is_empty();
            all_results.extend(results);
            if !has_results {
                break;
            }
            page_number += 1;
        }
        Ok(all_results
            .into_iter()
            .take(limit)
            .map(to_web_search_result)
            .collect())
    })
}
