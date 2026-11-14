use serde_json::Value;
use url::Url;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::utils::abort::{AbortError, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};
use crate::app::auth::web_search::web_search::WebSearchResult;

use super::super::engine_http::{EngineRequestOptions, debug_log, engine_fetch};
use super::super::types::{SearchEngineFuture, SearchEngineOptions};

const CSDN_SEARCH_URL: &str = "https://so.csdn.net/api/v3/search";
const REQUEST_TIMEOUT_MS: u64 = 30_000;
const FALLBACK_HEADERS: &[(&str, &str)] = &[
    ("Pragma", "no-cache"),
    ("Cookie", "uuid_tt_dd=10_20283040220-1750745713898-623562; dc_session_id=10_1750745713898.508399; dc_sid=0aa6fae5250c4389fac68320b1cb43b2; waf_captcha_marker=1b4e9099857d7aedf0941f03fa70bfb22ea2153f7fa053b8101ed28dc1504b11; c_pref=default; c_ref=default; fid=20_93458541565-1750745714849-027048; c_first_ref=default; c_first_page=https%3A//so.csdn.net/so/search%3Fq%3Dweb%2520search%2520mcp; c_dsid=11_1750745714849.980720; c_segment=10; c_page_id=default; creative_btn_mp=1; log_Id_view=9; fe_request_id=1750745715289_2973_2073791; dc_tos=syck1f; log_Id_pv=1; log_Id_click=1; uuid_tt_dd=10_20283045860-1751096847125-425142; dc_session_id=10_1751096847125.891975"),
    ("User-Agent", "Apifox/1.0.0 (https://apifox.com)"),
    ("Accept", "*/*"),
    ("Host", "so.csdn.net"),
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
        format!("CSDN search request failed: HTTP {status}."),
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
        format!("CSDN search request returned invalid JSON: {message}"),
        Error2Options {
            cause: Some(Box::new(error)),
            ..Default::default()
        },
    ))
}

async fn fetch_search_page(
    query: &str,
    page: usize,
    options: &SearchEngineOptions,
) -> Result<String, FetchError> {
    let mut search_url = Url::parse(CSDN_SEARCH_URL).expect("valid CSDN URL");
    search_url
        .query_pairs_mut()
        .append_pair("q", query)
        .append_pair("p", &page.to_string());
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
                format!("CSDN search request to {url} failed: {message}"),
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

pub fn search_csdn(
    query: String,
    limit: usize,
    options: SearchEngineOptions,
) -> SearchEngineFuture {
    Box::pin(async move {
        let mut all_results = Vec::new();
        let mut pn = 1usize;
        while all_results.len() < limit {
            let body = fetch_search_page(&query, pn, &options).await?;
            let parsed: Value = match serde_json::from_str(&body) {
                Ok(parsed) => parsed,
                Err(error) => return Err(invalid_json_error(error)),
            };
            let Some(result_vos) = parsed.get("result_vos").and_then(Value::as_array) else {
                break;
            };
            let mut results = Vec::new();
            for item in result_vos {
                let Some(title) = item.get("title").and_then(Value::as_str) else {
                    continue;
                };
                let Some(url) = item.get("url_location").and_then(Value::as_str) else {
                    continue;
                };
                results.push(WebSearchResult {
                    title: title.to_string(),
                    url: url.to_string(),
                    snippet: item
                        .get("digest")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    site_name: item
                        .get("nickname")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    date: None,
                });
            }
            let result_count = results.len();
            all_results.extend(results);
            if result_count == 0 {
                debug_log("No more CSDN results, ending early.");
                break;
            }
            pn += 1;
        }
        all_results.truncate(limit);
        Ok(all_results)
    })
}
