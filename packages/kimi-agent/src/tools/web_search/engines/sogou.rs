use std::collections::HashSet;
use std::time::Duration;

use reqwest::header::{LOCATION, SET_COOKIE};
use scraper::{Html, Selector};
use url::Url;

use super::super::WebSearchResultEntry;

const SOGOU_SEARCH_URL: &str = "https://www.sogou.com/web";
const SOGOU_PAGE_SIZE: usize = 10;
const REQUEST_TIMEOUT_SECS: u64 = 20;
const MAX_REDIRECTS: usize = 5;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";

pub async fn search(query: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("Sogou search request failed: {error}"))?;
    let mut all_results = Vec::new();
    let mut seen_urls = HashSet::new();
    let max_page = 1.max(limit.div_ceil(SOGOU_PAGE_SIZE));
    for page in 1..=max_page {
        if all_results.len() >= limit {
            break;
        }
        let page_results = search_page(&client, query, page).await?;
        let page_was_empty = page_results.is_empty();
        for result in page_results {
            if seen_urls.insert(result.url.clone()) {
                all_results.push(result);
            }
        }
        if page_was_empty {
            break;
        }
    }
    all_results.truncate(limit);
    Ok(all_results)
}

async fn search_page(
    client: &reqwest::Client,
    query: &str,
    page: usize,
) -> Result<Vec<WebSearchResultEntry>, String> {
    let html = fetch_sogou_html(client, &search_url(query, page)).await?;
    if is_challenge_page(&html) {
        return Err("Sogou returned a verification or anti-bot page".to_string());
    }
    Ok(parse_results(&html))
}

async fn fetch_sogou_html(client: &reqwest::Client, initial_url: &str) -> Result<String, String> {
    let mut current_url = initial_url.to_string();
    let mut cookie_header = String::new();
    for _ in 0..=MAX_REDIRECTS {
        let mut request = client
            .get(&current_url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
            .header("Referer", "https://www.sogou.com/");
        if !cookie_header.is_empty() {
            request = request.header("Cookie", &cookie_header);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("Sogou request to {current_url} failed: {error}"))?;
        let set_cookie = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_string)
            .collect::<Vec<_>>();
        cookie_header = merge_set_cookie(&cookie_header, &set_cookie);
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    format!(
                        "Sogou returned redirect status {} without a Location header",
                        status.as_u16()
                    )
                })?;
            let base = Url::parse(&current_url)
                .map_err(|error| format!("Sogou request to {current_url} failed: {error}"))?;
            let redirect_url = base
                .join(location)
                .map_err(|error| format!("Sogou request to {current_url} failed: {error}"))?;
            if !is_allowed_redirect_url(&redirect_url) {
                return Err(format!(
                    "Sogou redirected to an unexpected host: {}",
                    redirect_url.host_str().unwrap_or_default()
                ));
            }
            current_url = redirect_url.to_string();
            continue;
        }
        if !status.is_success() {
            return Err(format!(
                "Sogou search request failed: HTTP {}.",
                status.as_u16()
            ));
        }
        return response
            .text()
            .await
            .map_err(|error| format!("Sogou request to {current_url} failed: {error}"));
    }
    Err("Sogou returned too many redirects".to_string())
}

fn search_url(query: &str, page: usize) -> String {
    let mut url = Url::parse(SOGOU_SEARCH_URL).expect("valid Sogou URL");
    url.query_pairs_mut()
        .append_pair("query", query)
        .append_pair("page", &page.to_string())
        .append_pair("ie", "utf8");
    url.to_string()
}

fn is_allowed_redirect_url(url: &Url) -> bool {
    let hostname = url.host_str().unwrap_or_default().to_lowercase();
    (url.scheme() == "https" || url.scheme() == "http")
        && (hostname == "sogou.com" || hostname.ends_with(".sogou.com"))
}

fn merge_set_cookie(cookie_header: &str, set_cookie: &[String]) -> String {
    if set_cookie.is_empty() {
        return cookie_header.to_string();
    }
    let mut cookies = Vec::<(String, String)>::new();
    for cookie in cookie_header.split(';') {
        let trimmed = cookie.trim();
        if trimmed.is_empty() {
            continue;
        }
        let name = trimmed.split('=').next().unwrap().to_string();
        upsert_cookie(&mut cookies, name, trimmed.to_string());
    }
    for value in set_cookie {
        let pair = value.split(';').next().unwrap_or_default().trim();
        if pair.is_empty() {
            continue;
        }
        let name = pair.split('=').next().unwrap().to_string();
        upsert_cookie(&mut cookies, name, pair.to_string());
    }
    cookies
        .into_iter()
        .map(|(_, pair)| pair)
        .collect::<Vec<_>>()
        .join("; ")
}

fn upsert_cookie(cookies: &mut Vec<(String, String)>, name: String, pair: String) {
    if let Some((_, current_pair)) = cookies
        .iter_mut()
        .find(|(key, _)| key.as_str() == name.as_str())
    {
        *current_pair = pair;
    } else {
        cookies.push((name, pair));
    }
}

fn normalize_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_challenge_page(html: &str) -> bool {
    let normalized = html.to_lowercase();
    let document = Html::parse_document(html);
    let title_selector = Selector::parse("title").unwrap();
    let title = document
        .select(&title_selector)
        .next()
        .map(|element| element.text().collect::<String>().trim().to_string())
        .unwrap_or_default();
    normalized.contains("antispider")
        || normalized.contains("请输入验证码")
        || normalized.contains("访问过于频繁")
        || title.contains("搜狗搜索验证")
}

fn resolve_result_url(raw_url: &str) -> String {
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
    let absolute_url = parsed.to_string();
    let target = ["url", "u", "link"].iter().find_map(|name| {
        parsed
            .query_pairs()
            .find(|(key, _)| key == *name)
            .map(|(_, value)| value.into_owned())
    });
    if let Some(target) = target {
        let normalized = target.to_ascii_lowercase();
        if normalized.starts_with("http://") || normalized.starts_with("https://") {
            return target;
        }
    }
    if parsed.scheme() == "http" || parsed.scheme() == "https" {
        absolute_url
    } else {
        String::new()
    }
}

fn extract_source(url: &str, source_text: &str) -> String {
    let cleaned_source = normalize_text(source_text);
    if !cleaned_source.is_empty() {
        return cleaned_source;
    }
    Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default()
}

fn parse_results(html: &str) -> Vec<WebSearchResultEntry> {
    let document = Html::parse_document(html);
    let result_selector = Selector::parse(
        "#main .vrwrap, #main .rb, #main .result, #results .vrwrap, .results .vrwrap, .results .rb",
    )
    .unwrap();
    let title_link_selector =
        Selector::parse("h3 a[href], h2 a[href], .vr-title a[href], .pt a[href]").unwrap();
    let description_selector = Selector::parse(".str_info, .ft, .text-layout, .fz-mid, p").unwrap();
    let source_selector = Selector::parse("cite, .citeurl, .g, .url").unwrap();
    let mut results = Vec::new();
    let mut seen_urls = HashSet::new();
    for element in document.select(&result_selector) {
        let title_link = element.select(&title_link_selector).next();
        let raw_url = title_link
            .and_then(|element| element.value().attr("href"))
            .unwrap_or_default();
        let url = resolve_result_url(raw_url);
        let title = normalize_text(
            &title_link
                .map(|element| element.text().collect::<String>())
                .unwrap_or_default(),
        );
        if title.is_empty() || url.is_empty() || seen_urls.contains(&url) {
            continue;
        }
        let description = normalize_text(
            &element
                .select(&description_selector)
                .next()
                .map(|element| element.text().collect::<String>())
                .unwrap_or_default(),
        );
        let source = extract_source(
            &url,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_uses_v2_page_parameters() {
        let url = search_url("rust lang", 3);
        let parsed = url::Url::parse(&url).unwrap();
        assert!(parsed.as_str().starts_with("https://www.sogou.com/web?"));
        let params = parsed.query_pairs().collect::<Vec<_>>();
        assert_eq!(
            params,
            vec![
                ("query".into(), "rust lang".into()),
                ("page".into(), "3".into()),
                ("ie".into(), "utf8".into()),
            ]
        );
    }

    #[test]
    fn resolves_result_urls_with_nullish_target_priority() {
        assert_eq!(
            resolve_result_url(
                "/link?url=https%3A%2F%2Fexample.com%2Fa&u=https%3A%2F%2Fother.example%2F"
            ),
            "https://example.com/a"
        );
        assert_eq!(
            resolve_result_url("/link?url=&u=https%3A%2F%2Fother.example%2F"),
            "https://www.sogou.com/link?url=&u=https%3A%2F%2Fother.example%2F"
        );
        assert_eq!(
            resolve_result_url("//example.com/path"),
            "https://example.com/path"
        );
        assert_eq!(resolve_result_url("javascript:alert(1)"), "");
        assert_eq!(resolve_result_url("  "), "");
    }

    #[test]
    fn parses_results_with_source_fallback_and_deduplication() {
        let html = r#"<div id="main"><div class="vrwrap"><h3><a href="https://example.com/"> Rust   language </a></h3><p> Safe   code </p></div><div class="rb"><h2><a href="https://example.com/">duplicate</a></h2></div><div class="result"><div class="vr-title"><a href="/link?url=https%3A%2F%2Fother.example%2F">Other</a></div><div class="ft">Other snippet</div><cite>Other source</cite></div></div>"#;
        let results = parse_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust language");
        assert_eq!(results[0].snippet, "Safe code");
        assert_eq!(results[0].site_name.as_deref(), Some("example.com"));
        assert_eq!(results[1].url, "https://other.example/");
        assert_eq!(results[1].snippet, "Other snippet");
        assert_eq!(results[1].site_name.as_deref(), Some("Other source"));
    }

    #[test]
    fn detects_all_v2_challenge_signals() {
        assert!(is_challenge_page("<script>const x='antispider';</script>"));
        assert!(is_challenge_page("<p>请输入验证码</p>"));
        assert!(is_challenge_page("<p>访问过于频繁</p>"));
        assert!(is_challenge_page("<title>搜狗搜索验证</title>"));
        assert!(!is_challenge_page("<title>Rust search</title>"));
    }

    #[test]
    fn merges_cookies_without_reordering_existing_names() {
        let values = vec!["b=3; Path=/".to_string(), "c=4; HttpOnly".to_string()];
        assert_eq!(merge_set_cookie("a=1; b=2", &values), "a=1; b=3; c=4");
        assert_eq!(merge_set_cookie(" a=1 ; b=2 ", &[]), " a=1 ; b=2 ");
    }

    #[test]
    fn redirects_stay_on_http_sogou_domains() {
        assert!(is_allowed_redirect_url(
            &url::Url::parse("https://sogou.com/next").unwrap()
        ));
        assert!(is_allowed_redirect_url(
            &url::Url::parse("http://www.sogou.com/next").unwrap()
        ));
        assert!(!is_allowed_redirect_url(
            &url::Url::parse("https://sogou.com.evil.test/").unwrap()
        ));
        assert!(!is_allowed_redirect_url(
            &url::Url::parse("https://evil.test/").unwrap()
        ));
        assert!(!is_allowed_redirect_url(
            &url::Url::parse("ftp://sogou.com/file").unwrap()
        ));
    }
}
