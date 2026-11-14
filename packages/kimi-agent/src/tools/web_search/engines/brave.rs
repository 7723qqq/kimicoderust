use std::time::Duration;

use scraper::{ElementRef, Html, Selector};

use super::super::WebSearchResultEntry;

const BRAVE_BASE_URL: &str = "https://search.brave.com/search";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/112.0.0.0 Safari/537.36";

pub async fn search(query: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("Brave search request failed: {error}"))?;
    let mut all_results = Vec::new();
    let mut offset = 0;
    while all_results.len() < limit {
        let mut url = url::Url::parse(BRAVE_BASE_URL).expect("valid Brave URL");
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("source", "web")
            .append_pair("offset", &offset.to_string());
        let url: String = url.into();
        let response = client
            .get(&url)
            .header("Connection", "keep-alive")
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7")
            .header("Accept-Encoding", "gzip, deflate, br")
            .header("sec-ch-ua", "\"Chromium\";v=\"112\", \"Google Chrome\";v=\"112\", \"Not:A-Brand\";v=\"99\"")
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", "\"Windows\"")
            .header("upgrade-insecure-requests", "1")
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-user", "?1")
            .header("sec-fetch-dest", "document")
            .header("referer", "https://duckduckgo.com/")
            .header("accept-language", "zh-CN,zh;q=0.9,en;q=0.8")
            .send()
            .await
            .map_err(|error| format!("Brave search request to {url} failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "Brave search request failed: HTTP {}.",
                status.as_u16()
            ));
        }
        let body = response
            .text()
            .await
            .map_err(|error| format!("Brave search request to {url} failed: {error}"))?;
        let results = parse_results(&body);
        if results.is_empty() {
            break;
        }
        all_results.extend(results);
        offset += 1;
    }
    all_results.truncate(limit);
    Ok(all_results)
}

fn parse_results(html: &str) -> Vec<WebSearchResultEntry> {
    let document = Html::parse_document(html);
    let snippet_selector = Selector::parse("#results .snippet").unwrap();
    let content_selector = Selector::parse(".result-content").unwrap();
    let title_selector = Selector::parse(".search-snippet-title").unwrap();
    let description_selector = Selector::parse(".generic-snippet").unwrap();
    let source_selector = Selector::parse(".site-name-wrapper").unwrap();
    let mut results = Vec::new();
    for snippet in document.select(&snippet_selector) {
        let Some(content) = snippet.select(&content_selector).next() else {
            continue;
        };
        let Some(link) = direct_child_anchor(content) else {
            continue;
        };
        let Some(url) = link.value().attr("href") else {
            continue;
        };
        let title = link
            .select(&title_selector)
            .next()
            .map(|element| element.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let description = content
            .select(&description_selector)
            .next()
            .map(|element| element.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let source = link
            .select(&source_selector)
            .next()
            .map(|element| element.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        results.push(WebSearchResultEntry {
            title,
            url: url.to_string(),
            snippet: description,
            site_name: (!source.is_empty()).then_some(source),
            date: None,
        });
    }
    results
}

fn direct_child_anchor(element: ElementRef<'_>) -> Option<ElementRef<'_>> {
    element
        .children()
        .filter_map(ElementRef::wrap)
        .find(|child| child.value().name() == "a")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_result_content_and_direct_links_only() {
        let results = parse_results(
            r#"<div id="results"><div class="snippet"><div class="result-content"><a href="https://example.com"><span class="search-snippet-title"> Rust </span><span class="site-name-wrapper">Example</span></a><div class="generic-snippet"> Safe language </div></div></div><div class="snippet"><a href="https://ignored.com">ignored</a></div></div>"#,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].url, "https://example.com");
        assert_eq!(results[0].snippet, "Safe language");
        assert_eq!(results[0].site_name.as_deref(), Some("Example"));
    }

    #[test]
    fn skips_empty_titles() {
        assert!(parse_results(r#"<div id="results"><div class="snippet"><div class="result-content"><a href="https://example.com"></a></div></div></div>"#).is_empty());
    }
}
