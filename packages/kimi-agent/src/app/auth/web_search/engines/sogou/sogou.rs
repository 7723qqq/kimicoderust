use std::collections::HashSet;

use indexmap::IndexMap;
use url::Url;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::abort::{AbortError, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit};
use crate::app::auth::web_search::web_search::WebSearchResult;

use super::super::engine_http::{EngineRequestOptions, engine_fetch};
use super::super::types::{SearchEngineFuture, SearchEngineOptions};
use super::parser::{SogouSearchResult, is_sogou_challenge_page, parse_sogou_search_results};

const SOGOU_SEARCH_URL: &str = "https://www.sogou.com/web";
const SOGOU_PAGE_SIZE: usize = 10;
const REQUEST_TIMEOUT_MS: u64 = 20_000;
const MAX_REDIRECTS: usize = 5;
const COMMON_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36"),
    ("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
    ("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8"),
    ("Referer", "https://www.sogou.com/"),
];

fn common_headers() -> Vec<(String, String)> {
    COMMON_HEADERS
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn status_error(status: u16) -> FetchError {
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Sogou search request failed: HTTP {status}."),
        Error2Options {
            details: Some(serde_json::Map::from_iter([(
                "status".to_string(),
                serde_json::Value::from(status),
            )])),
            ..Default::default()
        },
    ))
}

fn is_allowed_sogou_redirect_url(url: &Url) -> bool {
    let hostname = url.host_str().unwrap_or_default().to_lowercase();
    (url.scheme() == "https" || url.scheme() == "http")
        && (hostname == "sogou.com" || hostname.ends_with(".sogou.com"))
}

fn merge_set_cookie(cookie_header: &str, set_cookie: Option<&str>) -> String {
    let Some(set_cookie) = set_cookie else {
        return cookie_header.to_string();
    };
    let mut cookie_map = IndexMap::new();
    for cookie in cookie_header.split(';') {
        let trimmed = cookie.trim();
        if trimmed.is_empty() {
            continue;
        }
        let name = trimmed.split('=').next().unwrap_or_default();
        cookie_map.insert(name.to_string(), trimmed.to_string());
    }
    if let Some(pair) = set_cookie.split(';').next().map(str::trim)
        && !pair.is_empty()
    {
        let name = pair.split('=').next().unwrap_or_default();
        cookie_map.insert(name.to_string(), pair.to_string());
    }
    cookie_map
        .into_values()
        .collect::<Vec<_>>()
        .join("; ")
}

async fn fetch_sogou_html(
    initial_url: &str,
    options: &SearchEngineOptions,
) -> Result<String, FetchError> {
    if let Some(fetch) = &options.fetch_impl {
        let response = match fetch
            .fetch(
                initial_url,
                FetchInit {
                    headers: common_headers(),
                    signal: options.signal.clone(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(response) => response,
            Err(error) if is_abort_error(error.as_ref()) => return Err(error),
            Err(error) => {
                let message = error.to_string();
                return Err(Box::new(Error2::with_options(
                    WEB_FETCH_FAILED,
                    format!("Sogou request to {initial_url} failed: {message}"),
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
        return response.text().await;
    }

    let mut current_url = initial_url.to_string();
    let mut cookie_header = String::new();
    for _ in 0..=MAX_REDIRECTS {
        if options.signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
            return Err(Box::new(AbortError::new("The operation was aborted.")));
        }
        let mut headers = common_headers();
        if !cookie_header.is_empty() {
            headers.push(("Cookie".to_string(), cookie_header.clone()));
        }
        let response = match engine_fetch(
            &current_url,
            EngineRequestOptions {
                headers,
                timeout_ms: Some(REQUEST_TIMEOUT_MS),
                manual_redirect: true,
                ..Default::default()
            },
        )
        .await
        {
            Ok(response) => response,
            Err(error) if is_abort_error(error.as_ref()) => return Err(error),
            Err(error) => {
                let message = error.to_string();
                return Err(Box::new(Error2::with_options(
                    WEB_FETCH_FAILED,
                    format!("Sogou request to {current_url} failed: {message}"),
                    Error2Options {
                        cause: Some(error),
                        ..Default::default()
                    },
                )));
            }
        };
        cookie_header = merge_set_cookie(&cookie_header, response.header("set-cookie"));
        if (300..400).contains(&response.status) {
            let location = response
                .header("location")
                .filter(|location| !location.is_empty())
                .map(str::to_string);
            let Some(location) = location else {
                return Err(Box::new(Error2::new(
                    WEB_FETCH_FAILED,
                    format!(
                        "Sogou returned redirect status {} without a Location header",
                        response.status
                    ),
                )));
            };
            let redirect_url = Url::parse(&current_url)?.join(&location)?;
            if !is_allowed_sogou_redirect_url(&redirect_url) {
                return Err(Box::new(Error2::new(
                    WEB_FETCH_FAILED,
                    format!(
                        "Sogou redirected to an unexpected host: {}",
                        redirect_url.host_str().unwrap_or_default()
                    ),
                )));
            }
            current_url = redirect_url.to_string();
            continue;
        }
        if !response.ok {
            return Err(status_error(response.status));
        }
        return response.text().await;
    }
    Err(Box::new(Error2::new(
        WEB_FETCH_FAILED,
        "Sogou returned too many redirects",
    )))
}

async fn search_sogou_page(
    query: &str,
    page: usize,
    options: &SearchEngineOptions,
) -> Result<Vec<SogouSearchResult>, FetchError> {
    let mut url = Url::parse(SOGOU_SEARCH_URL).expect("valid Sogou URL");
    url.query_pairs_mut()
        .append_pair("query", query)
        .append_pair("page", &page.to_string())
        .append_pair("ie", "utf8");
    let html = fetch_sogou_html(url.as_str(), options).await?;
    if is_sogou_challenge_page(&html) {
        return Err(Box::new(Error2::new(
            WEB_FETCH_FAILED,
            "Sogou returned a verification or anti-bot page",
        )));
    }
    Ok(parse_sogou_search_results(&html))
}

fn to_web_search_result(result: SogouSearchResult) -> WebSearchResult {
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

pub fn search_sogou(
    query: String,
    limit: usize,
    options: SearchEngineOptions,
) -> SearchEngineFuture {
    Box::pin(async move {
        let mut all_results = Vec::new();
        let mut seen_urls = HashSet::new();
        let max_page = 1.max(limit.div_ceil(SOGOU_PAGE_SIZE));
        for page in 1..=max_page {
            if all_results.len() >= limit {
                break;
            }
            let page_results = search_sogou_page(&query, page, &options).await?;
            let page_result_count = page_results.len();
            for result in page_results {
                if seen_urls.contains(&result.url) {
                    continue;
                }
                seen_urls.insert(result.url.clone());
                all_results.push(result);
            }
            if page_result_count == 0 {
                break;
            }
        }
        Ok(all_results
            .into_iter()
            .take(limit)
            .map(to_web_search_result)
            .collect())
    })
}
