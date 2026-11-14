use url::Url;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::abort::{AbortError, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};
use crate::app::auth::web_search::web_search::WebSearchResult;

use super::super::engine_http::{EngineRequestOptions, engine_fetch};
use super::super::types::{SearchEngineFuture, SearchEngineOptions};
use super::parser::{
    DuckDuckGoSearchResult, extract_duck_duck_go_preload_url, parse_duck_duck_go_jsonp,
    parse_duck_duck_go_results,
};

const REQUEST_TIMEOUT_MS: u64 = 30_000;
const PRELOAD_PAGE_HEADERS: &[(&str, &str)] = &[
    (
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
    ),
    (
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
    ),
    ("referer", "https://duckduckgo.com/"),
    ("accept-language", "zh-CN,zh;q=0.9,en;q=0.8"),
];
const PRELOAD_DATA_HEADERS: &[(&str, &str)] = &[
    (
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
    ),
    ("Accept", "*/*"),
    ("referer", "https://duckduckgo.com/"),
];
const HTML_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "application/x-www-form-urlencoded"),
    ("User-Agent", "Apifox/1.0.0 (https://apifox.com)"),
    ("Accept", "*/*"),
    ("Host", "html.duckduckgo.com"),
    ("Connection", "keep-alive"),
];
const HTML_RESULT_URL: &str = "https://html.duckduckgo.com/html/";

fn owned_headers(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn status_error(status: u16) -> FetchError {
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("DuckDuckGo request failed: HTTP {status}."),
        Error2Options {
            details: Some(serde_json::Map::from_iter([(
                "status".to_string(),
                serde_json::Value::from(status),
            )])),
            ..Default::default()
        },
    ))
}

async fn fetch_duck_duck_go_text(
    url: &str,
    options: &SearchEngineOptions,
    headers: &[(&str, &str)],
    body: Option<String>,
) -> Result<String, FetchError> {
    let method = if body.is_some() { "POST" } else { "GET" };
    let response_result: Result<FetchResponse, FetchError> = if let Some(fetch) = &options.fetch_impl {
        fetch
            .fetch(
                url,
                FetchInit {
                    method: Some(method.to_string()),
                    headers: owned_headers(headers),
                    body: body.clone(),
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
                method: Some(method.to_string()),
                headers: owned_headers(headers),
                body,
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
                format!("DuckDuckGo request to {url} failed: {message}"),
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

fn set_query_parameter(url: &mut Url, name: &str, value: &str) {
    let mut found = false;
    let mut parameters = Vec::new();
    for (key, current_value) in url.query_pairs() {
        if key == name {
            if !found {
                parameters.push((key.into_owned(), value.to_string()));
                found = true;
            }
        } else {
            parameters.push((key.into_owned(), current_value.into_owned()));
        }
    }
    if !found {
        parameters.push((name.to_string(), value.to_string()));
    }
    url.set_query(None);
    url.query_pairs_mut().extend_pairs(
        parameters
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
}

async fn search_duck_duck_go_preload_url(
    query: &str,
    max_results: usize,
    options: &SearchEngineOptions,
) -> Result<Vec<DuckDuckGoSearchResult>, FetchError> {
    let mut all_results = Vec::new();
    let mut search_url = Url::parse("https://duckduckgo.com/?t=h_&ia=web")
        .expect("valid DuckDuckGo URL");
    search_url.query_pairs_mut().append_pair("q", query);
    let page_html = fetch_duck_duck_go_text(
        search_url.as_str(),
        options,
        PRELOAD_PAGE_HEADERS,
        None,
    )
    .await?;
    let base_preload_url = extract_duck_duck_go_preload_url(&page_html);
    if base_preload_url.is_empty() {
        return Ok(all_results);
    }
    let mut preload_url = Url::parse(&base_preload_url).expect("trusted DuckDuckGo preload URL");
    let mut offset = 0usize;
    while all_results.len() < max_results {
        set_query_parameter(&mut preload_url, "s", &offset.to_string());
        let jsonp_text = fetch_duck_duck_go_text(
            preload_url.as_str(),
            options,
            PRELOAD_DATA_HEADERS,
            None,
        )
        .await?;
        let page_results = parse_duck_duck_go_jsonp(&jsonp_text);
        if page_results.is_empty() {
            break;
        }
        offset += page_results.len();
        for result in page_results {
            if all_results.len() >= max_results {
                break;
            }
            all_results.push(result);
        }
    }
    all_results.truncate(max_results);
    Ok(all_results)
}

fn html_body(query: &str, offset: Option<usize>) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("q", query);
    if let Some(offset) = offset {
        serializer
            .append_pair("s", &offset.to_string())
            .append_pair("dc", &offset.to_string())
            .append_pair("v", "l")
            .append_pair("o", "json")
            .append_pair("api", "d.js");
    }
    serializer.finish()
}

async fn search_duck_duck_go_html(
    query: &str,
    max_results: usize,
    options: &SearchEngineOptions,
) -> Result<Vec<DuckDuckGoSearchResult>, FetchError> {
    let mut all_results = Vec::new();
    let mut offset = 0usize;
    let first_html = fetch_duck_duck_go_text(
        HTML_RESULT_URL,
        options,
        HTML_HEADERS,
        Some(html_body(query, None)),
    )
    .await?;
    let mut page_results = parse_duck_duck_go_results(&first_html, max_results);
    all_results.extend(page_results.iter().cloned());
    while all_results.len() < max_results && !page_results.is_empty() {
        offset += page_results.len();
        let html = fetch_duck_duck_go_text(
            HTML_RESULT_URL,
            options,
            HTML_HEADERS,
            Some(html_body(query, Some(offset))),
        )
        .await?;
        page_results = parse_duck_duck_go_results(&html, max_results - all_results.len());
        all_results.extend(page_results.iter().cloned());
    }
    all_results.truncate(max_results);
    Ok(all_results)
}

fn to_web_search_result(result: DuckDuckGoSearchResult) -> WebSearchResult {
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

pub fn search_duck_duck_go(
    query: String,
    limit: usize,
    options: SearchEngineOptions,
) -> SearchEngineFuture {
    Box::pin(async move {
        match search_duck_duck_go_preload_url(&query, limit, &options).await {
            Ok(results) if !results.is_empty() => {
                return Ok(results.into_iter().map(to_web_search_result).collect());
            }
            Ok(_) => {}
            Err(error) if is_abort_error(error.as_ref()) => return Err(error),
            Err(_) => {}
        }
        let results = search_duck_duck_go_html(&query, limit, &options).await?;
        Ok(results.into_iter().map(to_web_search_result).collect())
    })
}
