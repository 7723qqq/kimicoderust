use std::collections::HashSet;

use regex::Regex;
use url::Url;

use super::super::engine_html::{load_html, query_html};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SogouSearchResult {
    pub title: String,
    pub url: String,
    pub description: String,
    pub source: String,
    pub engine: &'static str,
}

const SOGOU_SEARCH_URL: &str = "https://www.sogou.com/web";

fn normalize_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn is_sogou_challenge_page(html: &str) -> bool {
    let normalized = html.to_lowercase();
    let document = load_html(html);
    let title = query_html(&document, "title").first().text();
    let title = title.trim();
    normalized.contains("antispider")
        || normalized.contains("请输入验证码")
        || normalized.contains("访问过于频繁")
        || title.contains("搜狗搜索验证")
}

fn query_parameter(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

pub fn resolve_result_url(raw_url: &str) -> String {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Ok(base) = Url::parse(SOGOU_SEARCH_URL) else {
        return String::new();
    };
    let Ok(parsed) = base.join(trimmed) else {
        return String::new();
    };
    let target = query_parameter(&parsed, "url")
        .or_else(|| query_parameter(&parsed, "u"))
        .or_else(|| query_parameter(&parsed, "link"));
    if let Some(target) = target
        && !target.is_empty()
        && Regex::new(r"(?i)^https?://")
            .unwrap()
            .is_match(&target)
    {
        return target;
    }
    if parsed.scheme() == "http" || parsed.scheme() == "https" {
        return parsed.to_string();
    }
    String::new()
}

pub fn extract_source(url: &str, source_text: &str) -> String {
    let cleaned_source = normalize_text(source_text);
    if !cleaned_source.is_empty() {
        return cleaned_source;
    }
    Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn parse_sogou_search_results(html: &str) -> Vec<SogouSearchResult> {
    let document = load_html(html);
    let results = query_html(
        &document,
        "#main .vrwrap,#main .rb,#main .result,#results .vrwrap,.results .vrwrap,.results .rb",
    );
    let mut parsed_results = Vec::new();
    let mut seen_urls = HashSet::new();
    results.each(|_, element| {
        let title_link = element.query_selector(
            "h3 a[href], h2 a[href], .vr-title a[href], .pt a[href]",
        );
        let raw_url = title_link
            .and_then(|element| element.get_attribute("href"))
            .unwrap_or_default();
        let url = resolve_result_url(raw_url);
        let title = normalize_text(
            &title_link
                .map(|element| element.text_content())
                .unwrap_or_default(),
        );
        if title.is_empty() || url.is_empty() || seen_urls.contains(&url) {
            return;
        }
        let description = normalize_text(
            &element
                .query_selector(".str_info, .ft, .text-layout, .fz-mid, p")
                .map(|element| element.text_content())
                .unwrap_or_default(),
        );
        let source = extract_source(
            &url,
            &element
                .query_selector("cite, .citeurl, .g, .url")
                .map(|element| element.text_content())
                .unwrap_or_default(),
        );
        seen_urls.insert(url.clone());
        parsed_results.push(SogouSearchResult {
            title,
            url,
            description,
            source,
            engine: "sogou",
        });
    });
    parsed_results
}
