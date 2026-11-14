use scraper::{Html, Selector};
use url::Url;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::fetch_like::{FetchError, FetchInit};
use crate::app::auth::web_search::web_search::WebSearchResult;

use super::super::engine_http::{EngineRequestOptions, debug_log, engine_fetch};
use super::super::types::{SearchEngineFuture, SearchEngineOptions};
use super::parser::{BingSearchResult, parse_bing_search_results};

const BING_BASE_URL: &str = "https://cn.bing.com/search";
const REQUEST_TIMEOUT_MS: u64 = 30_000;
const FALLBACK_HEADERS: &[(&str, &str)] = &[
    (
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36",
    ),
    (
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
    ),
    ("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8"),
    ("Cache-Control", "no-cache"),
    ("Pragma", "no-cache"),
    ("Upgrade-Insecure-Requests", "1"),
];
const BOT_DETECTION_KEYWORDS: &[&str] = &[
    "captcha",
    "verification",
    "verify you are human",
    "access denied",
    "blocked",
    "rate limit",
    "too many requests",
    "请验证",
    "验证码",
    "人机验证",
];

#[derive(Debug)]
struct BlockedPageAnalysis {
    blocked: bool,
    has_results: bool,
    detected_keywords: Vec<&'static str>,
    title: String,
}

fn fallback_headers() -> Vec<(String, String)> {
    FALLBACK_HEADERS
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn build_bing_search_url(query: &str, page_number: usize) -> String {
    let mut url = Url::parse(BING_BASE_URL).expect("valid Bing URL");
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("setlang", "zh-CN")
        .append_pair("ensearch", "0")
        .append_pair("first", &(1 + page_number * 10).to_string());
    url.to_string()
}

fn analyze_blocked_page(html: &str) -> BlockedPageAnalysis {
    let normalized = html.to_lowercase();
    let document = Html::parse_document(html);
    let title_selector = Selector::parse("title").unwrap();
    let title = document
        .select(&title_selector)
        .next()
        .map(|element| element.text().collect::<String>().trim().to_lowercase())
        .unwrap_or_default();
    let detected_keywords = BOT_DETECTION_KEYWORDS
        .iter()
        .copied()
        .filter(|keyword| normalized.contains(keyword))
        .collect::<Vec<_>>();
    let result_selector =
        Selector::parse("#b_results .b_algo, #b_results li.b_algo, .b_algo, .b_ans").unwrap();
    let has_structured_results = document.select(&result_selector).next().is_some();
    let has_parsed_results = !parse_bing_search_results(html, 1).is_empty();
    let has_results = has_structured_results || has_parsed_results;
    let captcha_selector = Selector::parse(
        "iframe[src*=\"captcha\"],[id*=\"captcha\"],[class*=\"captcha\"],form[action*=\"validate\"],input[name*=\"captcha\"],#b_captcha,.b_captcha",
    )
    .unwrap();
    let has_captcha_ui = document.select(&captcha_selector).next().is_some();
    let has_strong_title_signal = [
        "captcha",
        "verify you are human",
        "access denied",
        "too many requests",
        "验证码",
        "人机验证",
        "请验证",
    ]
    .iter()
    .any(|keyword| title.contains(keyword));
    let blocked =
        !has_results && (has_captcha_ui || has_strong_title_signal || detected_keywords.len() >= 2);
    BlockedPageAnalysis {
        blocked,
        has_results,
        detected_keywords,
        title,
    }
}

fn status_error(status: u16) -> FetchError {
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Bing search request failed: HTTP {status}."),
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

async fn search_bing_with_http(
    query: &str,
    limit: usize,
    options: &SearchEngineOptions,
) -> Result<Vec<BingSearchResult>, FetchError> {
    let mut all_results = Vec::new();
    let mut page_number = 0usize;
    while all_results.len() < limit {
        let html = match fetch_search_page(&build_bing_search_url(query, page_number), options).await {
            Ok(html) => html,
            Err(error) if error.is::<Error2>() => return Err(error),
            Err(error) => {
                return Err(Box::new(Error2::new(
                    WEB_FETCH_FAILED,
                    format!("Bing search request failed: {error}"),
                )));
            }
        };
        let page_state = analyze_blocked_page(&html);
        if page_state.blocked {
            return Err(Box::new(Error2::new(
                WEB_FETCH_FAILED,
                format!(
                    "Bing returned a verification or anti-bot page (title: {}, keywords: {})",
                    if page_state.title.is_empty() {
                        "unknown"
                    } else {
                        &page_state.title
                    },
                    if page_state.detected_keywords.is_empty() {
                        "none".to_string()
                    } else {
                        page_state.detected_keywords.join(", ")
                    }
                ),
            )));
        }
        if page_state.has_results && !page_state.detected_keywords.is_empty() {
            debug_log(&format!(
                "Bing page contains suspicious keywords but also has results, skipping block detection: {}",
                page_state.detected_keywords.join(", ")
            ));
        }
        let results = parse_bing_search_results(&html, limit - all_results.len());
        let has_results = !results.is_empty();
        all_results.extend(results);
        if !has_results {
            debug_log("No more Bing results from HTTP mode, ending early.");
            break;
        }
        page_number += 1;
    }
    all_results.truncate(limit);
    Ok(all_results)
}

fn to_web_search_result(result: BingSearchResult) -> WebSearchResult {
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

pub fn search_bing(
    query: String,
    limit: usize,
    options: SearchEngineOptions,
) -> SearchEngineFuture {
    Box::pin(async move {
        let results = search_bing_with_http(&query, limit, &options).await?;
        Ok(results.into_iter().map(to_web_search_result).collect())
    })
}
