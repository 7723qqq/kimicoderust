use url::Url;

use crate::app::auth::web_search::web_search::WebSearchResult;

use super::super::bing::search_bing;
use super::super::brave::search_brave;
use super::super::duckduckgo::search_duck_duck_go;
use super::super::types::{SearchEngineFuture, SearchEngineOptions};

const DEFAULT_DELEGATE_ENGINE: &str = "bing";

fn is_linux_do_hostname(hostname: &str) -> bool {
    hostname == "linux.do" || hostname.ends_with(".linux.do")
}

pub fn search_linux_do(
    query: String,
    limit: usize,
    options: SearchEngineOptions,
) -> SearchEngineFuture {
    Box::pin(async move {
        let site_query = format!("site:linux.do {query}");
        let mut results: Vec<WebSearchResult> = match DEFAULT_DELEGATE_ENGINE {
            "duckduckgo" => {
                search_duck_duck_go(site_query.clone(), limit, options.clone()).await?
            }
            "bing" => search_bing(site_query.clone(), limit, options.clone()).await?,
            _ => search_brave(site_query.clone(), limit, options.clone()).await?,
        };
        if results.is_empty() && DEFAULT_DELEGATE_ENGINE != "brave" {
            results = search_brave(site_query, limit, options).await?;
        }
        Ok(results
            .into_iter()
            .filter(|result| {
                Url::parse(&result.url)
                    .ok()
                    .and_then(|url| url.host_str().map(is_linux_do_hostname))
                    .unwrap_or(false)
            })
            .take(limit)
            .map(|mut result| {
                result.site_name = Some("linux.do".to_string());
                result
            })
            .collect())
    })
}
