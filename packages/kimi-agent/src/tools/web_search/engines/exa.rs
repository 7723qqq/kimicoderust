use std::time::Duration;

use serde_json::{Value, json};

use super::super::WebSearchResultEntry;

const EXA_SEARCH_URL: &str = "https://exa.ai/search/api/search-fast";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/112.0.0.0 Safari/537.36";

pub async fn search(query: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    let data = json!({
        "numResults": limit,
        "query": query,
        "type": "auto",
        "useAutoprompt": true,
        "domainFilterType": "include",
        "text": true,
        "density": "compact",
        "resolvedSearchType": "neural",
        "moderation": true,
        "fastMode": false,
        "rerankerType": "default",
    });
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("Exa request to {EXA_SEARCH_URL} failed: {error}"))?;
    let response = client
        .post(EXA_SEARCH_URL)
        .header("Connection", "keep-alive")
        .header("Accept", "*/*")
        .header("Accept-Encoding", "gzip, deflate, br")
        .header(
            "sec-ch-ua",
            "\"Chromium\";v=\"112\", \"Google Chrome\";v=\"112\", \"Not:A-Brand\";v=\"99\"",
        )
        .header("content-type", "text/plain;charset=UTF-8")
        .header("sec-ch-ua-mobile", "?0")
        .header("sec-ch-ua-platform", "\"Windows\"")
        .header("origin", "https://exa.ai")
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-dest", "empty")
        .header("accept-language", "zh-CN,zh;q=0.9,en;q=0.8")
        .body(data.to_string())
        .send()
        .await
        .map_err(|error| format!("Exa request to {EXA_SEARCH_URL} failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Exa request failed: HTTP {}.", status.as_u16()));
    }
    let body = response
        .text()
        .await
        .map_err(|error| format!("Exa request to {EXA_SEARCH_URL} failed: {error}"))?;
    parse_results(&body, limit)
}

fn parse_results(body: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| format!("Exa request returned invalid JSON: {error}"))?;
    let Some(items) = value.get("results").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    Ok(items
        .iter()
        .take(limit)
        .filter_map(|item| {
            let url = item.get("url")?.as_str()?;
            let hostname = url::Url::parse(url).ok()?.host_str()?.to_string();
            let author = item.get("author").and_then(Value::as_str).unwrap_or("N/A");
            let published = item
                .get("publishedDate")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(locale_date)
                .unwrap_or_else(|| "N/A".to_string());
            Some(WebSearchResultEntry {
                title: item
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("No title")
                    .to_string(),
                url: url.to_string(),
                snippet: format!("Author: {author}. Published: {published}"),
                site_name: Some(hostname),
                date: None,
            })
        })
        .collect())
}

fn locale_date(raw: &str) -> String {
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(raw) {
        let local = date.with_timezone(&chrono::Local);
        return format!(
            "{}/{}/{}",
            local.format("%-m"),
            local.format("%-d"),
            local.format("%Y")
        );
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return format!(
            "{}/{}/{}",
            date.format("%-m"),
            date.format("%-d"),
            date.format("%Y")
        );
    }
    "Invalid Date".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v2_result_fields_and_skips_invalid_urls() {
        let results = parse_results(
            r#"{"results":[{"title":"Rust","url":"https://example.com/rust","author":"Alice","publishedDate":"2026-01-02"},{"url":"not a url"},{"title":"missing url"}]}"#,
            10,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].site_name.as_deref(), Some("example.com"));
        assert_eq!(results[0].snippet, "Author: Alice. Published: 1/2/2026");
    }

    #[test]
    fn defaults_and_limit_match_v2() {
        let results = parse_results(
            r#"{"results":[{"url":"https://a.com"},{"url":"https://b.com"}]}"#,
            1,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "No title");
        assert_eq!(results[0].snippet, "Author: N/A. Published: N/A");
        assert!(parse_results("{}", 10).unwrap().is_empty());
        assert!(parse_results("invalid", 10).is_err());
        let invalid_first = parse_results(
            r#"{"results":[{"url":"not a url"},{"url":"https://valid.com"}]}"#,
            1,
        )
        .unwrap();
        assert!(invalid_first.is_empty());
    }
}
