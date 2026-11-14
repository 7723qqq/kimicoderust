use std::collections::HashSet;

use indexmap::IndexMap;
use regex::Regex;
use scraper::{ElementRef, Selector};
use serde_json::Value;
use url::Url;

use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::Error2;

use crate::_base::utils::fetch_like::FetchError;

use super::super::engine_html::{load_html, query_html};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartpageSearchResult {
    pub title: String,
    pub url: String,
    pub description: String,
    pub source: String,
    pub engine: &'static str,
}

const CAPTCHA_UI_SELECTORS: &str =
    "form[action*=\"/sp/captcha\"],iframe[src*=\"captcha\"],[id*=\"captcha\"],[class*=\"captcha\"]";
const VERIFICATION_KEYWORDS: &[&str] = &[
    "verify you are human",
    "human verification",
    "security check",
];

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn is_captcha_page(html: &str) -> bool {
    let normalized = html.to_lowercase();
    let document = load_html(html);
    let title = query_html(&document, "title")
        .first()
        .text()
        .trim()
        .to_lowercase();
    if normalized.contains("/sp/captcha") {
        return true;
    }
    let has_captcha_ui = !query_html(&document, CAPTCHA_UI_SELECTORS).is_empty();
    let has_verification_text = VERIFICATION_KEYWORDS
        .iter()
        .any(|keyword| normalized.contains(keyword) || title.contains(keyword));
    has_captcha_ui || has_verification_text
}

pub fn extract_sc_code(html: &str) -> Option<String> {
    let document = load_html(html);
    query_html(
        &document,
        "form[action=\"/sp/search\"] input[name=\"sc\"]",
    )
    .first()
    .attr("value")
    .map(|value| value.trim().to_string())
}

pub fn extract_interstitial_payload(html: &str) -> Option<Vec<(String, String)>> {
    let captures = Regex::new(r"var data = (\{[\s\S]*?\});")
        .unwrap()
        .captures(html)?;
    let payload_raw = captures.get(1)?.as_str();
    let payload: IndexMap<String, Value> = serde_json::from_str(payload_raw).ok()?;
    if !payload.get("query").is_some_and(Value::is_string)
        || !payload.get("sgt").is_some_and(Value::is_string)
    {
        return None;
    }
    let data = payload
        .iter()
        .filter_map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_string()))
        })
        .collect::<Vec<_>>();
    if data.is_empty() { None } else { Some(data) }
}

fn first_matching_next_sibling<'a>(
    element: ElementRef<'a>,
    selector: &Selector,
) -> Option<ElementRef<'a>> {
    let mut sibling = element.next_sibling();
    while let Some(node) = sibling {
        let next = node.next_sibling();
        if let Some(element) = ElementRef::wrap(node)
            && selector.matches(&element)
        {
            return Some(element);
        }
        sibling = next;
    }
    None
}

fn extract_source(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn extract_results_from_html(
    html: &str,
) -> Result<Vec<StartpageSearchResult>, FetchError> {
    if is_captcha_page(html) {
        return Err(Box::new(Error2::new(
            WEB_FETCH_FAILED,
            "Startpage returned a verification or anti-bot page",
        )));
    }
    let document = load_html(html);
    let result_selector = Selector::parse("a.result-title.result-link[href]").unwrap();
    let title_selector = Selector::parse("h2").unwrap();
    let description_selector = Selector::parse("p.description").unwrap();
    let mut results = Vec::new();
    let mut seen_urls = HashSet::new();
    for element in document.select(&result_selector) {
        let url = element
            .value()
            .attr("href")
            .map(str::trim)
            .map(str::to_string);
        let title = normalize_whitespace(
            &element
                .select(&title_selector)
                .next()
                .map(|element| element.text().collect::<String>())
                .unwrap_or_default(),
        );
        let description = normalize_whitespace(
            &first_matching_next_sibling(element, &description_selector)
                .map(|element| element.text().collect::<String>())
                .unwrap_or_default(),
        );
        let Some(url) = url else {
            continue;
        };
        if url.is_empty() || title.is_empty() || seen_urls.contains(&url) {
            continue;
        }
        seen_urls.insert(url.clone());
        let source = extract_source(&url);
        results.push(StartpageSearchResult {
            title,
            url,
            description,
            source,
            engine: "startpage",
        });
    }
    Ok(results)
}
