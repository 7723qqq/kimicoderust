use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;

use crate::_base::utils::abort::AbortError;
use crate::_base::utils::fetch_like::{FetchError, FetchLike};
use crate::app::auth::web_search::engines::baidu::search_baidu;
use crate::app::auth::web_search::engines::bing::search_bing;
use crate::app::auth::web_search::engines::brave::search_brave;
use crate::app::auth::web_search::engines::csdn::search_csdn;
use crate::app::auth::web_search::engines::duckduckgo::search_duck_duck_go;
use crate::app::auth::web_search::engines::exa::search_exa;
use crate::app::auth::web_search::engines::juejin::search_juejin;
use crate::app::auth::web_search::engines::linuxdo::search_linux_do;
use crate::app::auth::web_search::engines::sogou::search_sogou;
use crate::app::auth::web_search::engines::startpage::search_startpage;
use crate::app::auth::web_search::engines::types::{SearchEngineFn, SearchEngineOptions};
use crate::app::auth::web_search::engines::zhihu::search_zhihu;
use crate::app::auth::web_search::web_search::{WebSearchOptions, WebSearchProvider, WebSearchResult};
use crate::mcp_core::client_shared::ReqwestFetch;

pub type LocalSearchEngine = String;

static ENGINE_REGISTRY: LazyLock<Vec<(&'static str, SearchEngineFn)>> = LazyLock::new(|| {
    vec![
        ("baidu", Arc::new(search_baidu) as SearchEngineFn),
        ("bing", Arc::new(search_bing) as SearchEngineFn),
        ("brave", Arc::new(search_brave) as SearchEngineFn),
        ("csdn", Arc::new(search_csdn) as SearchEngineFn),
        ("duckduckgo", Arc::new(search_duck_duck_go) as SearchEngineFn),
        ("exa", Arc::new(search_exa) as SearchEngineFn),
        ("juejin", Arc::new(search_juejin) as SearchEngineFn),
        ("linuxdo", Arc::new(search_linux_do) as SearchEngineFn),
        ("sogou", Arc::new(search_sogou) as SearchEngineFn),
        ("startpage", Arc::new(search_startpage) as SearchEngineFn),
        ("zhihu", Arc::new(search_zhihu) as SearchEngineFn),
    ]
});

fn has_engine(name: &str) -> bool {
    ENGINE_REGISTRY.iter().any(|(candidate, _)| *candidate == name)
}

fn engine_executor(name: &str) -> Option<SearchEngineFn> {
    ENGINE_REGISTRY
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, executor)| executor.clone())
}

pub fn resolve_search_engines(env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let preferred = env("KIMI_CODE_SEARCH_ENGINE").map(|value| value.trim().to_lowercase());
    let allowed = match env("KIMI_CODE_ALLOWED_SEARCH_ENGINES") {
        None => ENGINE_REGISTRY
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect::<Vec<_>>(),
        Some(raw) if raw.trim().is_empty() => ENGINE_REGISTRY
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect::<Vec<_>>(),
        Some(raw) => raw
            .split(',')
            .map(|name| name.trim().to_lowercase())
            .filter(|name| has_engine(name))
            .collect::<Vec<_>>(),
    };
    let mut ordered = vec![preferred
        .as_deref()
        .filter(|name| has_engine(name))
        .unwrap_or("bing")
        .to_string()];
    ordered.extend(
        allowed
            .into_iter()
            .filter(|name| Some(name.as_str()) != preferred.as_deref() && name != "bing"),
    );
    if ordered.is_empty() {
        vec!["bing".to_string()]
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

#[derive(Default)]
pub struct LocalWebSearchProviderOptions {
    pub fetch_impl: Option<Arc<dyn FetchLike>>,
    pub engine: Option<String>,
    pub env: Option<HashMap<String, String>>,
}

pub struct LocalWebSearchProvider {
    fetch_impl: Arc<dyn FetchLike>,
    engines: Vec<String>,
    limit: usize,
}

impl LocalWebSearchProvider {
    pub fn new(options: LocalWebSearchProviderOptions) -> Self {
        let LocalWebSearchProviderOptions {
            fetch_impl,
            engine,
            env,
        } = options;
        let fetch_impl = fetch_impl.unwrap_or_else(|| Arc::new(ReqwestFetch::new()));
        let env = env.unwrap_or_else(|| std::env::vars().collect());
        let get_env = |name: &str| env.get(name).cloned();
        let engines = match engine {
            Some(engine) => vec![engine],
            None => resolve_search_engines(&get_env),
        };
        let limit = resolve_result_limit(&get_env);
        Self {
            fetch_impl,
            engines,
            limit,
        }
    }
}

#[async_trait]
impl WebSearchProvider for LocalWebSearchProvider {
    async fn search(
        &self,
        query: &str,
        options: WebSearchOptions,
    ) -> Result<Vec<WebSearchResult>, FetchError> {
        let signal = options.signal;
        let mut failures = Vec::new();
        for engine in &self.engines {
            let Some(executor) = engine_executor(engine) else {
                continue;
            };
            let result = executor(
                query.to_string(),
                self.limit,
                SearchEngineOptions {
                    signal: signal.clone(),
                    fetch_impl: Some(self.fetch_impl.clone()),
                },
            )
            .await;
            match result {
                Ok(results) if !results.is_empty() => return Ok(results),
                Ok(_) => {}
                Err(error) => {
                    if signal.as_ref().is_some_and(|signal| signal.is_cancelled())
                        || error.is::<AbortError>()
                    {
                        return Err(error);
                    }
                    failures.push(format!("{engine}: {error}"));
                }
            }
        }
        if failures.len() == self.engines.len() {
            return Err(Box::new(Error2::new(
                WEB_FETCH_FAILED,
                format!("All search engines failed ({})", failures.join("; ")),
            )));
        }
        Ok(Vec::new())
    }
}
