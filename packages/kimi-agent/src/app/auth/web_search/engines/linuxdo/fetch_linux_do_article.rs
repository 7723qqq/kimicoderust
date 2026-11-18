use regex::Regex;
use serde_json::Value;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::abort::{AbortError, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};

use super::super::engine_html::{load_html, query_html};
use super::super::engine_http::{EngineRequestOptions, engine_fetch};
use super::super::types::{ArticleContent, ArticleFetchFuture, SearchEngineOptions};

const REQUEST_TIMEOUT_MS: u64 = 30_000;
const FALLBACK_HEADERS: &[(&str, &str)] = &[
    ("accept", "application/json, text/javascript, */*; q=0.01"),
    ("accept-language", "zh-CN,zh;q=0.9"),
    ("cache-control", "no-cache"),
    ("discourse-track-view", "true"),
    ("pragma", "no-cache"),
    ("referer", "https://linux.do/search"),
    ("sec-ch-ua", "\"Chromium\";v=\"112\", \"Google Chrome\";v=\"112\", \"Not:A-Brand\";v=\"99\""),
    ("sec-ch-ua-mobile", "?0"),
    ("sec-ch-ua-platform", "\"Windows\""),
    ("sec-fetch-dest", "empty"),
    ("sec-fetch-mode", "cors"),
    ("sec-fetch-site", "same-origin"),
    ("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/112.0.0.0 Safari/537.36"),
    ("x-requested-with", "XMLHttpRequest"),
    ("Host", "linux.do"),
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
        format!("LinuxDo topic request failed: HTTP {status}."),
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
        format!("LinuxDo topic request returned invalid JSON: {message}"),
        Error2Options {
            cause: Some(Box::new(error)),
            ..Default::default()
        },
    ))
}

async fn fetch_topic_json(
    topic_id: &str,
    options: &SearchEngineOptions,
) -> Result<String, FetchError> {
    let api_url = format!("https://linux.do/t/{topic_id}.json");
    let response_result: Result<FetchResponse, FetchError> = if let Some(fetch) = &options.fetch_impl {
        fetch
            .fetch(
                &api_url,
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
            &api_url,
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
                format!("LinuxDo topic request to {api_url} failed: {message}"),
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

pub fn fetch_linux_do_article(url: String, options: SearchEngineOptions) -> ArticleFetchFuture {
    Box::pin(async move {
        let topic_id = Regex::new(r"/topic/(\d+)")
            .unwrap()
            .captures(&url)
            .and_then(|captures| captures.get(1))
            .map(|matched| matched.as_str().to_string());
        let Some(topic_id) = topic_id else {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid URL: Cannot extract topic ID.",
            )) as FetchError);
        };
        let body = fetch_topic_json(&topic_id, &options).await?;
        let parsed: Value = match serde_json::from_str(&body) {
            Ok(parsed) => parsed,
            Err(error) => return Err(invalid_json_error(error)),
        };
        let cooked_html = parsed
            .get("post_stream")
            .and_then(|stream| stream.get("posts"))
            .and_then(Value::as_array)
            .and_then(|posts| posts.first())
            .and_then(|post| post.get("cooked"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if cooked_html.is_empty() {
            return Ok(None);
        }
        let document = load_html(cooked_html);
        let plain_text = query_html(&document, "body").text().trim().to_string();
        Ok(Some(ArticleContent {
            content: plain_text,
            title: None,
        }))
    })
}
