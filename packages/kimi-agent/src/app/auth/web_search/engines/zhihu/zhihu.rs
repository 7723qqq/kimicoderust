use url::Url;

use super::super::bing::search_bing;
use super::super::duckduckgo::search_duck_duck_go;
use super::super::types::{SearchEngineFuture, SearchEngineOptions};

const DEFAULT_DELEGATE_ENGINE: &str = "bing";
const ZHIHU_COLUMN_HOSTNAME: &str = "zhuanlan.zhihu.com";

pub fn search_zhihu(
    query: String,
    limit: usize,
    options: SearchEngineOptions,
) -> SearchEngineFuture {
    Box::pin(async move {
        let site_query = format!("site:zhuanlan.zhihu.com {query}");
        let results = if DEFAULT_DELEGATE_ENGINE == "duckduckgo" {
            search_duck_duck_go(site_query, limit, options).await?
        } else {
            search_bing(site_query, limit, options).await?
        };
        Ok(results
            .into_iter()
            .filter(|result| {
                Url::parse(&result.url)
                    .ok()
                    .and_then(|url| {
                        url.host_str()
                            .map(|hostname| hostname == ZHIHU_COLUMN_HOSTNAME)
                    })
                    .unwrap_or(false)
            })
            .take(limit)
            .map(|mut result| {
                result.site_name = Some(ZHIHU_COLUMN_HOSTNAME.to_string());
                result
            })
            .collect())
    })
}
