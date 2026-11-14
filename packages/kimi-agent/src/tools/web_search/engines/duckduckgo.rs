use std::collections::HashSet;
use std::time::Duration;

use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

use super::super::WebSearchResultEntry;

const REQUEST_TIMEOUT_SECS: u64 = 30;
const PRELOAD_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const HTML_RESULT_URL: &str = "https://html.duckduckgo.com/html/";
const PRELOAD_PAGE_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", PRELOAD_USER_AGENT),
    (
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
    ),
    ("referer", "https://duckduckgo.com/"),
    ("accept-language", "zh-CN,zh;q=0.9,en;q=0.8"),
];
const PRELOAD_DATA_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", PRELOAD_USER_AGENT),
    ("Accept", "*/*"),
    ("referer", "https://duckduckgo.com/"),
];
const HTML_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "application/x-www-form-urlencoded"),
    ("User-Agent", "Apifox/1.0.0 (https://apifox.com)"),
    ("Accept", "*/*"),
    ("Host", "html.duckduckgo.com"),
    ("Connection", "keep-alive"),
];

pub async fn search(query: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    let client = reqwest::Client::builder()
        .user_agent(PRELOAD_USER_AGENT)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .map_err(|error| format!("DuckDuckGo request failed: {error}"))?;
    if let Ok(results) = search_preload_url(&client, query, limit).await
        && !results.is_empty()
    {
        return Ok(results);
    }
    search_html(&client, query, limit).await
}

async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<String>,
) -> Result<String, String> {
    let mut request = if body.is_some() {
        client.post(url)
    } else {
        client.get(url)
    };
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("accept-language"))
    {
        request = request.header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8");
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("DuckDuckGo request to {url} failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "DuckDuckGo request failed: HTTP {}.",
            status.as_u16()
        ));
    }
    response
        .text()
        .await
        .map_err(|error| format!("DuckDuckGo request to {url} failed: {error}"))
}

async fn search_preload_url(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<Vec<WebSearchResultEntry>, String> {
    let mut all_results = Vec::new();
    let search_url = preload_search_url(query);
    let page_html = fetch_text(client, &search_url, PRELOAD_PAGE_HEADERS, None).await?;
    let base_preload_url = extract_preload_url(&page_html);
    if base_preload_url.is_empty() {
        return Ok(all_results);
    }
    let mut preload_url = Url::parse(&base_preload_url)
        .map_err(|error| format!("DuckDuckGo request to {base_preload_url} failed: {error}"))?;
    let mut offset = 0usize;
    while all_results.len() < max_results {
        set_query_parameter(&mut preload_url, "s", &offset.to_string());
        let jsonp_text =
            fetch_text(client, preload_url.as_str(), PRELOAD_DATA_HEADERS, None).await?;
        let page_results = parse_jsonp(&jsonp_text);
        if page_results.is_empty() {
            break;
        }
        offset += page_results.len();
        for result in page_results {
            if all_results.len() >= max_results {
                break;
            }
            all_results.push(result);
        }
    }
    all_results.truncate(max_results);
    Ok(all_results)
}

async fn search_html(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<Vec<WebSearchResultEntry>, String> {
    let mut all_results = Vec::new();
    let mut offset = 0usize;
    let first_html = fetch_text(
        client,
        HTML_RESULT_URL,
        HTML_HEADERS,
        Some(html_body(query, None)),
    )
    .await?;
    let mut page_results = parse_results(&first_html, max_results);
    all_results.extend(page_results.iter().cloned());
    while all_results.len() < max_results && !page_results.is_empty() {
        offset += page_results.len();
        let html = fetch_text(
            client,
            HTML_RESULT_URL,
            HTML_HEADERS,
            Some(html_body(query, Some(offset))),
        )
        .await?;
        page_results = parse_results(&html, max_results - all_results.len());
        all_results.extend(page_results.iter().cloned());
    }
    all_results.truncate(max_results);
    Ok(all_results)
}

fn preload_search_url(query: &str) -> String {
    let mut url = Url::parse("https://duckduckgo.com/?t=h_&ia=web").expect("valid DuckDuckGo URL");
    url.query_pairs_mut().append_pair("q", query);
    url.to_string()
}

fn set_query_parameter(url: &mut Url, name: &str, value: &str) {
    let mut found = false;
    let mut parameters = Vec::new();
    for (key, current_value) in url.query_pairs() {
        if key == name {
            if !found {
                parameters.push((key.into_owned(), value.to_string()));
                found = true;
            }
        } else {
            parameters.push((key.into_owned(), current_value.into_owned()));
        }
    }
    if !found {
        parameters.push((name.to_string(), value.to_string()));
    }
    url.set_query(None);
    url.query_pairs_mut().extend_pairs(
        parameters
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
}

fn html_body(query: &str, offset: Option<usize>) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("q", query);
    if let Some(offset) = offset {
        serializer
            .append_pair("s", &offset.to_string())
            .append_pair("dc", &offset.to_string())
            .append_pair("v", "l")
            .append_pair("o", "json")
            .append_pair("api", "d.js");
    }
    serializer.finish()
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn has_class(element: &scraper::ElementRef<'_>, class_name: &str) -> bool {
    element
        .value()
        .attr("class")
        .is_some_and(|classes| classes.split_whitespace().any(|class| class == class_name))
}

fn is_trusted_preload_url(value: &str) -> bool {
    let Ok(parsed) = Url::parse(value) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.host_str() == Some("links.duckduckgo.com")
        && parsed.port().is_none_or(|port| port == 443)
        && parsed.username().is_empty()
        && parsed.password().is_none_or(str::is_empty)
        && parsed.path() == "/d.js"
}

fn extract_preload_url(page_html: &str) -> String {
    let document = Html::parse_document(page_html);
    let preload_selector = Selector::parse("link[rel=\"preload\"]").unwrap();
    for element in document.select(&preload_selector) {
        if let Some(href) = element.value().attr("href") {
            if is_trusted_preload_url(href) {
                return href.to_string();
            }
        }
    }
    let script_selector = Selector::parse("#deep_preload_script").unwrap();
    for element in document.select(&script_selector) {
        if let Some(src) = element.value().attr("src") {
            if is_trusted_preload_url(src) {
                return src.to_string();
            }
        }
    }
    let pattern =
        regex::Regex::new(r#"(?i)https://links\.duckduckgo\.com/d\.js\?[^"'<>]+"#).unwrap();
    if let Some(matched) = pattern.find(page_html) {
        if is_trusted_preload_url(matched.as_str()) {
            return matched.as_str().to_string();
        }
    }
    String::new()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn sanitize_duckduckgo_url(raw_url: Option<&str>) -> String {
    let Some(raw_url) = raw_url else {
        return String::new();
    };
    let mut resolved_url = raw_url.trim().to_string();
    if resolved_url.is_empty() {
        return String::new();
    }
    if resolved_url.starts_with("//") {
        resolved_url = format!("https:{resolved_url}");
    }
    let Ok(url) = Url::parse(&resolved_url) else {
        return String::new();
    };
    if url
        .host_str()
        .is_some_and(|hostname| hostname.ends_with("duckduckgo.com"))
        && url.path().starts_with("/l/")
    {
        let target = url
            .query_pairs()
            .find(|(name, _)| name == "uddg")
            .map(|(_, value)| value.trim().to_string());
        if let Some(target) = target.filter(|target| !target.is_empty()) {
            let decoded_target = decode_uri_component(&target).unwrap_or(target);
            if decoded_target.starts_with("http://") || decoded_target.starts_with("https://") {
                return decoded_target;
            }
        }
        return String::new();
    }
    url.to_string()
}

fn parse_results(html: &str, limit: usize) -> Vec<WebSearchResultEntry> {
    let document = Html::parse_document(html);
    let result_selector = Selector::parse("div.result").unwrap();
    let title_selector = Selector::parse("a.result__a").unwrap();
    let snippet_selector = Selector::parse(".result__snippet").unwrap();
    let source_selector = Selector::parse(".result__url").unwrap();
    let mut results = Vec::new();
    let mut seen_urls = HashSet::new();
    for element in document.select(&result_selector) {
        if results.len() >= limit {
            break;
        }
        if has_class(&element, "result--ad") {
            continue;
        }
        let title_element = element.select(&title_selector).next();
        let title = normalize_whitespace(
            &title_element
                .map(|element| element.text().collect::<String>())
                .unwrap_or_default(),
        );
        let url =
            sanitize_duckduckgo_url(title_element.and_then(|element| element.value().attr("href")));
        if title.is_empty() || url.is_empty() || seen_urls.contains(&url) {
            continue;
        }
        let description = normalize_whitespace(
            &element
                .select(&snippet_selector)
                .next()
                .map(|element| element.text().collect::<String>())
                .unwrap_or_default(),
        );
        let source = normalize_whitespace(
            &element
                .select(&source_selector)
                .next()
                .map(|element| element.text().collect::<String>())
                .unwrap_or_default(),
        );
        seen_urls.insert(url.clone());
        results.push(WebSearchResultEntry {
            title,
            url,
            snippet: description,
            site_name: (!source.is_empty()).then_some(source),
            date: None,
        });
    }
    results
}

fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

fn json_string(item: &Value, key: &str) -> Option<String> {
    item.get(key)
        .and_then(|value| {
            if value.is_null() {
                None
            } else {
                value.as_str()
            }
        })
        .map(str::to_string)
}

fn parse_jsonp(jsonp_text: &str) -> Vec<WebSearchResultEntry> {
    let pattern = regex::Regex::new(r"(?s)DDG\.pageLayout\.load\('d',\s*(\[.*?\])\s*\);").unwrap();
    let Some(captures) = pattern.captures(jsonp_text) else {
        return Vec::new();
    };
    let Some(raw_json) = captures.get(1).map(|matched| matched.as_str()) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(raw_json) else {
        return Vec::new();
    };
    let Some(items) = parsed.as_array() else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for item in items {
        if !item.is_object() && !item.is_array() {
            continue;
        }
        if js_truthy(item.get("n")) {
            continue;
        }
        let source = json_string(item, "i")
            .or_else(|| json_string(item, "sn"))
            .unwrap_or_default();
        results.push(WebSearchResultEntry {
            title: json_string(item, "t").unwrap_or_default(),
            url: json_string(item, "u").unwrap_or_default(),
            snippet: json_string(item, "a").unwrap_or_default(),
            site_name: (!source.is_empty()).then_some(source),
            date: None,
        });
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preload_urls_require_the_exact_https_endpoint() {
        assert!(is_trusted_preload_url(
            "https://links.duckduckgo.com/d.js?q=rust"
        ));
        assert!(is_trusted_preload_url(
            "https://links.duckduckgo.com:443/d.js?q=rust"
        ));
        assert!(!is_trusted_preload_url(
            "http://links.duckduckgo.com/d.js?q=rust"
        ));
        assert!(!is_trusted_preload_url(
            "https://links.duckduckgo.com.evil.test/d.js?q=rust"
        ));
        assert!(!is_trusted_preload_url(
            "https://user@links.duckduckgo.com/d.js?q=rust"
        ));
        assert!(!is_trusted_preload_url(
            "https://links.duckduckgo.com/other.js?q=rust"
        ));
    }

    #[test]
    fn extracts_preload_from_dom_before_raw_html() {
        let html = r#"<link rel="preload" href="https://links.duckduckgo.com/d.js?q=dom"><script id="deep_preload_script" src="https://links.duckduckgo.com/d.js?q=script"></script><script>const url="https://links.duckduckgo.com/d.js?q=raw";</script>"#;
        assert_eq!(
            extract_preload_url(html),
            "https://links.duckduckgo.com/d.js?q=dom"
        );
        assert_eq!(
            extract_preload_url(
                r#"<script id="deep_preload_script" src="https://links.duckduckgo.com/d.js?q=script"></script>"#
            ),
            "https://links.duckduckgo.com/d.js?q=script"
        );
        assert_eq!(
            extract_preload_url(
                r#"<script>const url="https://links.duckduckgo.com/d.js?q=raw";</script>"#
            ),
            "https://links.duckduckgo.com/d.js?q=raw"
        );
        assert_eq!(extract_preload_url("<html></html>"), "");
    }

    #[test]
    fn parses_jsonp_with_v2_filter_and_source_precedence() {
        let results = parse_jsonp(
            r#"DDG.pageLayout.load('d', [{"t":"One","u":"https://example.com","a":"Snippet","i":"icon","sn":"site"},{"n":"next","t":"ignored"},{"n":0,"t":"Zero","sn":"fallback"},[]]);"#,
        );
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].title, "One");
        assert_eq!(results[0].site_name.as_deref(), Some("icon"));
        assert_eq!(results[1].title, "Zero");
        assert_eq!(results[1].site_name.as_deref(), Some("fallback"));
        assert_eq!(results[2].title, "");
        assert!(parse_jsonp("not jsonp").is_empty());
    }

    #[test]
    fn parses_html_results_with_double_redirect_decoding_and_deduplication() {
        let html = r#"<div class="results"><div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%253A%252F%252Fexample.com%252Fa%253Fx%253D1%2526y%253D2"> Rust   language </a><div class="result__snippet"> Safe   code </div><div class="result__url"> example.com </div></div><div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%253A%252F%252Fexample.com%252Fa%253Fx%253D1%2526y%253D2">duplicate</a></div><div class="result result--ad"><a class="result__a" href="https://ad.example/">ad</a></div><div class="result"><a class="result__a" href="//example.com/direct">Direct</a></div></div>"#;
        let results = parse_results(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust language");
        assert_eq!(results[0].url, "https://example.com/a?x=1&y=2");
        assert_eq!(results[0].snippet, "Safe code");
        assert_eq!(results[0].site_name.as_deref(), Some("example.com"));
        assert_eq!(results[1].url, "https://example.com/direct");
    }

    #[test]
    fn request_urls_and_forms_match_v2_pagination() {
        let initial = preload_search_url("rust lang");
        assert_eq!(initial, "https://duckduckgo.com/?t=h_&ia=web&q=rust+lang");
        assert_eq!(html_body("rust lang", None), "q=rust+lang");
        assert_eq!(
            html_body("rust lang", Some(20)),
            "q=rust+lang&s=20&dc=20&v=l&o=json&api=d.js"
        );
    }
}
