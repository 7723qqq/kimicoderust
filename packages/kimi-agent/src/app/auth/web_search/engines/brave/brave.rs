use scraper::{ElementRef, Html, Selector};
use url::Url;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::abort::{AbortError, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};
use crate::app::auth::web_search::web_search::WebSearchResult;

use super::super::engine_http::{EngineRequestOptions, debug_log, engine_fetch};
use super::super::types::{SearchEngineFuture, SearchEngineOptions};

const BRAVE_BASE_URL: &str = "https://search.brave.com/search";
const REQUEST_TIMEOUT_MS: u64 = 30_000;
const FALLBACK_HEADERS: &[(&str, &str)] = &[
    (
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/112.0.0.0 Safari/537.36",
    ),
    ("Connection", "keep-alive"),
    (
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",
    ),
    ("Accept-Encoding", "gzip, deflate, br"),
    (
        "sec-ch-ua",
        "\"Chromium\";v=\"112\", \"Google Chrome\";v=\"112\", \"Not:A-Brand\";v=\"99\"",
    ),
    ("sec-ch-ua-mobile", "?0"),
    ("sec-ch-ua-platform", "\"Windows\""),
    ("upgrade-insecure-requests", "1"),
    ("sec-fetch-site", "same-origin"),
    ("sec-fetch-mode", "navigate"),
    ("sec-fetch-user", "?1"),
    ("sec-fetch-dest", "document"),
    ("referer", "https://duckduckgo.com/"),
    ("accept-language", "zh-CN,zh;q=0.9,en;q=0.8"),
];

#[derive(Debug)]
struct BraveSearchResult {
    title: String,
    url: String,
    description: String,
    source: String,
}

fn fallback_headers() -> Vec<(String, String)> {
    FALLBACK_HEADERS
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn direct_child_anchor(element: ElementRef<'_>) -> Option<ElementRef<'_>> {
    element
        .children()
        .filter_map(ElementRef::wrap)
        .find(|child| child.value().name().eq_ignore_ascii_case("a"))
}

fn parse_brave_results(html: &str) -> Vec<BraveSearchResult> {
    let document = Html::parse_document(html);
    let snippet_selector = Selector::parse("#results .snippet").unwrap();
    let content_selector = Selector::parse(".result-content").unwrap();
    let title_selector = Selector::parse(".search-snippet-title").unwrap();
    let description_selector = Selector::parse(".generic-snippet").unwrap();
    let source_selector = Selector::parse(".site-name-wrapper").unwrap();
    let mut results = Vec::new();
    for result_element in document.select(&snippet_selector) {
        let Some(content) = result_element.select(&content_selector).next() else {
            continue;
        };
        let Some(main_link) = direct_child_anchor(content) else {
            continue;
        };
        let url = main_link.value().attr("href");
        let title = main_link
            .select(&title_selector)
            .next()
            .map(|element| element.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let description = content
            .select(&description_selector)
            .next()
            .map(|element| element.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let source = main_link
            .select(&source_selector)
            .next()
            .map(|element| element.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        if !title.is_empty() && url.is_some() {
            results.push(BraveSearchResult {
                title,
                url: url.unwrap_or_default().to_string(),
                description,
                source,
            });
        }
    }
    results
}

fn status_error(status: u16) -> FetchError {
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Brave search request failed: HTTP {status}."),
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
    let response_result: Result<FetchResponse, FetchError> = if let Some(fetch) = &options.fetch_impl {
        fetch
            .fetch(
                url,
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
            url,
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
                format!("Brave search request to {url} failed: {message}"),
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

pub fn search_brave(
    query: String,
    limit: usize,
    options: SearchEngineOptions,
) -> SearchEngineFuture {
    Box::pin(async move {
        let mut all_results = Vec::new();
        let mut pn = 0usize;
        while all_results.len() < limit {
            let mut search_url = Url::parse(BRAVE_BASE_URL).expect("valid Brave URL");
            search_url
                .query_pairs_mut()
                .append_pair("q", &query)
                .append_pair("source", "web")
                .append_pair("offset", &pn.to_string());
            let html = fetch_search_page(search_url.as_str(), &options).await?;
            let results = parse_brave_results(&html);
            let has_results = !results.is_empty();
            all_results.extend(results);
            if !has_results {
                debug_log("No more Brave results, ending early.");
                break;
            }
            pn += 1;
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
