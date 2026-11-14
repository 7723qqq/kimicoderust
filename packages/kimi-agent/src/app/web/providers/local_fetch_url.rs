use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::_base::utils::abort::AbortError;
use crate::_base::utils::fetch_like::{
    FetchByteStream, FetchError, FetchInit, FetchLike, FetchResponse,
};
use crate::app::auth::web_search::engines::engine_undici::undici_fetch;
use crate::os::backends::host::host_timers_service::HostTimersService;
use crate::os::interface::host_timers::{IHostTimerHandle, IHostTimers};

use super::super::errors::{
    WEB_FETCH_FAILED, WEB_INVALID_URL, register_web_errors,
};
use super::super::tools::fetch_url_types::{
    HttpFetchError, UrlFetchKind, UrlFetchOptions, UrlFetchResult, UrlFetcher,
};

const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";
const DEFAULT_MAX_BYTES: usize = 10 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_REDIRECT_HOPS: usize = 10;
const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

static SYSTEM_HOST_TIMERS: LazyLock<Arc<HostTimersService>> =
    LazyLock::new(|| Arc::new(HostTimersService::new()));
static SYSTEM_FETCH: LazyLock<Arc<dyn FetchLike>> = LazyLock::new(|| Arc::new(SystemFetch));

struct SystemFetch;

#[async_trait]
impl FetchLike for SystemFetch {
    async fn fetch(&self, url: &str, init: FetchInit) -> Result<FetchResponse, FetchError> {
        undici_fetch(url, init).await
    }
}

pub(crate) fn system_fetch() -> Arc<dyn FetchLike> {
    SYSTEM_FETCH.clone()
}

#[derive(Default)]
pub struct LocalFetchURLProviderOptions {
    pub user_agent: Option<String>,
    pub fetch_impl: Option<Arc<dyn FetchLike>>,
    pub max_bytes: Option<usize>,
    pub timeout_ms: Option<u64>,
    pub timers: Option<Arc<dyn IHostTimers>>,
}

pub struct LocalFetchURLProvider {
    user_agent: String,
    fetch_impl: Arc<dyn FetchLike>,
    max_bytes: usize,
    timeout_ms: u64,
    timers: Arc<dyn IHostTimers>,
}

impl LocalFetchURLProvider {
    pub fn new(options: LocalFetchURLProviderOptions) -> Self {
        register_web_errors();
        Self {
            user_agent: options
                .user_agent
                .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string()),
            fetch_impl: options.fetch_impl.unwrap_or_else(system_fetch),
            max_bytes: options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
            timeout_ms: options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            timers: options
                .timers
                .unwrap_or_else(|| SYSTEM_HOST_TIMERS.clone() as Arc<dyn IHostTimers>),
        }
    }

    async fn read_response(
        &self,
        response: FetchResponse,
        signal: Option<&CancellationToken>,
    ) -> Result<UrlFetchResult, FetchError> {
        if response.status >= 400 {
            return Err(Box::new(HttpFetchError::new(
                response.status,
                format!("HTTP {} {}", response.status, response.status_text),
            )));
        }

        if let Some(content_length) = response.header("content-length")
            && let Ok(length) = content_length.trim().parse::<f64>()
            && length.is_finite()
            && length > self.max_bytes as f64
        {
            return Err(Box::new(too_large_error(length, self.max_bytes)));
        }

        let content_type = response
            .header("content-type")
            .unwrap_or_default()
            .to_lowercase();
        let body = read_body_with_cap(response.stream(), self.max_bytes, signal).await?;
        let body = strip_bom(&body);

        if content_type.starts_with("text/plain") || content_type.starts_with("text/markdown") {
            return Ok(UrlFetchResult {
                content: body.to_string(),
                kind: UrlFetchKind::Passthrough,
            });
        }

        Ok(UrlFetchResult {
            content: self.extract_main_content(body)?,
            kind: UrlFetchKind::Extracted,
        })
    }

    async fn request_with_redirects(
        &self,
        url: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<FetchResponse, FetchError> {
        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms);
        let mut current_url = url.to_string();
        let mut redirects = 0usize;
        loop {
            validate_fetch_url(&current_url)?;
            let response = self
                .fetch_with_deadline(&current_url, signal, deadline)
                .await?;
            if !REDIRECT_STATUSES.contains(&response.status) {
                return Ok(response);
            }
            let Some(location) = response.header("location").map(str::to_string) else {
                return Ok(response);
            };
            drop(response);
            if redirects >= MAX_REDIRECT_HOPS {
                let details = Map::from_iter([
                    ("url".to_string(), Value::String(url.to_string())),
                    ("limit".to_string(), Value::from(MAX_REDIRECT_HOPS)),
                ]);
                return Err(Box::new(Error2::with_options(
                    WEB_FETCH_FAILED,
                    format!(
                        "Too many redirects while fetching \"{url}\" (limit {MAX_REDIRECT_HOPS})."
                    ),
                    Error2Options {
                        details: Some(details),
                        ..Default::default()
                    },
                )));
            }
            redirects += 1;
            let base = Url::parse(&current_url).map_err(|error| Box::new(error) as FetchError)?;
            current_url = base
                .join(&location)
                .map_err(|error| Box::new(error) as FetchError)?
                .to_string();
        }
    }

    async fn fetch_with_deadline(
        &self,
        url: &str,
        signal: Option<&CancellationToken>,
        deadline: Instant,
    ) -> Result<FetchResponse, FetchError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let remaining_ms = remaining.as_millis();
        if remaining_ms == 0 {
            return Err(Box::new(fetch_timeout_error(url, self.timeout_ms)));
        }

        let hop_signal = signal
            .map(|signal| signal.child_token())
            .unwrap_or_default();
        let timed_out = Arc::new(AtomicBool::new(false));
        let timer_signal = hop_signal.clone();
        let timer_flag = timed_out.clone();
        let timer: IHostTimerHandle = self.timers.set_timeout(
            Arc::new(move || {
                timer_flag.store(true, Ordering::Release);
                timer_signal.cancel();
            }),
            remaining_ms.min(u64::MAX as u128) as u64,
        );
        let result = self
            .fetch_impl
            .fetch(
                url,
                FetchInit {
                    headers: vec![("User-Agent".to_string(), self.user_agent.clone())],
                    method: Some("GET".to_string()),
                    signal: Some(hop_signal),
                    manual_redirect: true,
                    ..Default::default()
                },
            )
            .await;
        self.timers.clear_timeout(timer);

        match result {
            Ok(response) => Ok(response),
            Err(_)
                if timed_out.load(Ordering::Acquire)
                    && !signal.is_some_and(CancellationToken::is_cancelled) =>
            {
                Err(Box::new(fetch_timeout_error(url, self.timeout_ms)))
            }
            Err(error) => Err(error),
        }
    }

    fn extract_main_content(&self, html: &str) -> Result<String, FetchError> {
        let content = crate::tools::fetch_url::extract_html_content(html);
        if content.trim().is_empty() {
            return Err(Box::new(Error2::new(
                WEB_FETCH_FAILED,
                "Failed to extract meaningful content from the page. The page may require JavaScript to render.",
            )));
        }
        Ok(content)
    }
}

impl Default for LocalFetchURLProvider {
    fn default() -> Self {
        Self::new(LocalFetchURLProviderOptions::default())
    }
}

#[async_trait]
impl UrlFetcher for LocalFetchURLProvider {
    async fn fetch(
        &self,
        url: &str,
        options: UrlFetchOptions,
    ) -> Result<UrlFetchResult, FetchError> {
        let response = self
            .request_with_redirects(url, options.signal.as_ref())
            .await?;
        self.read_response(response, options.signal.as_ref()).await
    }
}

fn too_large_error(bytes: f64, max_bytes: usize) -> Error2 {
    let details = Map::from_iter([
        ("bytes".to_string(), Value::from(bytes)),
        ("maxBytes".to_string(), Value::from(max_bytes)),
    ]);
    Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Response body too large: {bytes} bytes exceeds maxBytes ({max_bytes})."),
        Error2Options {
            details: Some(details),
            ..Default::default()
        },
    )
}

fn fetch_timeout_error(url: &str, timeout_ms: u64) -> Error2 {
    let details = Map::from_iter([
        ("url".to_string(), Value::String(url.to_string())),
        ("timeoutMs".to_string(), Value::from(timeout_ms)),
    ]);
    Error2::with_options(
        WEB_FETCH_FAILED,
        format!("Fetching \"{url}\" timed out after {timeout_ms}ms."),
        Error2Options {
            details: Some(details),
            ..Default::default()
        },
    )
}

async fn read_body_with_cap(
    body: Option<FetchByteStream>,
    max_bytes: usize,
    signal: Option<&CancellationToken>,
) -> Result<String, FetchError> {
    let Some(mut stream) = body else {
        return Ok(String::new());
    };
    let mut bytes = Vec::new();
    let mut total = 0usize;
    loop {
        let next = match signal {
            Some(signal) => tokio::select! {
                next = stream.next() => next,
                _ = signal.cancelled() => {
                    return Err(Box::new(AbortError::new("The operation was aborted.")));
                }
            },
            None => stream.next().await,
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk?;
        total = total.saturating_add(chunk.len());
        if total > max_bytes {
            return Err(Box::new(too_large_error(total as f64, max_bytes)));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{FEFF}').unwrap_or(text)
}

fn validate_fetch_url(url: &str) -> Result<(), FetchError> {
    let parsed = match Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => {
            let details = Map::from_iter([(
                "url".to_string(),
                Value::String(url.to_string()),
            )]);
            return Err(Box::new(Error2::with_options(
                WEB_INVALID_URL,
                format!("Invalid URL: \"{url}\""),
                Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            )));
        }
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        let protocol = format!("{}:", parsed.scheme());
        let details = Map::from_iter([
            ("url".to_string(), Value::String(url.to_string())),
            (
                "protocol".to_string(),
                Value::String(protocol.clone()),
            ),
        ]);
        return Err(Box::new(Error2::with_options(
            WEB_INVALID_URL,
            format!("Unsupported URL scheme \"{protocol}\" — only http(s) allowed."),
            Error2Options {
                details: Some(details),
                ..Default::default()
            },
        )));
    }
    Ok(())
}
