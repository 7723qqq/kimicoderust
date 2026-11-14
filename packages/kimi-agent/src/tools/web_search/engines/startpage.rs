use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use scraper::{ElementRef, Html, Selector};
use serde_json::Value;

use super::super::WebSearchResultEntry;

const STARTPAGE_BASE_URL: &str = "https://www.startpage.com";
const STARTPAGE_SEARCH_URL: &str = "https://www.startpage.com/sp/search";
const STARTPAGE_SC_TTL: Duration = Duration::from_secs(30 * 60);
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";
const ACCEPT_HEADER: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

static SC_CACHE: LazyLock<Mutex<Option<(String, Instant)>>> = LazyLock::new(|| Mutex::new(None));

pub async fn search(query: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    let mut all_results = Vec::new();
    let mut seen_urls = HashSet::new();
    let max_page = 1.max(limit.div_ceil(10));
    let mut page = 1;
    while page <= max_page && all_results.len() < limit {
        let page_results = search_page(query, page).await?;
        let empty = page_results.is_empty();
        for result in page_results {
            if seen_urls.insert(result.url.clone()) {
                all_results.push(result);
            }
        }
        if empty {
            break;
        }
        page += 1;
    }
    all_results.truncate(limit);
    Ok(all_results)
}

async fn search_page(query: &str, page: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    let sc_code = get_sc_code().await?;
    let mut form = vec![
        ("query", query),
        ("cat", "web"),
        ("t", "device"),
        ("sc", sc_code.as_str()),
        ("abp", "1"),
        ("abd", "1"),
        ("abe", "1"),
    ];
    let page_text;
    let segment;
    if page > 1 {
        page_text = page.to_string();
        segment = "startpage.udog";
        form.push(("page", page_text.as_str()));
        form.push(("segment", segment));
    }
    let body = form_body(&form);
    let headers = vec![
        ("Accept", ACCEPT_HEADER),
        ("Content-Type", "application/x-www-form-urlencoded"),
        ("Origin", STARTPAGE_BASE_URL),
        ("Referer", "https://www.startpage.com/"),
    ];
    let mut html = fetch_text(STARTPAGE_SEARCH_URL, true, &headers, Some(body), 20).await?;
    if let Some(payload) = extract_interstitial_payload(&html) {
        let follow_headers = vec![
            ("Accept", ACCEPT_HEADER),
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Origin", STARTPAGE_BASE_URL),
            ("Referer", STARTPAGE_SEARCH_URL),
        ];
        let pairs: Vec<(String, String)> = payload.into_iter().collect();
        let body = form_body(&pairs);
        html = fetch_text(STARTPAGE_SEARCH_URL, true, &follow_headers, Some(body), 20).await?;
    }
    extract_results(&html)
}

async fn get_sc_code() -> Result<String, String> {
    if let Some((code, at)) = SC_CACHE.lock().unwrap().as_ref() {
        if at.elapsed() < STARTPAGE_SC_TTL {
            return Ok(code.clone());
        }
    }
    let html = fetch_text(
        "https://www.startpage.com/",
        false,
        &[("Accept", ACCEPT_HEADER)],
        None,
        15,
    )
    .await?;
    if is_captcha_page(&html) {
        return Err(
            "Startpage returned a verification or anti-bot page while requesting the search token"
                .to_string(),
        );
    }
    let code = extract_sc_code(&html)
        .ok_or_else(|| "Failed to extract Startpage search token".to_string())?;
    *SC_CACHE.lock().unwrap() = Some((code.clone(), Instant::now()));
    Ok(code)
}

async fn fetch_text(
    url: &str,
    post: bool,
    headers: &[(&str, &str)],
    body: Option<String>,
    timeout_secs: u64,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|error| format!("Startpage request to {url} failed: {error}"))?;
    let mut request = if post {
        client.post(url)
    } else {
        client.get(url)
    };
    request = request.header("Accept-Language", "en-US,en;q=0.9");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("Startpage request to {url} failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "Startpage request failed: HTTP {}.",
            status.as_u16()
        ));
    }
    response
        .text()
        .await
        .map_err(|error| format!("Startpage request to {url} failed: {error}"))
}

fn form_body<S: AsRef<str>>(pairs: &[(S, S)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            pairs
                .iter()
                .map(|(key, value)| (key.as_ref(), value.as_ref())),
        )
        .finish()
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_captcha_page(html: &str) -> bool {
    let normalized = html.to_lowercase();
    if normalized.contains("/sp/captcha") {
        return true;
    }
    let document = Html::parse_document(html);
    let captcha_selector = Selector::parse(
        "form[action*=\"/sp/captcha\"],iframe[src*=\"captcha\"],[id*=\"captcha\"],[class*=\"captcha\"]",
    )
    .unwrap();
    if document.select(&captcha_selector).next().is_some() {
        return true;
    }
    let title = document
        .select(&Selector::parse("title").unwrap())
        .next()
        .map(|element| element.text().collect::<String>().trim().to_lowercase())
        .unwrap_or_default();
    [
        "verify you are human",
        "human verification",
        "security check",
    ]
    .iter()
    .any(|keyword| normalized.contains(keyword) || title.contains(keyword))
}

fn extract_sc_code(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("form[action=\"/sp/search\"] input[name=\"sc\"]").unwrap();
    document
        .select(&selector)
        .next()
        .and_then(|element| element.value().attr("value"))
        .map(|value| value.trim().to_string())
}

fn extract_interstitial_payload(html: &str) -> Option<Vec<(String, String)>> {
    let regex = regex::Regex::new(r"(?s)var data = (\{.*?\});").ok()?;
    let raw = regex.captures(html)?.get(1)?.as_str();
    let value: Value = serde_json::from_str(raw).ok()?;
    let object = value.as_object()?;
    if !object.get("query")?.is_string() || !object.get("sgt")?.is_string() {
        return None;
    }
    let pairs: Vec<(String, String)> = object
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
        .collect();
    (!pairs.is_empty()).then_some(pairs)
}

fn extract_results(html: &str) -> Result<Vec<WebSearchResultEntry>, String> {
    if is_captcha_page(html) {
        return Err("Startpage returned a verification or anti-bot page".to_string());
    }
    let document = Html::parse_document(html);
    let link_selector = Selector::parse("a.result-title.result-link[href]").unwrap();
    let title_selector = Selector::parse("h2").unwrap();
    let mut results = Vec::new();
    let mut seen_urls = HashSet::new();
    for element in document.select(&link_selector) {
        let Some(url) = element.value().attr("href").map(str::trim) else {
            continue;
        };
        let title = element
            .select(&title_selector)
            .next()
            .map(|node| normalize_whitespace(&node.text().collect::<String>()))
            .unwrap_or_default();
        if url.is_empty() || title.is_empty() || !seen_urls.insert(url.to_string()) {
            continue;
        }
        let description = first_description(element)
            .map(|node| normalize_whitespace(&node.text().collect::<String>()))
            .unwrap_or_default();
        let source = url::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_default();
        results.push(WebSearchResultEntry {
            title,
            url: url.to_string(),
            snippet: description,
            site_name: (!source.is_empty()).then_some(source),
            date: None,
        });
    }
    Ok(results)
}

fn first_description(element: ElementRef<'_>) -> Option<ElementRef<'_>> {
    for sibling in element.next_siblings().filter_map(ElementRef::wrap) {
        if sibling.value().name() == "p"
            && sibling
                .value()
                .attr("class")
                .is_some_and(|class| class.split_whitespace().any(|name| name == "description"))
        {
            return Some(sibling);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_token_and_interstitial_payload() {
        assert_eq!(
            extract_sc_code(
                r#"<form action="/sp/search"><input name="sc" value=" token "></form>"#
            ),
            Some("token".to_string())
        );
        let payload = extract_interstitial_payload(
            r#"<script>var data = {"query":"rust","sgt":"abc","other":1,"next":"yes"};</script>"#,
        )
        .unwrap();
        assert!(payload.contains(&("query".to_string(), "rust".to_string())));
        assert!(payload.contains(&("sgt".to_string(), "abc".to_string())));
        assert!(payload.contains(&("next".to_string(), "yes".to_string())));
        assert_eq!(payload.len(), 3);
    }

    #[test]
    fn detects_captcha_and_missing_tokens() {
        assert!(is_captcha_page("<title>Security check</title>"));
        assert!(is_captcha_page(
            "<iframe src=\"https://example.com/captcha\"></iframe>"
        ));
        assert!(!is_captcha_page("<title>Rust search</title>"));
        assert_eq!(extract_sc_code("<form></form>"), None);
    }

    #[test]
    fn parses_results_with_sibling_descriptions_and_dedupes_urls() {
        let html = r#"<a class="result-title result-link" href="https://example.com"><h2> Rust   language </h2></a><p class="description"> Safe   code </p><a class="result-title result-link" href="https://example.com"><h2>duplicate</h2></a>"#;
        let results = extract_results(html).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust language");
        assert_eq!(results[0].snippet, "Safe code");
        assert_eq!(results[0].site_name.as_deref(), Some("example.com"));
    }

    #[test]
    fn form_encoding_uses_url_search_params_shape() {
        assert_eq!(
            form_body(&[("query", "rust lang"), ("cat", "web")]),
            "query=rust+lang&cat=web"
        );
    }
}
