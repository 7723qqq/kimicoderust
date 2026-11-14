use super::super::WebSearchResultEntry;

pub async fn search(query: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let site_query = format!("site:zhuanlan.zhihu.com {query}");
    let results = super::bing::search(&site_query, limit).await?;
    Ok(results
        .into_iter()
        .filter(|result| {
            url::Url::parse(&result.url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
                .is_some_and(|host| host == "zhuanlan.zhihu.com")
        })
        .take(limit)
        .map(|mut result| {
            result.site_name = Some("zhuanlan.zhihu.com".to_string());
            result
        })
        .collect())
}
