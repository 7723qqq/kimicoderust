use std::collections::HashSet;
use std::time::Duration;

use base64::Engine as _;
use scraper::{ElementRef, Html, Selector};
use url::Url;

use super::super::WebSearchResultEntry;

const BING_BASE_URL: &str = "https://cn.bing.com/search";
const REQUEST_TIMEOUT_SECS: u64 = 30;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36";
const BOT_DETECTION_KEYWORDS: &[&str] = &[
    "captcha",
    "verification",
    "verify you are human",
    "access denied",
    "blocked",
    "rate limit",
    "too many requests",
    "请验证",
    "验证码",
    "人机验证",
];
const RESULT_SELECTORS: &[&str] = &[
    "#b_results > li.b_algo",
    "#b_results > li.b_ans",
    "#b_results > li:not(.b_ad):not(.b_pag):not(.b_msg)",
    "#b_topw > li.b_algo",
    "#b_topw > li.b_ans",
    ".b_algo",
    ".b_ans",
];

#[derive(Debug)]
struct BlockedPageAnalysis {
    blocked: bool,
    has_results: bool,
    detected_keywords: Vec<&'static str>,
    title: String,
}

pub async fn search(query: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .map_err(|error| format!("Bing search request failed: {error}"))?;
    let mut all_results = Vec::new();
    let mut page_number = 0usize;
    while all_results.len() < limit {
        let html = fetch_search_page(&client, &search_url(query, page_number)).await?;
        let page_state = analyze_blocked_page(&html);
        if page_state.blocked {
            let title = if page_state.title.is_empty() {
                "unknown"
            } else {
                &page_state.title
            };
            let keywords = if page_state.detected_keywords.is_empty() {
                "none".to_string()
            } else {
                page_state.detected_keywords.join(", ")
            };
            return Err(format!(
                "Bing returned a verification or anti-bot page (title: {title}, keywords: {keywords})"
            ));
        }
        if page_state.has_results && !page_state.detected_keywords.is_empty() {
            debug_log(&format!(
                "Bing page contains suspicious keywords but also has results, skipping block detection: {}",
                page_state.detected_keywords.join(", ")
            ));
        }
        let results = parse_results(&html, limit - all_results.len());
        if results.is_empty() {
            debug_log("No more Bing results from HTTP mode, ending early.");
            break;
        }
        all_results.extend(results);
        page_number += 1;
    }
    all_results.truncate(limit);
    Ok(all_results)
}

async fn fetch_search_page(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
        )
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .header("Cache-Control", "no-cache")
        .header("Pragma", "no-cache")
        .header("Upgrade-Insecure-Requests", "1")
        .send()
        .await
        .map_err(|error| format!("Bing search request failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "Bing search request failed: HTTP {}.",
            status.as_u16()
        ));
    }
    response
        .text()
        .await
        .map_err(|error| format!("Bing search request failed: {error}"))
}

fn search_url(query: &str, page_number: usize) -> String {
    let mut url = Url::parse(BING_BASE_URL).expect("valid Bing URL");
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("setlang", "zh-CN")
        .append_pair("ensearch", "0")
        .append_pair("first", &(1 + page_number * 10).to_string());
    url.to_string()
}

fn debug_log(message: &str) {
    if std::env::var("OPEN_WEBSEARCH_DEBUG").as_deref() == Ok("1") {
        eprintln!("{message}");
    }
}

fn analyze_blocked_page(html: &str) -> BlockedPageAnalysis {
    let normalized = html.to_lowercase();
    let document = Html::parse_document(html);
    let title_selector = Selector::parse("title").unwrap();
    let title = document
        .select(&title_selector)
        .next()
        .map(|element| element.text().collect::<String>().trim().to_lowercase())
        .unwrap_or_default();
    let detected_keywords = BOT_DETECTION_KEYWORDS
        .iter()
        .copied()
        .filter(|keyword| normalized.contains(keyword))
        .collect::<Vec<_>>();
    let result_selector =
        Selector::parse("#b_results .b_algo, #b_results li.b_algo, .b_algo, .b_ans").unwrap();
    let has_structured_results = document.select(&result_selector).next().is_some();
    let has_parsed_results = !parse_results(html, 1).is_empty();
    let has_results = has_structured_results || has_parsed_results;
    let captcha_selector = Selector::parse(
        "iframe[src*=\"captcha\"], [id*=\"captcha\"], [class*=\"captcha\"], form[action*=\"validate\"], input[name*=\"captcha\"], #b_captcha, .b_captcha",
    )
    .unwrap();
    let has_captcha_ui = document.select(&captcha_selector).next().is_some();
    let has_strong_title_signal = [
        "captcha",
        "verify you are human",
        "access denied",
        "too many requests",
        "验证码",
        "人机验证",
        "请验证",
    ]
    .iter()
    .any(|keyword| title.contains(keyword));
    let blocked =
        !has_results && (has_captcha_ui || has_strong_title_signal || detected_keywords.len() >= 2);
    BlockedPageAnalysis {
        blocked,
        has_results,
        detected_keywords,
        title,
    }
}

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

fn closest<'a>(mut element: ElementRef<'a>, selector: &Selector) -> Option<ElementRef<'a>> {
    loop {
        if selector.matches(&element) {
            return Some(element);
        }
        element = element.parent().and_then(ElementRef::wrap)?;
    }
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
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(normalized.as_bytes())
    else {
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
    let Some(raw_url) = raw_url else {
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
    let fallback_text = normalize_whitespace(
        &element
            .map(|node| node.text().collect::<String>())
            .unwrap_or_default(),
    );
    let fallback_text = js_slice(&fallback_text, 50);
    if fallback_text.is_empty() {
        format!("Result {}", index + 1)
    } else {
        fallback_text
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
            .map(|node| node.text().collect::<String>())
            .unwrap_or_default(),
    )
    .replacen(title, "", 1)
    .trim()
    .to_string();
    js_slice(&fallback_text, 400)
}

fn extract_source(element: Option<ElementRef<'_>>, url: &str) -> String {
    let source_text = normalize_whitespace(
        &first_text(element, &[".b_tpcn", ".b_attribution cite", "cite"]).unwrap_or_default(),
    );
    if !source_text.is_empty() {
        return js_slice(&source_text, 200);
    }
    if url.is_empty() {
        return String::new();
    }
    Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
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
    results: &mut Vec<WebSearchResultEntry>,
) {
    let link_selector =
        Selector::parse("#b_results a[href], #b_topw a[href], .b_algo a[href], .b_ans a[href]")
            .unwrap();
    let container_selector = Selector::parse("li, .b_algo, .b_ans").unwrap();
    for (index, link) in document.select(&link_selector).enumerate() {
        if results.len() >= limit {
            break;
        }
        let url = sanitize_bing_url(raw_link_url(&link));
        if url.is_empty() || seen_urls.contains(&url) {
            continue;
        }
        let container = closest(link, &container_selector);
        let title = extract_title(container, &url, index);
        let mut description = extract_description(container, &title);
        if description.is_empty() {
            description = Url::parse(&url)
                .ok()
                .and_then(|url| url.host_str().map(|host| format!("Result from {host}")))
                .unwrap_or_default();
        }
        seen_urls.insert(url.clone());
        let source = extract_source(container, &url);
        results.push(WebSearchResultEntry {
            title,
            url,
            snippet: description,
            site_name: (!source.is_empty()).then_some(source),
            date: None,
        });
    }
}

fn parse_results(html: &str, limit: usize) -> Vec<WebSearchResultEntry> {
    let document = Html::parse_document(html);
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
                || closest(node, &ad_selector).is_some()
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
            let source = extract_source(Some(node), &url);
            results.push(WebSearchResultEntry {
                title,
                url,
                snippet: description,
                site_name: (!source.is_empty()).then_some(source),
                date: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn search_url_uses_cn_bing_v2_parameters() {
        let url = search_url("rust lang", 2);
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.origin().ascii_serialization(), "https://cn.bing.com");
        assert_eq!(parsed.path(), "/search");
        let params = parsed.query_pairs().collect::<Vec<_>>();
        assert_eq!(
            params,
            vec![
                ("q".into(), "rust lang".into()),
                ("setlang".into(), "zh-CN".into()),
                ("ensearch".into(), "0".into()),
                ("first".into(), "21".into()),
            ]
        );
    }

    #[test]
    fn sanitize_url_decodes_redirects_and_removes_tracking_parameters() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("https://example.com/path?utm_source=x&keep=1");
        let redirect = format!("https://cn.bing.com/ck/a?u=a1{payload}");
        assert_eq!(
            sanitize_bing_url(Some(&redirect)),
            "https://example.com/path?keep=1"
        );
        assert_eq!(
            sanitize_bing_url(Some("//example.com/a?utm_medium=x&ref=y&keep=1")),
            "https://example.com/a?keep=1"
        );
        assert_eq!(sanitize_bing_url(Some("/search?q=rust")), "");
        assert_eq!(sanitize_bing_url(Some("/ck/a?u=bad")), "");
        assert_eq!(sanitize_bing_url(Some("javascript:alert(1)")), "");
    }

    #[test]
    fn parses_complete_results_with_filtering_and_deduplication() {
        let html = r#"<ol id="b_results"><li class="b_algo"><h2><a href="https://example.com/"> Rust   language </a></h2><div class="b_caption"><p> Safe   code </p></div><cite>Example</cite></li><li class="b_algo"><h2><a href="https://example.com/">duplicate</a></h2></li><li class="b_algo b_ad"><h2><a href="https://ad.example/">ad</a></h2></li><li class="b_pag"><h2><a href="https://page.example/">page</a></h2></li></ol>"#;
        let results = parse_results(html, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust language");
        assert_eq!(results[0].url, "https://example.com/");
        assert_eq!(results[0].snippet, "Safe code");
        assert_eq!(results[0].site_name.as_deref(), Some("Example"));
    }

    #[test]
    fn fallback_links_use_container_text_and_hostname() {
        let html = r#"<div id="b_results"><ul><li><a href="https://example.com/docs"> Example   docs </a><span> A   guide </span></li></ul></div>"#;
        let results = parse_results(html, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example docs");
        assert_eq!(results[0].snippet, "A guide");
        assert_eq!(results[0].site_name.as_deref(), Some("example.com"));
    }

    #[test]
    fn blocked_page_requires_no_results_and_a_strong_signal() {
        assert!(!analyze_blocked_page("<p>captcha</p>").blocked);
        assert!(analyze_blocked_page("<p>captcha and access denied</p>").blocked);
        assert!(
            analyze_blocked_page("<iframe src=\"https://example.com/captcha\"></iframe>").blocked
        );
        let with_result = r#"<p>captcha and access denied</p><ol id="b_results"><li class="b_algo"><h2><a href="https://example.com/">Rust</a></h2></li></ol>"#;
        let analysis = analyze_blocked_page(with_result);
        assert!(!analysis.blocked);
        assert!(analysis.has_results);
        assert_eq!(analysis.detected_keywords.len(), 2);
    }
}
