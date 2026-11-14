use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use indexmap::IndexSet;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};
use crate::_base::utils::proxy::is_proxy_configured;
use crate::os::backends::host::host_timers_service::HostTimersService;
use crate::os::interface::host_timers::{IHostTimerHandle, IHostTimers};

use super::engine_undici::undici_fetch;

const PROXY_ENV_KEYS: [&str; 2] = ["NO_PROXY", "no_proxy"];

static SYSTEM_HOST_TIMERS: Lazy<Arc<HostTimersService>> =
    Lazy::new(|| Arc::new(HostTimersService::new()));

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum H3OriginState {
    #[default]
    Unknown,
    Ok,
    Dead,
}

#[derive(Clone, Default)]
struct CapturedNoProxy {
    upper: Option<String>,
    lower: Option<String>,
}

#[derive(Default)]
struct H3State {
    origins: HashMap<String, H3OriginState>,
    probing: IndexSet<String>,
    direct_hosts: IndexSet<String>,
    captured_no_proxy: Option<CapturedNoProxy>,
}

static H3_STATE: Lazy<Mutex<H3State>> = Lazy::new(|| Mutex::new(H3State::default()));

#[derive(Clone, Default)]
pub struct H3FetchInit {
    pub method: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub signal: Option<CancellationToken>,
    pub timeout_ms: Option<u64>,
    pub timers: Option<Arc<dyn IHostTimers>>,
}

fn render_no_proxy(state: &H3State) -> String {
    let mut parts = IndexSet::new();
    if let Some(captured) = &state.captured_no_proxy {
        for raw in [&captured.upper, &captured.lower] {
            for piece in raw.as_deref().unwrap_or_default().split(',') {
                let trimmed = piece.trim();
                if !trimmed.is_empty() {
                    parts.insert(trimmed.to_string());
                }
            }
        }
    }
    for host in &state.direct_hosts {
        parts.insert(host.clone());
    }
    parts.into_iter().collect::<Vec<_>>().join(",")
}

fn set_no_proxy(value: &str) {
    for key in PROXY_ENV_KEYS {
        unsafe {
            std::env::set_var(key, value);
        }
    }
}

fn push_direct_host(host: &str) {
    let mut state = H3_STATE.lock();
    if state.captured_no_proxy.is_none() {
        state.captured_no_proxy = Some(CapturedNoProxy {
            upper: std::env::var("NO_PROXY").ok(),
            lower: std::env::var("no_proxy").ok(),
        });
    }
    state.direct_hosts.insert(host.to_string());
    let value = render_no_proxy(&state);
    set_no_proxy(&value);
}

fn pop_direct_host(host: &str) {
    let mut state = H3_STATE.lock();
    let Some(captured) = state.captured_no_proxy.clone() else {
        return;
    };
    state.direct_hosts.shift_remove(host);
    if !state.direct_hosts.is_empty() {
        let value = render_no_proxy(&state);
        set_no_proxy(&value);
        return;
    }
    for (key, original) in [("NO_PROXY", captured.upper), ("no_proxy", captured.lower)] {
        if let Some(original) = original {
            unsafe {
                std::env::set_var(key, original);
            }
        } else {
            unsafe {
                std::env::remove_var(key);
            }
        }
    }
}

pub fn h3_origin_state(origin: &str) -> H3OriginState {
    H3_STATE
        .lock()
        .origins
        .get(origin)
        .copied()
        .unwrap_or(H3OriginState::Unknown)
}

pub fn mark_h3_origin(origin: &str, state: H3OriginState) {
    H3_STATE
        .lock()
        .origins
        .insert(origin.to_string(), state);
}

pub fn reset_h3_states() {
    let mut state = H3_STATE.lock();
    state.origins.clear();
    state.probing.clear();
    state.direct_hosts.clear();
    state.captured_no_proxy = None;
}

pub fn is_bun_runtime() -> bool {
    false
}

pub fn h3_disabled(env: &HashMap<String, String>) -> bool {
    env.get("KIMI_CODE_SEARCH_H3").map(String::as_str) == Some("0")
}

pub fn schedule_h3_probe<F>(origin: &str, attempt: F)
where
    F: Future<Output = Result<bool, FetchError>> + Send + 'static,
{
    if !is_bun_runtime() {
        return;
    }
    if std::env::var("KIMI_CODE_SEARCH_H3").as_deref() == Ok("0") {
        return;
    }
    {
        let mut state = H3_STATE.lock();
        if state
            .origins
            .get(origin)
            .copied()
            .unwrap_or(H3OriginState::Unknown)
            != H3OriginState::Unknown
        {
            return;
        }
        if state.probing.contains(origin) {
            return;
        }
        state.probing.insert(origin.to_string());
    }
    let origin = origin.to_string();
    tokio::spawn(async move {
        let state = match attempt.await {
            Ok(true) => H3OriginState::Ok,
            Ok(false) | Err(_) => H3OriginState::Dead,
        };
        mark_h3_origin(&origin, state);
        H3_STATE.lock().probing.shift_remove(&origin);
    });
}

struct H3Timeout {
    signal: CancellationToken,
    timer: IHostTimerHandle,
    watcher: Option<tokio::task::JoinHandle<()>>,
}

fn arm_timeout(
    timers: &Arc<dyn IHostTimers>,
    timeout_ms: u64,
    external: Option<CancellationToken>,
) -> H3Timeout {
    let signal = CancellationToken::new();
    let timer_signal = signal.clone();
    let timer = timers.set_timeout(Arc::new(move || timer_signal.cancel()), timeout_ms);
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
    H3Timeout {
        signal,
        timer,
        watcher,
    }
}

fn clear_timeout(timers: &Arc<dyn IHostTimers>, timeout: H3Timeout) {
    timers.clear_timeout(timeout.timer);
    if let Some(watcher) = timeout.watcher {
        watcher.abort();
    }
}

pub async fn h3_fetch(url: &str, init: H3FetchInit) -> Result<FetchResponse, FetchError> {
    let timers: Arc<dyn IHostTimers> = init
        .timers
        .clone()
        .unwrap_or_else(|| SYSTEM_HOST_TIMERS.clone());
    let timeout = arm_timeout(
        &timers,
        init.timeout_ms.unwrap_or(0),
        init.signal.clone(),
    );
    let env = std::env::vars().collect::<HashMap<_, _>>();
    let routed_directly = is_proxy_configured(&env);
    let host = if routed_directly {
        let host = Url::parse(url)?
            .host_str()
            .unwrap_or_default()
            .to_lowercase();
        push_direct_host(&host);
        Some(host)
    } else {
        None
    };
    let result = undici_fetch(
        url,
        FetchInit {
            headers: init.headers,
            method: Some(init.method.unwrap_or_else(|| "GET".to_string())),
            body: init.body,
            signal: Some(timeout.signal.clone()),
            manual_redirect: false,
            timeout_ms: None,
        },
    )
    .await;
    clear_timeout(&timers, timeout);
    if let Some(host) = host {
        pop_direct_host(&host);
    }
    result
}
