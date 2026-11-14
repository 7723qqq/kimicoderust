use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::abort::{AbortError, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};

use super::super::engine_html::{load_html, query_html};
use super::super::engine_http::{EngineRequestOptions, engine_fetch};
use super::super::types::{ArticleContent, ArticleFetchFuture, SearchEngineOptions};

const REQUEST_TIMEOUT_MS: u64 = 30_000;
const FALLBACK_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "Mozilla/5.0 (iPhone; CPU iPhone OS 16_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.6 Mobile/15E148 Safari/604.1"),
    ("Connection", "keep-alive"),
    ("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7"),
    ("Accept-Encoding", "gzip, deflate, br, zstd"),
    ("pragma", "no-cache"),
    ("cache-control", "no-cache"),
    ("upgrade-insecure-requests", "1"),
    ("sec-fetch-site", "none"),
    ("sec-fetch-mode", "navigate"),
    ("sec-fetch-user", "?1"),
    ("sec-fetch-dest", "document"),
    ("accept-language", "zh-CN,zh;q=0.9"),
    ("priority", "u=0, i"),
];
const CONTENT_SELECTORS: &[&str] = &[
    ".markdown-body",
    ".article-content",
    ".content",
    "[data-v-md-editor-preview]",
    ".bytemd-preview",
    ".article-area .content",
    ".main-area .article-area",
    ".article-wrapper .content",
];

fn fallback_headers() -> Vec<(String, String)> {
    FALLBACK_HEADERS
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn js_length(value: &str) -> usize {
    value.encode_utf16().count()
}

fn extract_article_content(html: &str) -> String {
    let document = load_html(html);
    let mut content = String::new();
    for selector in CONTENT_SELECTORS {
        let element = query_html(&document, selector).first();
        if !element.is_empty() {
            content = element
                .text_without("script, style, .code-block-extension, .hljs-ln-numbers")
                .trim()
                .to_string();
            if js_length(&content) > 100 {
                break;
            }
        }
    }
    if content.is_empty() || js_length(&content) < 100 {
        let body = query_html(&document, "body");
        content = if body.is_empty() {
            String::new()
        } else {
            body.text().trim().to_string()
        };
    }
    content
}

fn status_error(status: u16) -> FetchError {
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Juejin article request failed: HTTP {status}."),
        Error2Options {
            details: Some(serde_json::Map::from_iter([(
                "status".to_string(),
                serde_json::Value::from(status),
            )])),
            ..Default::default()
        },
    ))
}

async fn fetch_article_html(
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
                format!("Juejin article request to {url} failed: {message}"),
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

pub fn fetch_juejin_article(url: String, options: SearchEngineOptions) -> ArticleFetchFuture {
    Box::pin(async move {
        let html = fetch_article_html(&url, &options).await?;
        let content = extract_article_content(&html);
        if content.is_empty() {
            return Ok(None);
        }
        Ok(Some(ArticleContent {
            content,
            title: None,
        }))
    })
}
