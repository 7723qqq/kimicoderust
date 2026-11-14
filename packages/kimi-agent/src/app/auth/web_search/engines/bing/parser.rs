use std::collections::HashSet;

use base64::Engine as _;
use scraper::{ElementRef, Html, Selector};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BingSearchResult {
    pub title: String,
    pub url: String,
    pub description: String,
    pub source: String,
    pub engine: &'static str,
}

const RESULT_SELECTORS: &[&str] = &[
    "#b_results > li.b_algo",
    "#b_results > li.b_ans",
    "#b_results > li:not(.b_ad):not(.b_pag):not(.b_msg)",
    "#b_topw > li.b_algo",
    "#b_topw > li.b_ans",
    ".b_algo",
    ".b_ans",
];

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn js_slice(value: &str, end: usize) -> String {
    let units = value.encode_utf16().take(end).collect::<Vec<_>>();
    String::from_utf16_lossy(&units)
}

fn has_class(element: &ElementRef<'_>, class_name: &str) -> bool {
    element
        .value()
        .attr("class")
        .is_some_and(|classes| classes.split_whitespace().any(|class| class == class_name))
}

fn closest_element<'a>(
    mut element: ElementRef<'a>,
    selector: &Selector,
) -> Option<ElementRef<'a>> {
    loop {
        if selector.matches(&element) {
            return Some(element);
        }
        element = element.parent().and_then(ElementRef::wrap)?;
    }
}

fn decode_bing_redirect_target(url: &Url) -> String {
    let Some(encoded_target) = url
        .query_pairs()
        .find(|(name, _)| name == "u")
        .map(|(_, value)| value.trim().to_string())
    else {
        return String::new();
    };
    if encoded_target.is_empty() {
        return String::new();
    }
    let base64_payload = encoded_target.strip_prefix("a1").unwrap_or(&encoded_target);
    let mut normalized = base64_payload.replace('-', "+").replace('_', "/");
    let remainder = normalized.len() % 4;
    if remainder != 0 {
        normalized.extend(std::iter::repeat_n('=', 4 - remainder));
    }
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(normalized.as_bytes()) else {
        return String::new();
    };
    let decoded_target = String::from_utf8_lossy(&decoded).trim().to_string();
    if decoded_target.starts_with("http://") || decoded_target.starts_with("https://") {
        decoded_target
    } else {
        String::new()
    }
}

fn remove_tracking_parameters(url: &mut Url) {
    const TRACKING_PARAMETERS: &[&str] =
        &["utm_source", "utm_medium", "utm_campaign", "ref", "source"];
    let pairs = url.query_pairs().into_owned().collect::<Vec<_>>();
    if !pairs
        .iter()
        .any(|(name, _)| TRACKING_PARAMETERS.contains(&name.as_str()))
    {
        return;
    }
    let retained = pairs
        .into_iter()
        .filter(|(name, _)| !TRACKING_PARAMETERS.contains(&name.as_str()))
        .collect::<Vec<_>>();
    url.set_query(None);
    if !retained.is_empty() {
        url.query_pairs_mut().extend_pairs(
            retained
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
    }
}

fn sanitize_bing_url(raw_url: Option<&str>) -> String {
    let Some(raw_url) = raw_url.filter(|value| !value.is_empty()) else {
        return String::new();
    };
    let mut resolved_url = raw_url.trim().to_string();
    if resolved_url.is_empty() {
        return String::new();
    }
    if resolved_url.starts_with("//") {
        resolved_url = format!("https:{resolved_url}");
    } else if resolved_url.starts_with('/') {
        if resolved_url.starts_with("/search")
            || resolved_url.starts_with("/ck/a")
            || resolved_url.starts_with("/newtabredir")
        {
            return String::new();
        }
        resolved_url = format!("https://cn.bing.com{resolved_url}");
    }
    if !resolved_url.starts_with("http://") && !resolved_url.starts_with("https://") {
        return String::new();
    }
    let Ok(mut url) = Url::parse(&resolved_url) else {
        return String::new();
    };
    let hostname = url.host_str().unwrap_or_default().to_lowercase();
    let pathname = url.path().to_lowercase();
    if hostname.ends_with("bing.com") && pathname.starts_with("/ck/a") {
        let decoded_target = decode_bing_redirect_target(&url);
        return if decoded_target.is_empty() {
            String::new()
        } else {
            sanitize_bing_url(Some(&decoded_target))
        };
    }
    if hostname.ends_with("bing.com")
        && (pathname.starts_with("/search")
            || pathname.starts_with("/ck/a")
            || pathname.starts_with("/newtabredir"))
    {
        return String::new();
    }
    remove_tracking_parameters(&mut url);
    url.to_string()
}

fn first_text(element: Option<ElementRef<'_>>, selectors: &[&str]) -> Option<String> {
    let element = element?;
    for selector in selectors {
        let selector = Selector::parse(selector).unwrap();
        if let Some(found) = element.select(&selector).next() {
            return Some(found.text().collect::<String>());
        }
    }
    None
}

fn extract_title(element: Option<ElementRef<'_>>, fallback_url: &str, index: usize) -> String {
    let candidate_title = normalize_whitespace(
        &first_text(
            element,
            &[
                "h2 a",
                ".b_tpcn .tptt",
                ".b_title a",
                "a",
                "h2, h3, .b_title, .tptt",
            ],
        )
        .unwrap_or_default(),
    );
    if !candidate_title.is_empty() {
        return js_slice(&candidate_title, 200);
    }
    if !fallback_url.is_empty() {
        if let Ok(url) = Url::parse(fallback_url) {
            return format!("Result from {}", url.host_str().unwrap_or_default());
        }
    }
    let fallback = normalize_whitespace(
        &element
            .map(|element| element.text().collect::<String>())
            .unwrap_or_default(),
    );
    let fallback = js_slice(&fallback, 50);
    if fallback.is_empty() {
        format!("Result {}", index + 1)
    } else {
        fallback
    }
}

fn extract_description(element: Option<ElementRef<'_>>, title: &str) -> String {
    let direct_snippet = normalize_whitespace(
        &first_text(
            element,
            &[
                ".b_caption p",
                ".b_caption",
                ".b_snippet, .b_lineclamp2, .b_lineclamp3",
            ],
        )
        .unwrap_or_default(),
    );
    if !direct_snippet.is_empty() {
        return js_slice(&direct_snippet, 400);
    }
    let fallback_text = normalize_whitespace(
        &element
            .map(|element| element.text().collect::<String>())
            .unwrap_or_default(),
    )
    .replacen(title, "", 1)
    .trim()
    .to_string();
    js_slice(&fallback_text, 400)
}

fn extract_source(element: Option<ElementRef<'_>>, url: &str) -> String {
    let source_text = normalize_whitespace(
        &first_text(
            element,
            &[".b_tpcn", ".b_attribution cite", "cite"],
        )
        .unwrap_or_default(),
    );
    if !source_text.is_empty() {
        return js_slice(&source_text, 200);
    }
    if url.is_empty() {
        return String::new();
    }
    Url::parse(url)
        .ok()
        .map(|url| url.host_str().unwrap_or_default().to_string())
        .unwrap_or_default()
}

fn raw_link_url<'a>(link: &ElementRef<'a>) -> Option<&'a str> {
    link.value()
        .attr("href")
        .or_else(|| link.value().attr("redirecturl"))
        .or_else(|| link.value().attr("data-h"))
}

fn collect_fallback_links(
    document: &Html,
    limit: usize,
    seen_urls: &mut HashSet<String>,
    results: &mut Vec<BingSearchResult>,
) {
    let link_selector =
        Selector::parse("#b_results a[href], #b_topw a[href], .b_algo a[href], .b_ans a[href]")
            .unwrap();
    let container_selector = Selector::parse("li, .b_algo, .b_ans").unwrap();
    for (index, link_element) in document.select(&link_selector).enumerate() {
        if results.len() >= limit {
            break;
        }
        let url = sanitize_bing_url(raw_link_url(&link_element));
        if url.is_empty() || seen_urls.contains(&url) {
            continue;
        }
        let container = closest_element(link_element, &container_selector);
        let title = extract_title(container, &url, index);
        let mut description = extract_description(container, &title);
        if description.is_empty() {
            description = Url::parse(&url)
                .ok()
                .map(|url| format!("Result from {}", url.host_str().unwrap_or_default()))
                .unwrap_or_default();
        }
        seen_urls.insert(url.clone());
        results.push(BingSearchResult {
            title,
            url: url.clone(),
            description,
            source: extract_source(container, &url),
            engine: "bing",
        });
    }
}

pub fn parse_bing_search_results(html_content: &str, limit: usize) -> Vec<BingSearchResult> {
    let document = Html::parse_document(html_content);
    let mut results = Vec::new();
    let mut seen_urls = HashSet::new();
    let ad_selector = Selector::parse(".b_ad").unwrap();
    let title_link_selector =
        Selector::parse("h2 a, .b_title a, a.tilk, a[target=\"_blank\"]").unwrap();
    for result_selector in RESULT_SELECTORS {
        let selector = Selector::parse(result_selector).unwrap();
        for (index, node) in document.select(&selector).enumerate() {
            if results.len() >= limit {
                break;
            }
            if has_class(&node, "b_ad")
                || closest_element(node, &ad_selector).is_some()
                || has_class(&node, "b_pag")
                || has_class(&node, "b_msg")
            {
                continue;
            }
            let title_link = node.select(&title_link_selector).next();
            let url = title_link
                .as_ref()
                .map_or_else(String::new, |link| sanitize_bing_url(raw_link_url(link)));
            if url.is_empty() || seen_urls.contains(&url) {
                continue;
            }
            let title = extract_title(Some(node), &url, index);
            let description = extract_description(Some(node), &title);
            if title.is_empty() && description.is_empty() {
                continue;
            }
            seen_urls.insert(url.clone());
            results.push(BingSearchResult {
                title,
                url: url.clone(),
                description,
                source: extract_source(Some(node), &url),
                engine: "bing",
            });
        }
        if results.len() >= limit {
            break;
        }
    }
    if results.is_empty() {
        collect_fallback_links(&document, limit, &mut seen_urls, &mut results);
    }
    results.truncate(limit);
    results
}
