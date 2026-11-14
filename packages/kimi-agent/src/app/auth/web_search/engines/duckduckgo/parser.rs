use std::collections::HashSet;

use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuckDuckGoSearchResult {
    pub title: String,
    pub url: String,
    pub description: String,
    pub source: String,
    pub engine: &'static str,
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn has_class(element: &ElementRef<'_>, class_name: &str) -> bool {
    element
        .value()
        .attr("class")
        .is_some_and(|classes| classes.split_whitespace().any(|class| class == class_name))
}

pub fn is_trusted_duck_duck_go_preload_url(value: &str) -> bool {
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

fn sanitize_duck_duck_go_url(raw_url: Option<&str>) -> String {
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

pub fn extract_duck_duck_go_preload_url(page_html: &str) -> String {
    let document = Html::parse_document(page_html);
    let preload_selector = Selector::parse("link[rel=\"preload\"]").unwrap();
    for element in document.select(&preload_selector) {
        if let Some(href) = element.value().attr("href")
            && is_trusted_duck_duck_go_preload_url(href)
        {
            return href.to_string();
        }
    }
    let script_selector = Selector::parse("#deep_preload_script").unwrap();
    for element in document.select(&script_selector) {
        if let Some(src) = element.value().attr("src")
            && is_trusted_duck_duck_go_preload_url(src)
        {
            return src.to_string();
        }
    }
    let pattern =
        regex::Regex::new(r#"(?i)https://links\.duckduckgo\.com/d\.js\?[^"'<>]+"#).unwrap();
    if let Some(matched) = pattern.find(page_html)
        && is_trusted_duck_duck_go_preload_url(matched.as_str())
    {
        return matched.as_str().to_string();
    }
    String::new()
}

pub fn parse_duck_duck_go_results(
    html_content: &str,
    limit: usize,
) -> Vec<DuckDuckGoSearchResult> {
    let document = Html::parse_document(html_content);
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
        let url = sanitize_duck_duck_go_url(
            title_element.and_then(|element| element.value().attr("href")),
        );
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
        results.push(DuckDuckGoSearchResult {
            title,
            url,
            description,
            source,
            engine: "duckduckgo",
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

pub fn parse_duck_duck_go_jsonp(jsonp_text: &str) -> Vec<DuckDuckGoSearchResult> {
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
    for raw_item in items {
        if !raw_item.is_object() && !raw_item.is_array() {
            continue;
        }
        if js_truthy(raw_item.get("n")) {
            continue;
        }
        results.push(DuckDuckGoSearchResult {
            title: json_string(raw_item, "t").unwrap_or_default(),
            url: json_string(raw_item, "u").unwrap_or_default(),
            description: json_string(raw_item, "a").unwrap_or_default(),
            source: json_string(raw_item, "i")
                .or_else(|| json_string(raw_item, "sn"))
                .unwrap_or_default(),
            engine: "duckduckgo",
        });
    }
    results
}
