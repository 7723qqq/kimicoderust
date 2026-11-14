use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::err_result;
use super::moonshot_service::{self, MoonshotServiceConfig};
use crate::i18n::{LocalizedText, i18n_params};
use crate::native::web_search::{SearchResult, urlencoded};
use crate::turn_loop::types::ExecutableToolResult;

mod engines;

const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngine {
    Baidu,
    Bing,
    Brave,
    Csdn,
    Ddg,
    Exa,
    Juejin,
    LinuxDo,
    Sogou,
    Startpage,
    Zhihu,
}

impl SearchEngine {
    pub fn from_name(s: &str) -> Option<Self> {
        if s.eq_ignore_ascii_case("ddg") {
            return Some(Self::Ddg);
        }
        Self::from_registry_name(s)
    }

    fn from_registry_name(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "baidu" => Some(Self::Baidu),
            "bing" => Some(Self::Bing),
            "brave" => Some(Self::Brave),
            "csdn" => Some(Self::Csdn),
            "duckduckgo" => Some(Self::Ddg),
            "exa" => Some(Self::Exa),
            "juejin" => Some(Self::Juejin),
            "linuxdo" => Some(Self::LinuxDo),
            "sogou" => Some(Self::Sogou),
            "startpage" => Some(Self::Startpage),
            "zhihu" => Some(Self::Zhihu),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Ddg => "ddg",
            _ => self.registry_name(),
        }
    }

    pub fn registry_name(&self) -> &'static str {
        match self {
            Self::Baidu => "baidu",
            Self::Bing => "bing",
            Self::Brave => "brave",
            Self::Csdn => "csdn",
            Self::Ddg => "duckduckgo",
            Self::Exa => "exa",
            Self::Juejin => "juejin",
            Self::LinuxDo => "linuxdo",
            Self::Sogou => "sogou",
            Self::Startpage => "startpage",
            Self::Zhihu => "zhihu",
        }
    }
}

const ENGINE_REGISTRY: &[SearchEngine] = &[
    SearchEngine::Baidu,
    SearchEngine::Bing,
    SearchEngine::Brave,
    SearchEngine::Csdn,
    SearchEngine::Ddg,
    SearchEngine::Exa,
    SearchEngine::Juejin,
    SearchEngine::LinuxDo,
    SearchEngine::Sogou,
    SearchEngine::Startpage,
    SearchEngine::Zhihu,
];

static ACTIVE_ENGINE: LazyLock<Mutex<Option<SearchEngine>>> = LazyLock::new(|| Mutex::new(None));

pub fn set_active_engine(engine: SearchEngine) {
    *ACTIVE_ENGINE.lock().unwrap() = Some(engine);
}

pub fn active_engine() -> SearchEngine {
    configured_engine().unwrap_or(SearchEngine::Bing)
}

fn configured_engine() -> Option<SearchEngine> {
    *ACTIVE_ENGINE.lock().unwrap()
}

pub fn resolve_search_engines(env: &dyn Fn(&str) -> Option<String>) -> Vec<SearchEngine> {
    let preferred = env("KIMI_CODE_SEARCH_ENGINE")
        .map(|value| value.trim().to_ascii_lowercase())
        .and_then(|value| SearchEngine::from_registry_name(&value));
    let allowed: Vec<SearchEngine> = match env("KIMI_CODE_ALLOWED_SEARCH_ENGINES") {
        None => ENGINE_REGISTRY.to_vec(),
        Some(raw) if raw.trim().is_empty() => ENGINE_REGISTRY.to_vec(),
        Some(raw) => raw
            .split(',')
            .filter_map(|name| SearchEngine::from_registry_name(name.trim()))
            .collect(),
    };
    let mut ordered = vec![preferred.unwrap_or(SearchEngine::Bing)];
    ordered.extend(
        allowed
            .into_iter()
            .filter(|engine| Some(*engine) != preferred && *engine != SearchEngine::Bing),
    );
    if ordered.is_empty() {
        vec![SearchEngine::Bing]
    } else {
        ordered
    }
}

pub fn resolve_result_limit(env: &dyn Fn(&str) -> Option<String>) -> usize {
    let Some(raw) = env("KIMI_CODE_SEARCH_RESULTS") else {
        return 10;
    };
    let Some(value) = parse_js_number(&raw) else {
        return 10;
    };
    if value.is_finite() && value > 0.0 && value <= 20.0 {
        value.floor() as usize
    } else {
        10
    }
}

fn parse_js_number(raw: &str) -> Option<f64> {
    let value = raw.trim();
    if value.is_empty() {
        return Some(0.0);
    }
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16)
            .ok()
            .map(|number| number as f64);
    }
    if let Some(binary) = value
        .strip_prefix("0b")
        .or_else(|| value.strip_prefix("0B"))
    {
        return u64::from_str_radix(binary, 2)
            .ok()
            .map(|number| number as f64);
    }
    if let Some(octal) = value
        .strip_prefix("0o")
        .or_else(|| value.strip_prefix("0O"))
    {
        return u64::from_str_radix(octal, 8)
            .ok()
            .map(|number| number as f64);
    }
    value.parse::<f64>().ok()
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
    let body = json!({ "text_query": query, "search_mode": "links" }).to_string();
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

    let configured_result = match service_config() {
        Some(config) => search_via_moonshot(&config, query, tool_call_id).await,
        None => None,
    };
    if configured_result
        .as_ref()
        .is_some_and(|result| !result.is_error && !is_empty_search_result(result))
    {
        return configured_result;
    }

    let local_result = execute_local_search(query, tool_call_id).await;
    if local_result
        .as_ref()
        .is_some_and(|result| !result.is_error && !is_empty_search_result(result))
        || local_result.as_ref().is_some_and(|result| result.is_error)
    {
        return local_result;
    }
    if configured_result
        .as_ref()
        .is_some_and(|result| result.is_error)
    {
        return configured_result;
    }
    local_result
}

async fn execute_local_search(
    query: &str,
    tool_call_id: Option<&str>,
) -> Option<ExecutableToolResult> {
    let limit = resolve_result_limit(&|name| std::env::var(name).ok());
    let engines = configured_engine().map_or_else(
        || resolve_search_engines(&|name| std::env::var(name).ok()),
        |engine| vec![engine],
    );
    let mut failures = Vec::new();
    for engine in &engines {
        let result = execute_engine(*engine, query, limit).await;
        if result
            .as_ref()
            .is_some_and(|result| !result.is_error && !is_empty_search_result(result))
        {
            return result;
        }
        if let Some(result) = result {
            if result.is_error {
                failures.push(format!("{}: {}", engine.registry_name(), result.content));
            }
        }
    }

    if let Some(api_result) = search_via_bing_api(query, tool_call_id).await {
        if !api_result.is_error && !is_empty_search_result(&api_result) {
            return Some(api_result);
        }
        if api_result.is_error {
            return Some(api_result);
        }
    }
    if !engines.is_empty() && failures.len() == engines.len() {
        return Some(err_result(format!(
            "All search engines failed ({})",
            failures.join("; ")
        )));
    }
    structured_search_result(Ok(Vec::new()))
}

async fn execute_engine(
    engine: SearchEngine,
    query: &str,
    limit: usize,
) -> Option<ExecutableToolResult> {
    match engine {
        SearchEngine::Bing => structured_search_result(engines::bing::search(query, limit).await),
        SearchEngine::Ddg => {
            structured_search_result(engines::duckduckgo::search(query, limit).await)
        }
        SearchEngine::Sogou => structured_search_result(engines::sogou::search(query, limit).await),
        SearchEngine::Baidu => structured_search_result(engines::baidu::search(query, limit).await),
        SearchEngine::Brave => structured_search_result(engines::brave::search(query, limit).await),
        SearchEngine::Csdn => structured_search_result(engines::csdn::search(query, limit).await),
        SearchEngine::Exa => structured_search_result(engines::exa::search(query, limit).await),
        SearchEngine::Juejin => {
            structured_search_result(engines::juejin::search(query, limit).await)
        }
        SearchEngine::LinuxDo => {
            structured_search_result(engines::linuxdo::search(query, limit).await)
        }
        SearchEngine::Startpage => {
            structured_search_result(engines::startpage::search(query, limit).await)
        }
        SearchEngine::Zhihu => structured_search_result(engines::zhihu::search(query, limit).await),
    }
}

fn structured_search_result(
    result: Result<Vec<WebSearchResultEntry>, String>,
) -> Option<ExecutableToolResult> {
    match result {
        Err(error) => Some(err_result(error)),
        Ok(results) if results.is_empty() => Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: "No search results found.".to_string(),
            is_error: false,
            note: None,
            display: None,
        }),
        Ok(results) => Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: format_search_results(results),
            is_error: false,
            note: None,
            display: None,
        }),
    }
}

fn is_empty_search_result(result: &ExecutableToolResult) -> bool {
    !result.is_error && result.content == "No search results found."
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
        assert_eq!(body, r#"{"search_mode":"links","text_query":"rust tokio"}"#);
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
        let prev = configured_engine();
        set_active_engine(SearchEngine::Ddg);
        assert_eq!(active_engine(), SearchEngine::Ddg);
        set_active_engine(SearchEngine::Bing);
        assert_eq!(active_engine(), SearchEngine::Bing);
        *ACTIVE_ENGINE.lock().unwrap() = prev;
    }

    #[test]
    fn test_resolve_search_engines_uses_v2_order_and_filters() {
        let empty = |_name: &str| None;
        assert_eq!(
            resolve_search_engines(&empty),
            vec![
                SearchEngine::Bing,
                SearchEngine::Baidu,
                SearchEngine::Brave,
                SearchEngine::Csdn,
                SearchEngine::Ddg,
                SearchEngine::Exa,
                SearchEngine::Juejin,
                SearchEngine::LinuxDo,
                SearchEngine::Sogou,
                SearchEngine::Startpage,
                SearchEngine::Zhihu,
            ]
        );
        let custom = |name: &str| match name {
            "KIMI_CODE_SEARCH_ENGINE" => Some(" Brave ".to_string()),
            "KIMI_CODE_ALLOWED_SEARCH_ENGINES" => Some("bing,brave,exa,unknown,exa".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_search_engines(&custom),
            vec![SearchEngine::Brave, SearchEngine::Exa, SearchEngine::Exa]
        );
        let none_allowed =
            |name: &str| (name == "KIMI_CODE_ALLOWED_SEARCH_ENGINES").then(|| ",".to_string());
        assert_eq!(
            resolve_search_engines(&none_allowed),
            vec![SearchEngine::Bing]
        );
        let legacy_ddg =
            |name: &str| (name == "KIMI_CODE_SEARCH_ENGINE").then(|| "ddg".to_string());
        assert_eq!(
            resolve_search_engines(&legacy_ddg),
            resolve_search_engines(&empty)
        );
    }

    #[test]
    fn test_resolve_result_limit_matches_js_number_rules() {
        for (raw, expected) in [
            ("", 10),
            ("0", 10),
            ("0.5", 0),
            ("5.9", 5),
            ("20", 20),
            ("20.1", 10),
            ("0x10", 16),
            ("0b101", 5),
            ("0o10", 8),
            ("NaN", 10),
            ("Infinity", 10),
            ("-2", 10),
        ] {
            let env = |name: &str| (name == "KIMI_CODE_SEARCH_RESULTS").then(|| raw.to_string());
            assert_eq!(resolve_result_limit(&env), expected, "raw={raw:?}");
        }
        let missing = |_name: &str| None;
        assert_eq!(resolve_result_limit(&missing), 10);
    }
}
