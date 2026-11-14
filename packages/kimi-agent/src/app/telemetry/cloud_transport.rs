use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::_base::utils::abort::{AbortError, AbortSignal, abortable, is_abort_error};
use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchLike, FetchResponse};
use crate::mcp_core::client_shared::ReqwestFetch;
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::wire::record::WireAsyncError;

pub type CloudPrimitive = Value;
pub type CloudProperties = Map<String, CloudPrimitive>;
pub type CloudContext = Map<String, CloudPrimitive>;
pub type EndpointResolver = Arc<dyn Fn() -> Option<String> + Send + Sync>;
pub type AccessTokenProvider = Arc<dyn Fn() -> BoxFuture<'static, Option<String>> + Send + Sync>;
pub type SleepImpl = Arc<
    dyn Fn(u64, Option<AbortSignal>) -> BoxFuture<'static, Result<(), WireAsyncError>>
        + Send
        + Sync,
>;
pub type NowImpl = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudEvent {
    pub event_id: String,
    pub device_id: Option<String>,
    pub session_id: Option<String>,
    pub event: String,
    pub timestamp: f64,
    pub properties: CloudProperties,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrichedCloudEvent {
    pub event_id: String,
    pub device_id: Option<String>,
    pub session_id: Option<String>,
    pub event: String,
    pub timestamp: f64,
    pub properties: CloudProperties,
    pub context: CloudContext,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudPayload {
    pub user_id: String,
    pub events: Vec<Map<String, CloudPrimitive>>,
}

#[derive(Clone)]
pub struct CloudTransportOptions {
    pub storage: Arc<dyn IFileSystemStorageService>,
    pub device_id: String,
    pub endpoint: Option<String>,
    pub resolve_endpoint: Option<EndpointResolver>,
    pub get_access_token: Option<AccessTokenProvider>,
    pub fetch_impl: Option<Arc<dyn FetchLike>>,
    pub retry_backoffs_ms: Option<Vec<u64>>,
    pub request_timeout_ms: Option<u64>,
    pub sleep: Option<SleepImpl>,
    pub now: Option<NowImpl>,
    pub max_disk_files: Option<usize>,
    pub max_disk_bytes: Option<u64>,
}

pub const SERVER_EVENT_PREFIX: &str = "kfc_";
pub const USER_ID_PREFIX: &str = "kfc_device_id_";
pub const DISK_EVENT_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
pub const DISK_EVENT_MAX_FILES: usize = 500;
pub const DISK_EVENT_MAX_BYTES: u64 = 50 * 1024 * 1024;
pub const RETRY_BACKOFFS_MS: &[u64] = &[1_000, 4_000, 16_000];

const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 10_000;
const TELEMETRY_SCOPE: &str = "telemetry";
const FAILED_PREFIX: &str = "failed_";
const JSONL_SUFFIX: &str = ".jsonl";
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskEventCaps {
    pub max_disk_files: Option<usize>,
    pub max_disk_bytes: Option<u64>,
}

#[derive(Debug)]
pub struct TelemetryTypeError {
    message: String,
}

impl TelemetryTypeError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TelemetryTypeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TelemetryTypeError {}

#[derive(Debug)]
pub struct TransientCloudError {
    message: String,
}

impl TransientCloudError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn name(&self) -> &'static str {
        "TransientCloudError"
    }
}

impl fmt::Display for TransientCloudError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TransientCloudError {}

#[derive(Debug)]
struct TelemetryRequestTimeout;

impl fmt::Display for TelemetryRequestTimeout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("telemetry request timed out")
    }
}

impl Error for TelemetryRequestTimeout {}

pub struct CloudTransport {
    storage: Arc<dyn IFileSystemStorageService>,
    device_id: String,
    endpoint: Option<String>,
    resolve_endpoint: EndpointResolver,
    get_access_token: Option<AccessTokenProvider>,
    fetch_impl: Arc<dyn FetchLike>,
    retry_backoffs_ms: Vec<u64>,
    request_timeout_ms: u64,
    sleep_impl: SleepImpl,
    now: NowImpl,
    pub max_disk_files: usize,
    pub max_disk_bytes: u64,
}

impl CloudTransport {
    pub fn new(options: CloudTransportOptions) -> Self {
        let _requested_caps = DiskEventCaps {
            max_disk_files: options.max_disk_files,
            max_disk_bytes: options.max_disk_bytes,
        };
        Self {
            storage: options.storage,
            device_id: options.device_id,
            endpoint: options.endpoint,
            resolve_endpoint: options
                .resolve_endpoint
                .unwrap_or_else(|| Arc::new(|| None)),
            get_access_token: options.get_access_token,
            fetch_impl: options
                .fetch_impl
                .unwrap_or_else(|| Arc::new(ReqwestFetch::new())),
            retry_backoffs_ms: options
                .retry_backoffs_ms
                .unwrap_or_else(|| RETRY_BACKOFFS_MS.to_vec()),
            request_timeout_ms: options
                .request_timeout_ms
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
            sleep_impl: options.sleep.unwrap_or_else(|| Arc::new(abortable_sleep)),
            now: options
                .now
                .unwrap_or_else(|| Arc::new(|| chrono::Utc::now().timestamp_millis())),
            max_disk_files: DISK_EVENT_MAX_FILES,
            max_disk_bytes: DISK_EVENT_MAX_BYTES,
        }
    }

    fn current_endpoint(&self) -> Option<String> {
        self.endpoint
            .clone()
            .or_else(|| (self.resolve_endpoint)())
    }

    pub async fn send(
        &self,
        events: &[EnrichedCloudEvent],
        signal: Option<AbortSignal>,
    ) -> Result<(), WireAsyncError> {
        if events.is_empty() {
            return Ok(());
        }
        let Some(endpoint) = self.current_endpoint() else {
            return Ok(());
        };
        let mut saved_to_disk = false;
        if is_signal_aborted(signal.as_ref()) {
            self.save_events_to_disk_once(events, &mut saved_to_disk)
                .await?;
            return Err(abort_error());
        }
        let payload = match build_payload(events, &self.device_id) {
            Ok(payload) => payload,
            Err(_) => return Ok(()),
        };

        for attempt in 0..=self.retry_backoffs_ms.len() {
            match self.send_http(&payload, signal.clone(), &endpoint).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if is_signal_aborted(signal.as_ref()) || is_abort_error(error.as_ref()) {
                        self.save_events_to_disk_once(events, &mut saved_to_disk)
                            .await?;
                        return Err(error);
                    }
                    if error.downcast_ref::<TransientCloudError>().is_none() {
                        break;
                    }
                    let Some(backoff) = self.retry_backoffs_ms.get(attempt).copied() else {
                        break;
                    };
                    if let Err(error) = (self.sleep_impl)(backoff, signal.clone()).await {
                        if is_signal_aborted(signal.as_ref()) || is_abort_error(error.as_ref()) {
                            self.save_events_to_disk_once(events, &mut saved_to_disk)
                                .await?;
                            return Err(error);
                        }
                        break;
                    }
                }
            }
        }

        self.save_events_to_disk_once(events, &mut saved_to_disk)
            .await
    }

    async fn save_events_to_disk_once(
        &self,
        events: &[EnrichedCloudEvent],
        saved_to_disk: &mut bool,
    ) -> Result<(), WireAsyncError> {
        if *saved_to_disk {
            return Ok(());
        }
        self.save_to_disk(events).await?;
        *saved_to_disk = true;
        Ok(())
    }

    pub async fn save_to_disk(&self, events: &[EnrichedCloudEvent]) -> Result<(), WireAsyncError> {
        if events.is_empty() {
            return Ok(());
        }
        let key = format!(
            "{FAILED_PREFIX}{}_{}{JSONL_SUFFIX}",
            (self.now)(),
            random_suffix()?
        );
        let mut text = events
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        text.push('\n');
        self.storage
            .write(TELEMETRY_SCOPE, &key, text.into_bytes(), None)
            .await
    }

    pub async fn retry_disk_events(&self) -> Result<(), WireAsyncError> {
        let endpoint = self.current_endpoint();
        let keys = self
            .storage
            .list(TELEMETRY_SCOPE, Some(FAILED_PREFIX), None)
            .await?;
        let now = (self.now)() as f64;
        for key in keys {
            if !key.starts_with(FAILED_PREFIX) || !key.ends_with(JSONL_SUFFIX) {
                continue;
            }
            let created_at = parse_failed_timestamp(&key);
            if created_at.is_none_or(|created_at| now - created_at > DISK_EVENT_MAX_AGE_MS as f64)
            {
                let _ = self.storage.delete(TELEMETRY_SCOPE, &key).await;
                continue;
            }

            let events = match self.read_jsonl(&key).await {
                Ok(events) => events,
                Err(error) => {
                    if is_invalid_cloud_data(error.as_ref()) {
                        let _ = self.storage.delete(TELEMETRY_SCOPE, &key).await;
                    }
                    continue;
                }
            };

            let Some(endpoint) = endpoint.as_deref() else {
                continue;
            };
            let payload = match build_payload(&events, &self.device_id) {
                Ok(payload) => payload,
                Err(_) => {
                    let _ = self.storage.delete(TELEMETRY_SCOPE, &key).await;
                    continue;
                }
            };

            match self.send_http(&payload, None, endpoint).await {
                Ok(()) => {
                    let _ = self.storage.delete(TELEMETRY_SCOPE, &key).await;
                }
                Err(error) if error.downcast_ref::<TransientCloudError>().is_some() => continue,
                Err(_) => continue,
            }
        }
        Ok(())
    }

    pub async fn prune_expired_disk_events(&self) -> Result<(), WireAsyncError> {
        let listed = self
            .storage
            .list(TELEMETRY_SCOPE, Some(FAILED_PREFIX), None)
            .await?;
        let now = (self.now)() as f64;
        let mut survivors = Vec::new();
        for key in listed {
            if !key.starts_with(FAILED_PREFIX) || !key.ends_with(JSONL_SUFFIX) {
                continue;
            }
            let created_at = parse_failed_timestamp(&key);
            if created_at.is_none_or(|created_at| now - created_at > DISK_EVENT_MAX_AGE_MS as f64)
            {
                let _ = self.storage.delete(TELEMETRY_SCOPE, &key).await;
                continue;
            }
            match self.read_jsonl(&key).await {
                Ok(_) => {}
                Err(error) if is_invalid_cloud_data(error.as_ref()) => {
                    let _ = self.storage.delete(TELEMETRY_SCOPE, &key).await;
                    continue;
                }
                Err(_) => {}
            }
            survivors.push(key);
        }
        self.enforce_disk_event_caps(&survivors).await
    }

    async fn enforce_disk_event_caps(&self, keys: &[String]) -> Result<(), WireAsyncError> {
        struct SizedEvent {
            key: String,
            created_at: f64,
            size: u64,
        }

        let mut sized = Vec::new();
        for key in keys {
            let size = match self.storage.size(TELEMETRY_SCOPE, key).await {
                Ok(Some(size)) => size,
                Ok(None) | Err(_) => continue,
            };
            sized.push(SizedEvent {
                key: key.clone(),
                created_at: parse_failed_timestamp(key).unwrap_or(0.0),
                size,
            });
        }
        sized.sort_by(|left, right| left.created_at.total_cmp(&right.created_at));
        let mut total_bytes = sized.iter().map(|entry| entry.size).sum::<u64>();
        let mut total_files = sized.len();
        for entry in sized {
            if total_files <= self.max_disk_files && total_bytes <= self.max_disk_bytes {
                return Ok(());
            }
            let _ = self.storage.delete(TELEMETRY_SCOPE, &entry.key).await;
            total_files -= 1;
            total_bytes = total_bytes.saturating_sub(entry.size);
        }
        Ok(())
    }

    async fn read_jsonl(&self, key: &str) -> Result<Vec<EnrichedCloudEvent>, WireAsyncError> {
        let Some(bytes) = self.storage.read(TELEMETRY_SCOPE, key).await? else {
            return Ok(Vec::new());
        };
        let text = String::from_utf8(bytes)
            .map_err(|error| Box::new(TelemetryTypeError::new(error.to_string())) as WireAsyncError)?;
        let mut events = Vec::new();
        for line in text.split('\n') {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let event = serde_json::from_str::<EnrichedCloudEvent>(trimmed).map_err(|error| {
                Box::new(TelemetryTypeError::new(error.to_string())) as WireAsyncError
            })?;
            events.push(event);
        }
        Ok(events)
    }

    async fn send_http(
        &self,
        payload: &CloudPayload,
        signal: Option<AbortSignal>,
        endpoint: &str,
    ) -> Result<(), WireAsyncError> {
        let token = match &self.get_access_token {
            Some(get_access_token) => get_access_token().await,
            None => None,
        };
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let has_authorization = token.as_ref().is_some_and(|token| !token.is_empty());
        if let Some(token) = token.filter(|token| !token.is_empty()) {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }

        let response = self
            .post(payload, headers.clone(), signal.clone(), endpoint)
            .await?;
        if response.status == 401 && has_authorization {
            headers.retain(|(key, _)| key != "Authorization");
            let retry = self.post(payload, headers, signal, endpoint).await?;
            handle_status(retry.status)?;
            return Ok(());
        }
        handle_status(response.status)?;
        Ok(())
    }

    async fn post(
        &self,
        payload: &CloudPayload,
        headers: Vec<(String, String)>,
        signal: Option<AbortSignal>,
        endpoint: &str,
    ) -> Result<FetchResponse, WireAsyncError> {
        let result = match serde_json::to_string(payload) {
            Ok(body) => {
                fetch_with_timeout(
                    self.fetch_impl.clone(),
                    endpoint.to_string(),
                    FetchInit {
                        method: Some("POST".to_string()),
                        headers,
                        body: Some(body),
                        timeout_ms: Some(self.request_timeout_ms),
                        ..Default::default()
                    },
                    self.request_timeout_ms,
                    signal.clone(),
                )
                .await
            }
            Err(error) => Err(Box::new(error) as FetchError),
        };
        match result {
            Ok(response) => Ok(response),
            Err(error) if is_signal_aborted(signal.as_ref()) || is_abort_error(error.as_ref()) => {
                Err(error)
            }
            Err(error) => Err(Box::new(TransientCloudError::new(error.to_string()))),
        }
    }
}

fn random_suffix() -> Result<String, WireAsyncError> {
    let mut bytes = [0_u8; 6];
    getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn parse_failed_timestamp(key: &str) -> Option<f64> {
    let rest = &key[FAILED_PREFIX.len()..];
    let underscore = rest.find('_')?;
    let timestamp = rest[..underscore].parse::<f64>().ok()?;
    timestamp.is_finite().then_some(timestamp)
}

pub fn build_user_id(device_id: &str) -> String {
    format!("{USER_ID_PREFIX}{device_id}")
}

pub fn build_payload(
    events: &[EnrichedCloudEvent],
    device_id: &str,
) -> Result<CloudPayload, TelemetryTypeError> {
    Ok(CloudPayload {
        user_id: build_user_id(device_id),
        events: events
            .iter()
            .cloned()
            .map(|event| flatten_event(&apply_server_prefix(&event)))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

pub fn apply_server_prefix(event: &EnrichedCloudEvent) -> EnrichedCloudEvent {
    if event.event.is_empty() || event.event.starts_with(SERVER_EVENT_PREFIX) {
        return event.clone();
    }
    let mut prefixed = event.clone();
    prefixed.event = format!("{SERVER_EVENT_PREFIX}{}", event.event);
    prefixed
}

pub fn flatten_event(
    event: &EnrichedCloudEvent,
) -> Result<Map<String, CloudPrimitive>, TelemetryTypeError> {
    let mut out = Map::new();
    insert_primitive(&mut out, "event_id", Value::String(event.event_id.clone()))?;
    insert_primitive(
        &mut out,
        "device_id",
        event.device_id.clone().map_or(Value::Null, Value::String),
    )?;
    insert_primitive(
        &mut out,
        "session_id",
        event.session_id.clone().map_or(Value::Null, Value::String),
    )?;
    insert_primitive(&mut out, "event", Value::String(event.event.clone()))?;
    let timestamp = Number::from_f64(event.timestamp)
        .ok_or_else(|| TelemetryTypeError::new("telemetry timestamp must be primitive"))?;
    insert_primitive(&mut out, "timestamp", Value::Number(timestamp))?;
    flatten_nested(&mut out, "property", &event.properties)?;
    flatten_nested(&mut out, "context", &event.context)?;
    Ok(out)
}

pub fn is_cloud_primitive(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => true,
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                value >= -(MAX_SAFE_INTEGER as i64) && value <= MAX_SAFE_INTEGER as i64
            } else if let Some(value) = number.as_u64() {
                value <= MAX_SAFE_INTEGER as u64
            } else if let Some(value) = number.as_f64() {
                value.is_finite() && value.abs() <= MAX_SAFE_INTEGER
            } else {
                false
            }
        }
        Value::Array(_) | Value::Object(_) => false,
    }
}

fn flatten_nested(
    target: &mut Map<String, CloudPrimitive>,
    prefix: &str,
    values: &Map<String, CloudPrimitive>,
) -> Result<(), TelemetryTypeError> {
    for (key, value) in values {
        assert_primitive(&format!("{prefix}.{key}"), value)?;
        if !value.is_null() {
            target.insert(format!("{prefix}_{key}"), value.clone());
        }
    }
    Ok(())
}

fn insert_primitive(
    target: &mut Map<String, CloudPrimitive>,
    key: &str,
    value: CloudPrimitive,
) -> Result<(), TelemetryTypeError> {
    assert_primitive(key, &value)?;
    if !value.is_null() {
        target.insert(key.to_string(), value);
    }
    Ok(())
}

fn assert_primitive(key: &str, value: &Value) -> Result<(), TelemetryTypeError> {
    if is_cloud_primitive(value) {
        return Ok(());
    }
    Err(TelemetryTypeError::new(format!(
        "telemetry {key} must be primitive"
    )))
}

fn handle_status(status: u16) -> Result<(), TransientCloudError> {
    if status >= 500 || status == 429 {
        return Err(TransientCloudError::new(format!("HTTP {status}")));
    }
    Ok(())
}

async fn fetch_with_timeout(
    fetch_impl: Arc<dyn FetchLike>,
    url: String,
    init: FetchInit,
    timeout_ms: u64,
    external_signal: Option<AbortSignal>,
) -> Result<FetchResponse, FetchError> {
    let request = async move { fetch_impl.fetch(&url, init).await };
    let request = async move {
        match external_signal {
            Some(signal) => match abortable(request, signal).await {
                Ok(result) => result,
                Err(_) => Err(abort_error() as FetchError),
            },
            None => request.await,
        }
    };
    match tokio::time::timeout(Duration::from_millis(timeout_ms), request).await {
        Ok(result) => result,
        Err(_) => Err(Box::new(TelemetryRequestTimeout)),
    }
}

fn abortable_sleep(
    ms: u64,
    signal: Option<AbortSignal>,
) -> BoxFuture<'static, Result<(), WireAsyncError>> {
    Box::pin(async move {
        let sleep = tokio::time::sleep(Duration::from_millis(ms));
        match signal {
            Some(signal) => match abortable(sleep, signal).await {
                Ok(()) => Ok(()),
                Err(_) => Err(abort_error()),
            },
            None => {
                sleep.await;
                Ok(())
            }
        }
    })
}

fn is_signal_aborted(signal: Option<&AbortSignal>) -> bool {
    signal.is_some_and(AbortSignal::aborted)
}

fn abort_error() -> WireAsyncError {
    Box::new(AbortError::new("The operation was aborted."))
}

fn is_invalid_cloud_data(error: &(dyn Error + 'static)) -> bool {
    error.is::<TelemetryTypeError>()
}
