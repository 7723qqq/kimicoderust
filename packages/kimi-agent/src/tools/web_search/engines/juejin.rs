use std::time::Duration;

use serde_json::Value;

use super::super::WebSearchResultEntry;

const JUEJIN_SEARCH_URL: &str = "https://api.juejin.cn/search_api/v1/search";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/112.0.0.0 Safari/537.36";

pub async fn search(query: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("Juejin search request failed: {error}"))?;
    let mut all_results = Vec::new();
    let mut cursor = "0".to_string();
    while all_results.len() < limit {
        let page_limit = 20.min(limit - all_results.len());
        let url = search_url(query, &cursor, page_limit);
        let response = client
            .get(&url)
            .header("pragma", "no-cache")
            .header("priority", "u=1, i")
            .header("content-type", "application/json")
            .header("Accept", "*/*")
            .header("Host", "api.juejin.cn")
            .header("Connection", "keep-alive")
            .send()
            .await
            .map_err(|error| format!("Juejin search request to {url} failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "Juejin search request failed: HTTP {}.",
                status.as_u16()
            ));
        }
        let body = response
            .text()
            .await
            .map_err(|error| format!("Juejin search request to {url} failed: {error}"))?;
        let value: Value = serde_json::from_str(&body)
            .map_err(|error| format!("Juejin search request returned invalid JSON: {error}"))?;
        if value.get("err_no").and_then(Value::as_i64) != Some(0) {
            break;
        }
        let Some(data) = value.get("data").and_then(Value::as_array) else {
            break;
        };
        let results = parse_results(data);
        let has_more = value.get("has_more").and_then(Value::as_bool) == Some(true);
        let next_cursor = value
            .get("cursor")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let empty = results.is_empty();
        all_results.extend(results);
        if !has_more || next_cursor.is_empty() || empty {
            break;
        }
        cursor = next_cursor;
    }
    all_results.truncate(limit);
    Ok(all_results)
}

fn search_url(query: &str, cursor: &str, limit: usize) -> String {
    let mut url = url::Url::parse(JUEJIN_SEARCH_URL).expect("valid Juejin URL");
    url.query_pairs_mut()
        .append_pair("aid", "2608")
        .append_pair("uuid", "7259393293459605051")
        .append_pair("spider", "0")
        .append_pair("query", query)
        .append_pair("id_type", "0")
        .append_pair("cursor", cursor)
        .append_pair("limit", &limit.to_string())
        .append_pair("search_type", "0")
        .append_pair("sort_type", "0")
        .append_pair("version", "1");
    url.into()
}

fn parse_results(data: &[Value]) -> Vec<WebSearchResultEntry> {
    data.iter()
        .filter_map(|item| {
            let model = item.get("result_model")?;
            let article_id = model.get("article_id")?.as_str()?;
            let title = strip_em(item.get("title_highlight").and_then(Value::as_str));
            let content = strip_em(item.get("content_highlight").and_then(Value::as_str));
            let tags = model
                .get("tags")
                .and_then(Value::as_array)
                .map(|tags| {
                    tags.iter()
                        .filter_map(|tag| tag.get("tag_name").and_then(Value::as_str))
                        .filter(|name| !name.is_empty())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let category = model
                .get("category")
                .and_then(|category| category.get("category_name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let digg = model
                .get("article_info")
                .and_then(|info| info.get("digg_count"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let views = model
                .get("article_info")
                .and_then(|info| info.get("view_count"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            Some(WebSearchResultEntry {
                title: if title.is_empty() {
                    "No title".to_string()
                } else {
                    title
                },
                url: format!("https://juejin.cn/post/{article_id}"),
                snippet: format!(
                    "{content} | 分类: {category} | 标签: {tags} | 👍 {digg} | 👀 {views}"
                ),
                site_name: model
                    .get("author_user_info")
                    .and_then(|author| author.get("user_name"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                date: None,
            })
        })
        .collect()
}

fn strip_em(value: Option<&str>) -> String {
    value
        .unwrap_or_default()
        .replace("<em>", "")
        .replace("</em>", "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_highlights_metadata_and_fallback_title() {
        let data = json!([{
            "result_model": {
                "article_id": "123",
                "article_info": {"digg_count": 4, "view_count": 20},
                "author_user_info": {"user_name": "Alice"},
                "category": {"category_name": "后端"},
                "tags": [{"tag_name": "Rust"}, {"tag_name": " async "}]
            },
            "title_highlight": "<em>Rust</em>",
            "content_highlight": "safe <em>async</em>"
        }]);
        let results = parse_results(data.as_array().unwrap());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].url, "https://juejin.cn/post/123");
        assert_eq!(
            results[0].snippet,
            "safe async | 分类: 后端 | 标签: Rust,  async  | 👍 4 | 👀 20"
        );
        assert_eq!(results[0].site_name.as_deref(), Some("Alice"));
    }

    #[test]
    fn skips_items_without_article_id() {
        let data = json!([{"result_model": {}, "title_highlight": "missing"}]);
        assert!(parse_results(data.as_array().unwrap()).is_empty());
        let empty_title = json!([{"result_model": {"article_id": "1"}}]);
        assert_eq!(
            parse_results(empty_title.as_array().unwrap())[0].title,
            "No title"
        );
    }

    #[test]
    fn url_contains_cursor_and_page_limit() {
        let url = search_url("rust lang", "20", 10);
        assert!(url.contains("query=rust+lang"));
        assert!(url.contains("cursor=20"));
        assert!(url.contains("limit=10"));
        assert!(url.contains("aid=2608"));
    }
}
