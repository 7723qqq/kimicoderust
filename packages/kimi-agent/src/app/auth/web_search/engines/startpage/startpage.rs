use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use parking_lot::Mutex;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::abort::{AbortError, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};
use crate::app::auth::web_search::web_search::WebSearchResult;

use super::super::engine_http::{EngineRequestOptions, engine_fetch};
use super::super::types::{SearchEngineFuture, SearchEngineOptions};
use super::parser::{
    StartpageSearchResult, extract_interstitial_payload, extract_results_from_html,
    extract_sc_code, is_captcha_page,
};

const STARTPAGE_BASE_URL: &str = "https://www.startpage.com";
const STARTPAGE_SEARCH_URL: &str = "https://www.startpage.com/sp/search";
const STARTPAGE_SC_TTL_MS: u64 = 30 * 60 * 1000;
const DEFAULT_PAGE_SIZE: usize = 10;
const TOKEN_REQUEST_TIMEOUT_MS: u64 = 15_000;
const SEARCH_REQUEST_TIMEOUT_MS: u64 = 20_000;
const COMMON_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36"),
    ("Accept-Language", "en-US,en;q=0.9"),
];
const ACCEPT_HEADER: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

static SC_CACHE: Lazy<Mutex<Option<(String, u64)>>> = Lazy::new(|| Mutex::new(None));

struct StartpageRequestInit {
    method: Option<String>,
    headers: Vec<(String, String)>,
    body: Option<String>,
    timeout_ms: u64,
}

fn common_headers() -> Vec<(String, String)> {
    COMMON_HEADERS
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn status_error(status: u16) -> FetchError {
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Startpage request failed: HTTP {status}."),
        Error2Options {
            details: Some(serde_json::Map::from_iter([(
                "status".to_string(),
                serde_json::Value::from(status),
            )])),
            ..Default::default()
        },
    ))
}

async fn fetch_startpage_text(
    url: &str,
    options: &SearchEngineOptions,
    init: StartpageRequestInit,
) -> Result<String, FetchError> {
    let response_result: Result<FetchResponse, FetchError> = if let Some(fetch) = &options.fetch_impl {
        fetch
            .fetch(
                url,
                FetchInit {
                    method: init.method.clone(),
                    headers: init.headers.clone(),
                    body: init.body.clone(),
                    signal: options.signal.clone(),
                    ..Default::default()
                },
            )
            .await
    } else if options.signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
        Err(Box::new(AbortError::new("The operation was aborted.")))
    } else {
        engine_fetch(
            url,
            EngineRequestOptions {
                method: init.method,
                headers: init.headers,
                body: init.body,
                timeout_ms: Some(init.timeout_ms),
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
                format!("Startpage request to {url} failed: {message}"),
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

async fn get_sc_code(options: &SearchEngineOptions) -> Result<String, FetchError> {
    let now = now_ms();
    let cached = SC_CACHE.lock().clone();
    if let Some((code, cached_at)) = cached
        && now.saturating_sub(cached_at) < STARTPAGE_SC_TTL_MS
    {
        return Ok(code);
    }
    let mut headers = common_headers();
    headers.push(("Accept".to_string(), ACCEPT_HEADER.to_string()));
    let html = fetch_startpage_text(
        "https://www.startpage.com/",
        options,
        StartpageRequestInit {
            method: None,
            headers,
            body: None,
            timeout_ms: TOKEN_REQUEST_TIMEOUT_MS,
        },
    )
    .await?;
    if is_captcha_page(&html) {
        return Err(Box::new(Error2::new(
            WEB_FETCH_FAILED,
            "Startpage returned a verification or anti-bot page while requesting the search token",
        )));
    }
    let Some(sc_code) = extract_sc_code(&html) else {
        return Err(Box::new(Error2::new(
            WEB_FETCH_FAILED,
            "Failed to extract Startpage search token",
        )));
    };
    *SC_CACHE.lock() = Some((sc_code.clone(), now));
    Ok(sc_code)
}

fn search_form_data(query: &str, page: usize, sc_code: &str) -> String {
    let mut form_data = url::form_urlencoded::Serializer::new(String::new());
    form_data
        .append_pair("query", query)
        .append_pair("cat", "web")
        .append_pair("t", "device")
        .append_pair("sc", sc_code)
        .append_pair("abp", "1")
        .append_pair("abd", "1")
        .append_pair("abe", "1");
    if page > 1 {
        form_data
            .append_pair("page", &page.to_string())
            .append_pair("segment", "startpage.udog");
    }
    form_data.finish()
}

fn interstitial_form_data(payload: Vec<(String, String)>) -> String {
    let mut form_data = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in payload {
        form_data.append_pair(&key, &value);
    }
    form_data.finish()
}

async fn search_startpage_page(
    query: &str,
    page: usize,
    options: &SearchEngineOptions,
) -> Result<Vec<StartpageSearchResult>, FetchError> {
    let sc_code = get_sc_code(options).await?;
    let mut search_headers = common_headers();
    search_headers.extend([
        ("Accept".to_string(), ACCEPT_HEADER.to_string()),
        (
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        ),
        ("Origin".to_string(), STARTPAGE_BASE_URL.to_string()),
        (
            "Referer".to_string(),
            "https://www.startpage.com/".to_string(),
        ),
    ]);
    let mut html = fetch_startpage_text(
        STARTPAGE_SEARCH_URL,
        options,
        StartpageRequestInit {
            method: Some("POST".to_string()),
            headers: search_headers.clone(),
            body: Some(search_form_data(query, page, &sc_code)),
            timeout_ms: SEARCH_REQUEST_TIMEOUT_MS,
        },
    )
    .await?;
    if let Some(interstitial_payload) = extract_interstitial_payload(&html) {
        if let Some((_, referer)) = search_headers
            .iter_mut()
            .find(|(name, _)| name == "Referer")
        {
            *referer = STARTPAGE_SEARCH_URL.to_string();
        }
        html = fetch_startpage_text(
            STARTPAGE_SEARCH_URL,
            options,
            StartpageRequestInit {
                method: Some("POST".to_string()),
                headers: search_headers,
                body: Some(interstitial_form_data(interstitial_payload)),
                timeout_ms: SEARCH_REQUEST_TIMEOUT_MS,
            },
        )
        .await?;
    }
    extract_results_from_html(&html)
}

pub fn search_startpage(
    query: String,
    limit: usize,
    options: SearchEngineOptions,
) -> SearchEngineFuture {
    Box::pin(async move {
        let mut all_results = Vec::new();
        let mut seen_urls = HashSet::new();
        let max_page = 1.max(limit.div_ceil(DEFAULT_PAGE_SIZE));
        for page in 1..=max_page {
            if all_results.len() >= limit {
                break;
            }
            let page_results = search_startpage_page(&query, page, &options).await?;
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
            .map(|result| WebSearchResult {
                title: result.title,
                url: result.url,
                snippet: result.description,
                site_name: if result.source.is_empty() {
                    None
                } else {
                    Some(result.source)
                },
                date: None,
            })
            .collect())
    })
}
