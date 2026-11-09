use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::err_result;
use super::moonshot_service::{self, MoonshotServiceConfig};
use crate::i18n::{LocalizedText, i18n_params};
use crate::native::web_search::{
    SearchResult, parse_bing_results, parse_ddg_results, parse_sogou_results, urlencoded,
};
use crate::turn_loop::types::ExecutableToolResult;

const BING_HTML_URL: &str = "https://www.bing.com/search";
const BING_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/112.0.0.0 Safari/537.36";
const DDG_HTML_URL: &str = "https://html.duckduckgo.com/html/";
const DDG_USER_AGENT: &str = BING_USER_AGENT;
const SOGOU_HTML_URL: &str = "https://www.sogou.com/web";
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_RESULTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngine {
    Bing,
    Ddg,
    Sogou,
}

impl SearchEngine {
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "bing" => Some(Self::Bing),
            "ddg" | "duckduckgo" => Some(Self::Ddg),
            "sogou" => Some(Self::Sogou),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Bing => "bing",
            Self::Ddg => "ddg",
            Self::Sogou => "sogou",
        }
    }
}

static ACTIVE_ENGINE: LazyLock<Mutex<SearchEngine>> =
    LazyLock::new(|| Mutex::new(SearchEngine::Bing));

static BING_COOKIES: LazyLock<std::sync::Arc<reqwest::cookie::Jar>> =
    LazyLock::new(|| std::sync::Arc::new(reqwest::cookie::Jar::default()));

pub fn set_active_engine(engine: SearchEngine) {
    *ACTIVE_ENGINE.lock().unwrap() = engine;
}

pub fn active_engine() -> SearchEngine {
    *ACTIVE_ENGINE.lock().unwrap()
}

#[derive(Debug, Clone)]
pub struct WebSearchResultEntry {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub site_name: Option<String>,
    pub date: Option<String>,
}

impl From<SearchResult> for WebSearchResultEntry {
    fn from(r: SearchResult) -> Self {
        Self {
            title: r.title,
            url: r.url,
            snippet: r.snippet,
            site_name: r.site_name,
            date: r.date,
        }
    }
}

pub type WebSearchServiceConfig = MoonshotServiceConfig;

static SERVICE_CONFIG: LazyLock<Mutex<Option<MoonshotServiceConfig>>> =
    LazyLock::new(|| Mutex::new(None));

pub fn set_service_config(config: Option<MoonshotServiceConfig>) {
    moonshot_service::install(&SERVICE_CONFIG, config);
}

fn service_config() -> Option<MoonshotServiceConfig> {
    moonshot_service::current(&SERVICE_CONFIG)
}

#[derive(Debug, Clone)]
pub struct BingApiConfig {
    pub base_url: String,
    pub api_key: Option<String>,
}

impl Default for BingApiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.bing.microsoft.com/v7.0/search".to_string(),
            api_key: None,
        }
    }
}

static BING_API_CONFIG: LazyLock<Mutex<Option<BingApiConfig>>> = LazyLock::new(|| Mutex::new(None));

pub fn set_bing_api_config(config: Option<BingApiConfig>) {
    let mut guard = BING_API_CONFIG.lock().unwrap();
    *guard = config;
}

fn bing_api_config() -> Option<BingApiConfig> {
    BING_API_CONFIG.lock().unwrap().clone()
}

pub fn moonshot_search_request_parts(
    config: &MoonshotServiceConfig,
    query: &str,
    api_key: &str,
    tool_call_id: Option<&str>,
) -> (String, String, Vec<(String, String)>) {
    let body = json!({ "text_query": query }).to_string();
    let headers =
        moonshot_service::bearer_headers(api_key, &config.custom_headers, &[], tool_call_id);
    (config.base_url.clone(), body, headers)
}

pub fn parse_moonshot_search_response(body: &str) -> Result<Vec<WebSearchResultEntry>, String> {
    let json: Value = serde_json::from_str(body).map_err(|e| {
        LocalizedText::with_params("engine.tools.webSearch.invalidJson", i18n_params!["e" => e])
            .render()
    })?;
    let raw = match json.get("search_results").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Ok(Vec::new()),
    };
    let mut results = Vec::with_capacity(raw.len());
    for entry in raw {
        let text = |key: &str| {
            entry
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        };
        results.push(WebSearchResultEntry {
            title: text("title").unwrap_or_default(),
            url: text("url").unwrap_or_default(),
            snippet: text("snippet").unwrap_or_default(),
            site_name: text("site_name"),
            date: text("date"),
        });
    }
    Ok(results)
}

fn format_search_results(results: Vec<WebSearchResultEntry>) -> String {
    let mut output = String::new();
    let mut first = true;
    for result in results {
        if !first {
            output.push_str("---\n\n");
        }
        first = false;
        output.push_str(
            &LocalizedText::with_params(
                "engine.tools.webSearch.resultTitle",
                i18n_params!["title" => result.title],
            )
            .render(),
        );
        if let Some(ref site) = result.site_name {
            output.push_str(
                &LocalizedText::with_params(
                    "engine.tools.webSearch.resultSite",
                    i18n_params!["site" => site],
                )
                .render(),
            );
        }
        if let Some(ref date) = result.date {
            output.push_str(
                &LocalizedText::with_params(
                    "engine.tools.webSearch.resultDate",
                    i18n_params!["date" => date],
                )
                .render(),
            );
        }
        output.push_str(
            &LocalizedText::with_params(
                "engine.tools.webSearch.resultUrl",
                i18n_params!["url" => result.url],
            )
            .render(),
        );
        output.push_str(
            &LocalizedText::with_params(
                "engine.tools.webSearch.resultSnippet",
                i18n_params!["snippet" => result.snippet],
            )
            .render(),
        );
    }
    output.push_str("When you rely on a result in your answer, cite it inline as a markdown link, e.g. [title](url).");
    output
}

async fn search_via_moonshot(
    config: &MoonshotServiceConfig,
    query: &str,
    tool_call_id: Option<&str>,
) -> Option<ExecutableToolResult> {
    let Some(api_key) = moonshot_service::non_blank_key(&config.api_key) else {
        return Some(err_result(
            LocalizedText::new("engine.tools.webSearch.missingApiKey").render(),
        ));
    };
    let (url, body, headers) = moonshot_search_request_parts(config, query, &api_key, tool_call_id);
    let client = match moonshot_service::build_client() {
        Ok(c) => c,
        Err(e) => {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.webSearch.clientInitFailed",
                    i18n_params!["e" => e],
                )
                .render(),
            ));
        }
    };
    let mut request = client.post(&url).body(body);
    request = moonshot_service::apply_headers(request, headers);
    let response = match request.send().await {
        Ok(resp) => resp,
        Err(e) => {
            let msg = if e.is_timeout() {
                LocalizedText::with_params(
                    "engine.tools.webSearch.timedOut",
                    i18n_params!["e" => e],
                )
                .render()
            } else {
                LocalizedText::with_params(
                    "engine.tools.webSearch.networkFailed",
                    i18n_params!["e" => e],
                )
                .render()
            };
            return Some(err_result(msg));
        }
    };
    let status = response.status();
    let text = match response.text().await {
        Ok(t) => t,
        Err(e) => {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.webSearch.readBodyFailed",
                    i18n_params!["e" => e],
                )
                .render(),
            ));
        }
    };
    if status.as_u16() != 200 {
        let qualifier = if status.as_u16() == 401 {
            " (auth/unauthorized)"
        } else {
            ""
        };
        return Some(err_result(
            LocalizedText::with_params(
                "engine.tools.webSearch.moonshotHttpFailed",
                i18n_params!["status" => status, "qualifier" => qualifier, "body" => text.trim()],
            )
            .render(),
        ));
    }
    let results = match parse_moonshot_search_response(&text) {
        Ok(r) => r,
        Err(e) => {
            return Some(err_result(
                LocalizedText::with_params("engine.tools.webSearch.failed", i18n_params!["e" => e])
                    .render(),
            ));
        }
    };
    if results.is_empty() {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: "No search results found.".to_string(),
            is_error: false,
            note: None,
            display: None,
        });
    }
    Some(ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: format_search_results(results),
        is_error: false,
        note: None,
        display: None,
    })
}

async fn search_via_bing_api(
    query: &str,
    tool_call_id: Option<&str>,
) -> Option<ExecutableToolResult> {
    let config = bing_api_config()?;
    let api_key = config.api_key.as_deref().filter(|k| !k.trim().is_empty())?;
    let url = format!("{}?q={}", config.base_url, urlencoded(query));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.webSearch.clientInitFailed",
                    i18n_params!["e" => e],
                )
                .render(),
            ));
        }
    };
    let mut request = client
        .get(&url)
        .header("Ocp-Apim-Subscription-Key", api_key)
        .header("Accept", "application/json");
    if let Some(id) = tool_call_id.filter(|id| !id.trim().is_empty()) {
        request = request.header("X-Msh-Tool-Call-Id", id);
    }
    let response = match request.send().await {
        Ok(resp) => resp,
        Err(e) => {
            let msg = if e.is_timeout() {
                LocalizedText::with_params(
                    "engine.tools.webSearch.timedOut",
                    i18n_params!["e" => e],
                )
                .render()
            } else {
                LocalizedText::with_params(
                    "engine.tools.webSearch.networkFailed",
                    i18n_params!["e" => e],
                )
                .render()
            };
            return Some(err_result(msg));
        }
    };
    let status = response.status();
    let text = match response.text().await {
        Ok(t) => t,
        Err(e) => {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.webSearch.readBodyFailed",
                    i18n_params!["e" => e],
                )
                .render(),
            ));
        }
    };
    if status.as_u16() != 200 {
        let qualifier = if status.as_u16() == 401 {
            " (auth/unauthorized)"
        } else {
            ""
        };
        return Some(err_result(
            LocalizedText::with_params(
                "engine.tools.webSearch.bingApiHttpFailed",
                i18n_params!["status" => status, "qualifier" => qualifier, "body" => text.trim()],
            )
            .render(),
        ));
    }
    let results = match parse_bing_api_response(&text) {
        Ok(r) => r,
        Err(e) => {
            return Some(err_result(
                LocalizedText::with_params("engine.tools.webSearch.failed", i18n_params!["e" => e])
                    .render(),
            ));
        }
    };
    if results.is_empty() {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: "No search results found.".to_string(),
            is_error: false,
            note: None,
            display: None,
        });
    }
    Some(ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: format_search_results(results),
        is_error: false,
        note: None,
        display: None,
    })
}

pub fn parse_bing_api_response(body: &str) -> Result<Vec<WebSearchResultEntry>, String> {
    let json: Value = serde_json::from_str(body).map_err(|e| {
        LocalizedText::with_params("engine.tools.webSearch.invalidJson", i18n_params!["e" => e])
            .render()
    })?;
    let raw = json
        .get("webPages")
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_array());
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut results = Vec::with_capacity(raw.len());
    for entry in raw {
        let text = |key: &str| {
            entry
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        };
        results.push(WebSearchResultEntry {
            title: text("name").unwrap_or_default(),
            url: text("url").unwrap_or_default(),
            snippet: text("snippet").unwrap_or_default(),
            site_name: text("displayUrl"),
            date: text("dateLastCrawled"),
        });
    }
    Ok(results)
}

pub async fn execute_web_search(
    args: &Value,
    tool_call_id: Option<&str>,
) -> Option<ExecutableToolResult> {
    let query = args.get("query")?.as_str()?;
    if query.trim().is_empty() {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: LocalizedText::new("engine.tools.webSearch.emptyQuery").render(),
            is_error: true,
            note: None,
            display: None,
        });
    }

    if let Some(config) = service_config() {
        return search_via_moonshot(&config, query, tool_call_id).await;
    }

    let html_result = match active_engine() {
        SearchEngine::Bing => search_via_bing_html(query).await,
        SearchEngine::Ddg => search_via_ddg_html(query).await,
        SearchEngine::Sogou => search_via_sogou_html(query).await,
    };
    if html_result.as_ref().is_some_and(|r| !r.is_error) {
        return html_result;
    }
    if let Some(api_result) = search_via_bing_api(query, tool_call_id).await {
        return Some(api_result);
    }
    html_result
}

async fn search_via_bing_html(query: &str) -> Option<ExecutableToolResult> {
    let client = match reqwest::Client::builder()
        .user_agent(BING_USER_AGENT)
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .cookie_store(true)
        .cookie_provider(BING_COOKIES.clone())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: LocalizedText::with_params(
                    "engine.tools.webSearch.clientInitFailed",
                    i18n_params!["e" => e],
                )
                .render(),
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    let url = format!("{}?q={}&setlang=en", BING_HTML_URL, urlencoded(query));
    let response = match client
        .get(&url)
        .header("Accept", "text/html")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Referer", "https://www.bing.com/")
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            let msg = if e.is_timeout() {
                LocalizedText::with_params(
                    "engine.tools.webSearch.timedOut",
                    i18n_params!["e" => e],
                )
                .render()
            } else {
                LocalizedText::with_params(
                    "engine.tools.webSearch.networkFailed",
                    i18n_params!["e" => e],
                )
                .render()
            };
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: msg,
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    let status = response.status();
    if !status.is_success() {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: LocalizedText::with_params(
                "engine.tools.webSearch.bingHttpFailed",
                i18n_params!["status" => status],
            )
            .render(),
            is_error: true,
            note: None,
            display: None,
        });
    }

    let body = match response.text().await {
        Ok(t) => t,
        Err(e) => {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: LocalizedText::with_params(
                    "engine.tools.webSearch.readBodyFailed",
                    i18n_params!["e" => e],
                )
                .render(),
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    let results = match parse_bing_results(&body, MAX_RESULTS).map(|rs| {
        rs.into_iter()
            .map(WebSearchResultEntry::from)
            .collect::<Vec<_>>()
    }) {
        Ok(res) => res,
        Err(e) => {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: LocalizedText::with_params(
                    "engine.tools.webSearch.failed",
                    i18n_params!["e" => e],
                )
                .render(),
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    if crate::native::web_search::is_hot_feed_fallback(
        query,
        &results.iter().map(|r| r.url.clone()).collect::<Vec<_>>(),
    ) {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: LocalizedText::new("engine.tools.webSearch.hotFeedFallback").render(),
            is_error: true,
            note: None,
            display: None,
        });
    }

    if results.is_empty() {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: "No search results found.".to_string(),
            is_error: false,
            note: None,
            display: None,
        });
    }

    Some(ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: format_search_results(results),
        is_error: false,
        note: None,
        display: None,
    })
}

async fn search_via_ddg_html(query: &str) -> Option<ExecutableToolResult> {
    let client = match reqwest::Client::builder()
        .user_agent(DDG_USER_AGENT)
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: LocalizedText::with_params(
                    "engine.tools.webSearch.clientInitFailed",
                    i18n_params!["e" => e],
                )
                .render(),
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    let form_body = format!("q={}", urlencoded(query));
    let response = match client
        .post(DDG_HTML_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "*/*")
        .body(form_body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            let msg = if e.is_timeout() {
                LocalizedText::with_params(
                    "engine.tools.webSearch.timedOut",
                    i18n_params!["e" => e],
                )
                .render()
            } else {
                LocalizedText::with_params(
                    "engine.tools.webSearch.networkFailed",
                    i18n_params!["e" => e],
                )
                .render()
            };
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: msg,
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    let status = response.status();
    if !status.is_success() {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: LocalizedText::with_params(
                "engine.tools.webSearch.ddgHttpFailed",
                i18n_params!["status" => status],
            )
            .render(),
            is_error: true,
            note: None,
            display: None,
        });
    }

    let body = match response.text().await {
        Ok(t) => t,
        Err(e) => {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: LocalizedText::with_params(
                    "engine.tools.webSearch.readBodyFailed",
                    i18n_params!["e" => e],
                )
                .render(),
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    let results = match parse_ddg_results(&body, MAX_RESULTS).map(|rs| {
        rs.into_iter()
            .map(WebSearchResultEntry::from)
            .collect::<Vec<_>>()
    }) {
        Ok(res) => res,
        Err(e) => {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: LocalizedText::with_params(
                    "engine.tools.webSearch.failed",
                    i18n_params!["e" => e],
                )
                .render(),
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    if results.is_empty() {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: "No search results found.".to_string(),
            is_error: false,
            note: None,
            display: None,
        });
    }

    Some(ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: format_search_results(results),
        is_error: false,
        note: None,
        display: None,
    })
}

async fn search_via_sogou_html(query: &str) -> Option<ExecutableToolResult> {
    let client = match reqwest::Client::builder()
        .user_agent(BING_USER_AGENT)
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .cookie_store(true)
        .cookie_provider(BING_COOKIES.clone())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: LocalizedText::with_params(
                    "engine.tools.webSearch.clientInitFailed",
                    i18n_params!["e" => e],
                )
                .render(),
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    let url = format!("{}?query={}", SOGOU_HTML_URL, urlencoded(query));
    let response = match client
        .get(&url)
        .header("Accept", "text/html")
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            let msg = if e.is_timeout() {
                LocalizedText::with_params(
                    "engine.tools.webSearch.timedOut",
                    i18n_params!["e" => e],
                )
                .render()
            } else {
                LocalizedText::with_params(
                    "engine.tools.webSearch.networkFailed",
                    i18n_params!["e" => e],
                )
                .render()
            };
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: msg,
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    let status = response.status();
    if !status.is_success() {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: LocalizedText::with_params(
                "engine.tools.webSearch.sogouHttpFailed",
                i18n_params!["status" => status],
            )
            .render(),
            is_error: true,
            note: None,
            display: None,
        });
    }

    let body = match response.text().await {
        Ok(t) => t,
        Err(e) => {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: LocalizedText::with_params(
                    "engine.tools.webSearch.readBodyFailed",
                    i18n_params!["e" => e],
                )
                .render(),
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    let results = match parse_sogou_results(&body, MAX_RESULTS).map(|rs| {
        rs.into_iter()
            .map(WebSearchResultEntry::from)
            .collect::<Vec<_>>()
    }) {
        Ok(res) => res,
        Err(e) => {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: LocalizedText::with_params(
                    "engine.tools.webSearch.failed",
                    i18n_params!["e" => e],
                )
                .render(),
                is_error: true,
                note: None,
                display: None,
            });
        }
    };

    if results.is_empty() {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: "No search results found.".to_string(),
            is_error: false,
            note: None,
            display: None,
        });
    }

    Some(ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: format_search_results(results),
        is_error: false,
        note: None,
        display: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_urlencoded() {
        assert_eq!(urlencoded("hello world"), "hello+world");
        assert_eq!(urlencoded("rust lang"), "rust+lang");
        assert_eq!(urlencoded("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencoded("你好"), "%E4%BD%A0%E5%A5%BD");
    }

    #[test]
    fn test_moonshot_search_request_parts() {
        let config = WebSearchServiceConfig {
            base_url: "https://api.example.test/coding/v1/search".into(),
            api_key: Some("sk-test".into()),
            custom_headers: [("X-Trace".to_string(), "t1".to_string())]
                .into_iter()
                .collect(),
        };
        let (url, body, headers) =
            moonshot_search_request_parts(&config, "rust tokio", "sk-test", Some("call-1"));
        assert_eq!(url, "https://api.example.test/coding/v1/search");
        assert_eq!(body, r#"{"text_query":"rust tokio"}"#);
        assert!(headers.contains(&("Authorization".to_string(), "Bearer sk-test".to_string())));
        assert!(headers.contains(&("Content-Type".to_string(), "application/json".to_string())));
        assert!(headers.contains(&("X-Msh-Tool-Call-Id".to_string(), "call-1".to_string())));
        assert!(headers.contains(&("X-Trace".to_string(), "t1".to_string())));

        let (_, _, headers) = moonshot_search_request_parts(&config, "rust tokio", "sk-test", None);
        assert!(!headers.iter().any(|(k, _)| k == "X-Msh-Tool-Call-Id"));
        let (_, _, headers) =
            moonshot_search_request_parts(&config, "rust tokio", "sk-test", Some(""));
        assert!(!headers.iter().any(|(k, _)| k == "X-Msh-Tool-Call-Id"));
    }

    #[test]
    fn test_parse_moonshot_search_response() {
        let body = r#"{"search_results":[
            {"title":"Rust","url":"https://rust.example.test","snippet":"lang","site_name":"rust.example.test","date":"2026-01-01"},
            {"title":"No Site"}
        ]}"#;
        let results = parse_moonshot_search_response(body).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].site_name.as_deref(), Some("rust.example.test"));
        assert_eq!(results[0].date.as_deref(), Some("2026-01-01"));
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[1].site_name, None);
        assert_eq!(results[1].date, None);
        assert_eq!(results[1].url, "");

        let rendered = format_search_results(results);
        assert!(rendered.contains("Date: 2026-01-01\n"));
    }

    #[test]
    fn test_parse_moonshot_search_response_empty_and_invalid() {
        assert!(parse_moonshot_search_response("{}").unwrap().is_empty());
        assert!(
            parse_moonshot_search_response(r#"{"search_results":[]}"#)
                .unwrap()
                .is_empty()
        );
        assert!(parse_moonshot_search_response("not json").is_err());
    }

    #[test]
    fn test_parse_bing_api_response() {
        let body = r#"{"webPages":{"value":[
            {"name":"Rust","url":"https://rust.example.test","snippet":"lang","displayUrl":"rust.example.test","dateLastCrawled":"2026-01-01"},
            {"name":"No Site"}
        ]}}"#;
        let results = parse_bing_api_response(body).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].url, "https://rust.example.test");
        assert_eq!(results[0].snippet, "lang");
        assert_eq!(results[0].site_name.as_deref(), Some("rust.example.test"));
        assert_eq!(results[0].date.as_deref(), Some("2026-01-01"));
        assert_eq!(results[1].site_name, None);
        assert_eq!(results[1].date, None);
        assert_eq!(results[1].url, "");
    }

    #[test]
    fn test_parse_bing_api_response_empty_and_invalid() {
        assert!(parse_bing_api_response("{}").unwrap().is_empty());
        assert!(
            parse_bing_api_response(r#"{"webPages":{"value":[]}}"#)
                .unwrap()
                .is_empty()
        );
        assert!(parse_bing_api_response("not json").is_err());
    }

    #[test]
    fn test_search_engine_from_name() {
        assert_eq!(SearchEngine::from_name("bing"), Some(SearchEngine::Bing));
        assert_eq!(SearchEngine::from_name("Bing"), Some(SearchEngine::Bing));
        assert_eq!(SearchEngine::from_name("ddg"), Some(SearchEngine::Ddg));
        assert_eq!(
            SearchEngine::from_name("duckduckgo"),
            Some(SearchEngine::Ddg)
        );
        assert_eq!(SearchEngine::from_name("google"), None);
        assert_eq!(SearchEngine::from_name(""), None);
    }

    #[test]
    fn test_set_and_active_engine() {
        let prev = active_engine();
        set_active_engine(SearchEngine::Ddg);
        assert_eq!(active_engine(), SearchEngine::Ddg);
        set_active_engine(SearchEngine::Bing);
        assert_eq!(active_engine(), SearchEngine::Bing);
        set_active_engine(prev);
    }
}
