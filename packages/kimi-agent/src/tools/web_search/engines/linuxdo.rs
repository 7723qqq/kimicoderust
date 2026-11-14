use super::super::WebSearchResultEntry;

pub async fn search(query: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let site_query = format!("site:linux.do {query}");
    let mut results = super::bing::search(&site_query, limit).await?;
    if results.is_empty() {
        results = super::brave::search(&site_query, limit).await?;
    }
    Ok(results
        .into_iter()
        .filter(|result| {
            url::Url::parse(&result.url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
                .is_some_and(|host| host == "linux.do" || host.ends_with(".linux.do"))
        })
        .take(limit)
        .map(|mut result| {
            result.site_name = Some("linux.do".to_string());
            result
        })
        .collect())
}
