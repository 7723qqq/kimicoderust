use regex::Regex;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::abort::{AbortError, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};

use super::super::engine_html::{load_html, query_html};
use super::super::engine_http::{EngineRequestOptions, engine_fetch};
use super::super::types::{ArticleContent, ArticleFetchFuture, SearchEngineOptions};

const REQUEST_TIMEOUT_MS: u64 = 30_000;
const FALLBACK_HEADERS: &[(&str, &str)] = &[
    ("Accept", "*/*"),
    ("Host", "blog.csdn.net"),
    ("Connection", "keep-alive"),
    ("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36"),
];
const BOT_KEYWORDS: &[&str] = &[
    "captcha",
    "verification",
    "verify you are human",
    "access denied",
    "blocked",
    "rate limit",
    "too many requests",
    "please enable javascript",
    "please verify",
    "请验证",
    "验证码",
    "人机验证",
    "安全验证",
];

fn fallback_headers() -> Vec<(String, String)> {
    FALLBACK_HEADERS
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn normalize_extracted_text(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\u{00a0}', " ");
    let text = Regex::new(r"[ \t]+\n")
        .unwrap()
        .replace_all(&text, "\n")
        .into_owned();
    let text = Regex::new(r"\n[ \t]+")
        .unwrap()
        .replace_all(&text, "\n")
        .into_owned();
    let text = Regex::new(r"[ \t]{2,}")
        .unwrap()
        .replace_all(&text, " ")
        .into_owned();
    Regex::new(r"\n{3,}")
        .unwrap()
        .replace_all(&text, "\n\n")
        .trim()
        .to_string()
}

fn js_length(value: &str) -> usize {
    value.encode_utf16().count()
}

fn extract_article_content(html: &str) -> String {
    let document = load_html(html);
    let article = query_html(&document, "#content_views").first();
    normalize_extracted_text(&article.text_without("script, style, noscript"))
}

fn looks_like_bot_challenge_page(html: &str) -> bool {
    let normalized = html.to_lowercase();
    BOT_KEYWORDS
        .iter()
        .any(|keyword| normalized.contains(keyword))
}

fn status_error(status: u16) -> FetchError {
    Box::new(Error2::with_options(
        WEB_FETCH_FAILED,
        format!("CSDN article request failed: HTTP {status}."),
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
                format!("CSDN article request to {url} failed: {message}"),
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

pub fn fetch_csdn_article(url: String, options: SearchEngineOptions) -> ArticleFetchFuture {
    Box::pin(async move {
        let html = fetch_article_html(&url, &options).await?;
        let content = extract_article_content(&html);
        if content.is_empty() || (looks_like_bot_challenge_page(&html) && js_length(&content) < 200) {
            return Ok(None);
        }
        Ok(Some(ArticleContent {
            content,
            title: None,
        }))
    })
}
