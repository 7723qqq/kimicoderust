use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};
use crate::_base::utils::proxy::{is_proxy_configured, make_no_proxy_matcher, resolve_no_proxy};
use crate::os::backends::host::host_timers_service::HostTimersService;
use crate::os::interface::host_timers::{IHostTimerHandle, IHostTimers};

use super::engine_undici::{build_undici_client, undici_fetch_with_client};
use super::http3::{
    H3FetchInit, H3OriginState, h3_fetch, h3_origin_state, is_bun_runtime, mark_h3_origin,
    schedule_h3_probe,
};

pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const DEFAULT_TIMEOUT_MS: u64 = 15_000;

static SYSTEM_HOST_TIMERS: Lazy<Arc<HostTimersService>> =
    Lazy::new(|| Arc::new(HostTimersService::new()));
static DISPATCHER_CACHE: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

#[derive(Clone, Default)]
pub struct EngineRequestOptions {
    pub headers: Vec<(String, String)>,
    pub timeout_ms: Option<u64>,
    pub method: Option<String>,
    pub body: Option<String>,
    pub signal: Option<CancellationToken>,
    pub manual_redirect: bool,
    pub timers: Option<Arc<dyn IHostTimers>>,
}

pub type EngineHttpResponse = FetchResponse;

fn process_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

fn dispatcher_for(url: &Url) -> Result<Option<String>, FetchError> {
    let env = process_env();
    if !is_proxy_configured(&env) {
        return Ok(None);
    }
    let proxy_url = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
        .iter()
        .find_map(|key| env.get(*key).cloned());
    let Some(proxy_url) = proxy_url.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let no_proxy = make_no_proxy_matcher(&resolve_no_proxy(&env));
    let port = url.port().map(|port| port.to_string()).unwrap_or_else(|| {
        if url.scheme() == "https" {
            "443".to_string()
        } else {
            "80".to_string()
        }
    });
    if no_proxy(url.host_str().unwrap_or_default(), Some(&port)) {
        return Ok(None);
    }
    let mut cache = DISPATCHER_CACHE.lock();
    if cache.is_none() {
        *cache = Some(proxy_url);
    }
    Ok(cache.clone())
}

fn merge_headers(custom: &[(String, String)]) -> Vec<(String, String)> {
    let mut headers = vec![
        ("User-Agent".to_string(), DEFAULT_USER_AGENT.to_string()),
        (
            "Accept".to_string(),
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".to_string(),
        ),
        (
            "Accept-Language".to_string(),
            "zh-CN,zh;q=0.9,en;q=0.8".to_string(),
        ),
    ];
    for (name, value) in custom {
        if let Some((_, current)) = headers.iter_mut().find(|(key, _)| key == name) {
            *current = value.clone();
        } else {
            headers.push((name.clone(), value.clone()));
        }
    }
    headers
}

async fn bun_h3_engine_fetch(
    url: &str,
    options: &EngineRequestOptions,
    timeout_ms: u64,
) -> Result<EngineHttpResponse, FetchError> {
    h3_fetch(
        url,
        H3FetchInit {
            method: Some(options.method.clone().unwrap_or_else(|| "GET".to_string())),
            headers: merge_headers(&options.headers),
            body: options.body.clone(),
            signal: options.signal.clone(),
            timeout_ms: Some(timeout_ms),
            timers: options.timers.clone(),
        },
    )
    .await
}

struct EngineTimeout {
    signal: CancellationToken,
    timer: IHostTimerHandle,
    watcher: Option<tokio::task::JoinHandle<()>>,
}

fn arm_timeout(
    timers: &Arc<dyn IHostTimers>,
    timeout_ms: u64,
    external: Option<CancellationToken>,
) -> EngineTimeout {
    let signal = CancellationToken::new();
    let timer_signal = signal.clone();
    let timer = timers.set_timeout(
        Arc::new(move || timer_signal.cancel()),
        timeout_ms,
    );
    let watcher = external.and_then(|external| {
        if external.is_cancelled() {
            signal.cancel();
            return None;
        }
        let combined = signal.clone();
        Some(tokio::spawn(async move {
            external.cancelled().await;
            combined.cancel();
        }))
    });
    EngineTimeout {
        signal,
        timer,
        watcher,
    }
}

fn clear_timeout(timers: &Arc<dyn IHostTimers>, timeout: EngineTimeout) {
    timers.clear_timeout(timeout.timer);
    if let Some(watcher) = timeout.watcher {
        watcher.abort();
    }
}

pub async fn engine_fetch(
    url: &str,
    options: EngineRequestOptions,
) -> Result<EngineHttpResponse, FetchError> {
    let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let timers: Arc<dyn IHostTimers> = options
        .timers
        .clone()
        .unwrap_or_else(|| SYSTEM_HOST_TIMERS.clone());
    let parsed = Url::parse(url)?;
    let origin = parsed.origin().ascii_serialization();
    let dispatcher = dispatcher_for(&parsed)?;
    let client = build_undici_client(options.manual_redirect, dispatcher.as_deref())?;
    let h3_eligible = is_bun_runtime() && std::env::var("KIMI_CODE_SEARCH_H3").as_deref() != Ok("0");
    if h3_eligible && h3_origin_state(&origin) == H3OriginState::Ok {
        match bun_h3_engine_fetch(url, &options, timeout_ms).await {
            Ok(response) => return Ok(response),
            Err(_) => mark_h3_origin(&origin, H3OriginState::Dead),
        }
    }
    let timeout = arm_timeout(&timers, timeout_ms, options.signal.clone());
    let method = options.method.clone().unwrap_or_else(|| "GET".to_string());
    let response = match undici_fetch_with_client(
        &client,
        url,
        FetchInit {
            headers: merge_headers(&options.headers),
            method: Some(method.clone()),
            body: options.body.clone(),
            signal: Some(timeout.signal.clone()),
            manual_redirect: options.manual_redirect,
            timeout_ms: None,
        },
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            clear_timeout(&timers, timeout);
            return Err(error);
        }
    };
    if h3_eligible
        && h3_origin_state(&origin) == H3OriginState::Unknown
        && method == "GET"
        && options.body.is_none()
        && response
            .header("alt-svc")
            .unwrap_or_default()
            .contains("h3")
    {
        mark_h3_origin(&origin, H3OriginState::Ok);
    }
    if h3_eligible && h3_origin_state(&origin) == H3OriginState::Unknown && method == "GET" {
        let probe_url = url.to_string();
        let probe_headers = options.headers.clone();
        let probe_timers = options.timers.clone();
        schedule_h3_probe(&origin, async move {
            let probe = bun_h3_engine_fetch(
                &probe_url,
                &EngineRequestOptions {
                    headers: probe_headers,
                    timeout_ms: Some(3000),
                    method: Some("GET".to_string()),
                    body: None,
                    signal: None,
                    manual_redirect: false,
                    timers: probe_timers,
                },
                3000,
            )
            .await?;
            Ok(probe.ok)
        });
    }
    clear_timeout(&timers, timeout);
    Ok(response)
}

pub fn debug_log(message: &str) {
    if std::env::var("OPEN_WEBSEARCH_DEBUG").as_deref() != Ok("1") {
        return;
    }
    eprintln!("{message}");
}
