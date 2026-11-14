use std::time::Duration;

use scraper::{Html, Selector};

use super::super::WebSearchResultEntry;

const BAIDU_BASE_URL: &str = "https://www.baidu.com/s";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";

pub async fn search(query: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("Baidu search request failed: {error}"))?;
    let mut all_results = Vec::new();
    let mut page = 0;
    while all_results.len() < limit {
        let url = search_url(query, page);
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|error| format!("Baidu search request failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "Baidu search request failed: HTTP {}.",
                status.as_u16()
            ));
        }
        let body = response
            .text()
            .await
            .map_err(|error| format!("Baidu search request failed: {error}"))?;
        let results = parse_results(&body);
        if results.is_empty() {
            break;
        }
        all_results.extend(results);
        page += 1;
    }
    all_results.truncate(limit);
    Ok(all_results)
}

fn search_url(query: &str, page: usize) -> String {
    let mut url = url::Url::parse(BAIDU_BASE_URL).expect("valid Baidu URL");
    url.query_pairs_mut()
        .append_pair("wd", query)
        .append_pair("pn", &(page * 10).to_string())
        .append_pair("ie", "utf-8")
        .append_pair("mod", "1")
        .append_pair("isbd", "1")
        .append_pair("isid", "f7ba1776007bcf9e")
        .append_pair("oq", query)
        .append_pair("tn", "88093251_62_hao_pg")
        .append_pair("usm", "1")
        .append_pair("fenlei", "256")
        .append_pair("rsv_idx", "1")
        .append_pair("rsv_pq", "f7ba1776007bcf9e")
        .append_pair(
            "rsv_t",
            "8179fxGiNMUh/0dXHrLsJXPlKYbkj9S5QH6rOLHY6pG6OGQ81YqzRTIGjjeMwEfiYQTSiTQIhCJj",
        )
        .append_pair("bs", query)
        .append_pair("_ss", "1")
        .append_pair("f4s", "1")
        .append_pair("csor", "5")
        .append_pair("_cr1", "30385");
    url.into()
}

fn parse_results(html: &str) -> Vec<WebSearchResultEntry> {
    let document = Html::parse_document(html);
    let content_selector = Selector::parse("#content_left").unwrap();
    let title_selector = Selector::parse("h3").unwrap();
    let link_selector = Selector::parse("a").unwrap();
    let snippet_baidu_selector = Selector::parse(".c-font-normal.c-color-text").unwrap();
    let snippet_selector = Selector::parse(".cos-row").unwrap();
    let source_selector = Selector::parse(".cosc-source").unwrap();
    let mut results = Vec::new();
    for content in document.select(&content_selector) {
        for element in content.children().filter_map(scraper::ElementRef::wrap) {
            let Some(title_element) = element.select(&title_selector).next() else {
                continue;
            };
            let Some(link) = element.select(&link_selector).next() else {
                continue;
            };
            let Some(url) = link.value().attr("href") else {
                continue;
            };
            if !url.starts_with("http") {
                continue;
            }
            let snippet = element
                .select(&snippet_baidu_selector)
                .next()
                .and_then(|node| node.value().attr("aria-label").map(str::to_string))
                .or_else(|| {
                    element
                        .select(&snippet_selector)
                        .next()
                        .map(|node| node.text().collect::<String>().trim().to_string())
                })
                .unwrap_or_default();
            let source = element
                .select(&source_selector)
                .flat_map(|node| node.text().collect::<Vec<_>>())
                .collect::<String>()
                .trim()
                .to_string();
            results.push(WebSearchResultEntry {
                title: title_element.text().collect(),
                url: url.to_string(),
                snippet,
                site_name: (!source.is_empty()).then_some(source),
                date: None,
            });
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_direct_result_children_and_snippet_priority() {
        let results = parse_results(
            r#"<div id="content_left"><div><h3>Rust</h3><a href="https://baidu.com/link">go</a><span class="c-font-normal c-color-text" aria-label="description"></span><span class="cosc-source">Source</span></div><div><h3>ignored</h3><a href="/relative">relative</a></div></div>"#,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].snippet, "description");
        assert_eq!(results[0].site_name.as_deref(), Some("Source"));
    }

    #[test]
    fn url_contains_all_v2_pagination_parameters() {
        let url = search_url("rust lang", 2);
        assert!(url.contains("wd=rust+lang"));
        assert!(url.contains("pn=20"));
        assert!(url.contains("ie=utf-8"));
        assert!(url.contains("mod=1"));
        assert!(url.contains("csor=5"));
    }
}
