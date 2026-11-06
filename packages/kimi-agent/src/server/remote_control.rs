//! Remote Control runtime (Rust port).
//!
//! This module is a faithful port of the TypeScript reference implementation at
//! `apps/kimi-code/src/cli/sub/web/remote-control.ts` (1109 lines). It implements
//! the standalone Remote Control client: device registration against the relay,
//! a long-lived WebSocket management + HTTP-tunnel connection, a 30s heartbeat /
//! 300s silence watchdog, exponential-backoff reconnection (capped at 30s), and a
//! reverse HTTP proxy that forwards tunneled requests to the local loopback kimi
//! service via `reqwest`.
//!
//! **Scope of this port:** the module body only. Wiring the runtime into the
//! standalone HTTP server's `GET/POST /api/v1/remote-control` endpoints (and
//! reading `RemoteControlStatusWire` from `server/mod.rs`) is performed by
//! `team-lead` in `server/mod.rs`. This file must not be edited by the wiring
//! step beyond the single `pub mod remote_control;` declaration.
//!
//! Conventions: every timeout / constant mirrors the TS source; the TS source
//! line that defines it is noted in a trailing comment.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http as ws_http;
use url::Url;

// Re-use the wire shape declared (and the `/api/v1/remote-control` endpoint)
// by the wiring layer in `server/mod.rs`; this keeps our status payloads
// exactly aligned with what the HTTP server serializes.
use super::RemoteControlStatusWire;

// ----------------------------------------------------------------------------
// Constants (mirrored from remote-control.ts)
// ----------------------------------------------------------------------------

/// Default relay origin. (ts:27)
pub const REMOTE_CONTROL_RELAY_ORIGIN: &str = "https://code-rc.kimi.com";
/// Environment variable overriding the relay origin. (ts:29)
pub const REMOTE_CONTROL_RELAY_URL_ENV: &str = "KIMI_CODE_REMOTE_CONTROL_RELAY_URL";

const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024; // ts:40
const MAX_HTTP_REQUEST_BYTES: usize = 10 * 1024 * 1024; // ts:41
const HTTP_REQUEST_TIMEOUT_MS: u64 = 30_000; // ts:42
const REGISTER_TIMEOUT_MS: u64 = 10_000; // ts:43
const MAX_RECONNECT_DELAY_MS: u64 = 30_000; // ts:44
const RELAY_PING_INTERVAL_MS: u64 = 30_000; // ts:45
const RELAY_SILENCE_TIMEOUT_MS: u64 = 300_000; // ts:46
// The TS implementation reads the relay's rejection body on a refused WS
// upgrade (ts:47-49, 837-855). `tokio-tungstenite` surfaces the handshake
// failure directly as an error, so this port does not parse that body; the
// constants are retained for parity with the reference source.
#[allow(dead_code)]
const HANDSHAKE_REJECTION_TIMEOUT_MS: u64 = 2000; // ts:47
#[allow(dead_code)]
const HANDSHAKE_REJECTION_BODY_BYTES: usize = 512; // ts:48
#[allow(dead_code)]
const HANDSHAKE_REJECTION_TEXT_LIMIT: usize = 200; // ts:49

/// Request headers stripped before forwarding to the local service. (ts:50-65)
pub const BLOCKED_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "host",
    "origin",
    "proxy-authorization",
    "proxy-authenticate",
    "accept-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Response headers stripped before tunneling back to the relay. (ts:66-77)
const BLOCKED_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "content-length",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

// ----------------------------------------------------------------------------
// Options / handle
// ----------------------------------------------------------------------------

/// Options for [`RemoteControlRuntime::start`].
///
/// Mirrors the TS `RemoteControlOptions` (ts:102) minus the Node-only bits
/// (`homeDir`, `stderr`, `onStatus` callback — status is exposed via
/// [`RemoteControlHandle::status`] here). The device id and tokens are produced
/// by the caller (wiring layer) and injected.
#[derive(Clone)]
pub struct RemoteControlOptions {
    /// Explicit relay origin. When set it wins outright, including over the
    /// env override (v2 `options.relayOrigin ?? resolveRemoteControl…`).
    pub relay_origin: String,
    /// The relay origin the login region resolves to (v2 `auth.relayOrigin`).
    /// It is the fallback the env override resolves against, so an operator
    /// override still wins over the region default.
    pub region_relay_origin: String,
    /// Stable device id (TS: `createKimiDeviceId(homeDir)`).
    pub device_id: String,
    /// Human-readable device name (TS: `hostname()`).
    pub device_name: String,
    /// Local loopback origin the reverse proxy forwards to, e.g. `http://127.0.0.1:3461`.
    pub local_base_url: String,
    /// Bearer token used to authenticate to the local kimi service on forward. (ts:105)
    pub local_server_token: String,
    /// OAuth refresh token sent to the relay for the WS upgrade.
    pub refresh_token: String,
    /// `client_version` advertised in the register payload (ts: `kimi-code/${getVersion()}`).
    pub client_version: String,
    /// `platform` advertised in the register payload (ts: `platform()`).
    pub platform: String,
    /// Heartbeat ping interval, ms (ts:45).
    pub ping_interval_ms: u64,
    /// Silence watchdog timeout, ms (ts:46).
    pub silence_timeout_ms: u64,
}

impl Default for RemoteControlOptions {
    fn default() -> Self {
        Self {
            relay_origin: String::new(),
            region_relay_origin: REMOTE_CONTROL_RELAY_ORIGIN.to_string(),
            device_id: String::new(),
            device_name: whoami::fallible::hostname().unwrap_or_else(|_| "unknown".to_string()),
            local_base_url: "http://127.0.0.1:3461".to_string(),
            local_server_token: String::new(),
            refresh_token: String::new(),
            client_version: format!("kimi-code/{}", env!("CARGO_PKG_VERSION")),
            platform: std::env::consts::OS.to_string(),
            ping_interval_ms: RELAY_PING_INTERVAL_MS,
            silence_timeout_ms: RELAY_SILENCE_TIMEOUT_MS,
        }
    }
}

/// Handle returned by [`RemoteControlRuntime::start`]; owns the background task.
pub struct RemoteControlHandle {
    device_id: String,
    device_name: String,
    url: String,
    /// Local loopback origin this tunnel forwards to, reported as the conflict
    /// holder's `local_origin`.
    local_origin: String,
    /// Epoch milliseconds when this runtime took the local slot.
    started_at: u64,
    status: Arc<Mutex<RemoteControlStatusWire>>,
    shutdown: tokio_util::sync::CancellationToken,
    _task: tokio::task::JoinHandle<()>,
}

impl RemoteControlHandle {
    /// Device id assigned at registration.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Device name announced to the relay.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Public Remote Control URL (built from `device_id` + relay origin).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Local loopback origin reported when this runtime is the conflict holder.
    pub fn local_origin(&self) -> &str {
        &self.local_origin
    }

    /// Epoch milliseconds when this runtime took the local slot.
    pub fn started_at(&self) -> u64 {
        self.started_at
    }

    /// Current status, serialized to the existing wire shape in `server/mod.rs`.
    pub fn status(&self) -> RemoteControlStatusWire {
        self.status.lock().unwrap().clone()
    }

    /// Stop the runtime and wait for the background task to finish.
    pub async fn close(self) {
        self.shutdown.cancel();
        let _ = self._task.await;
    }
}

/// The runtime entry point. Spawns the state machine and returns a
/// [`RemoteControlHandle`].
pub struct RemoteControlRuntime;

impl RemoteControlRuntime {
    /// Start the Remote Control client. With no explicit `relay_origin`, the
    /// `KIMI_CODE_REMOTE_CONTROL_RELAY_URL` env override wins, then the
    /// region-resolved fallback — the v2 precedence
    /// `resolveRemoteControlRelayOrigin(env, auth.relayOrigin)` (ts:33-38).
    pub fn start(options: RemoteControlOptions) -> RemoteControlHandle {
        let relay_origin = if options.relay_origin.trim().is_empty() {
            resolve_remote_control_relay_origin(&options.region_relay_origin)
        } else {
            options.relay_origin.clone()
        };
        let mut options = options;
        options.relay_origin = relay_origin;

        let url = build_remote_control_url(&options.device_id, None, &options.relay_origin);
        let status = Arc::new(Mutex::new(RemoteControlStatusWire::off()));
        {
            let mut s = status.lock().unwrap();
            s.enabled = true;
            s.state = "connecting".to_string();
            s.url = Some(url.clone());
            s.device_id = Some(options.device_id.clone());
            s.device_name = Some(options.device_name.clone());
        }

        let shutdown = tokio_util::sync::CancellationToken::new();
        let inner = Arc::new(options);
        let handle_options = inner.clone();
        let task = tokio::spawn(run_state_machine(inner, status.clone(), shutdown.clone()));

        RemoteControlHandle {
            device_id: handle_options.device_id.clone(),
            device_name: handle_options.device_name.clone(),
            url,
            local_origin: handle_options.local_base_url.clone(),
            started_at: epoch_millis(),
            status,
            shutdown,
            _task: task,
        }
    }
}

/// Epoch milliseconds, the `started_at` unit on the conflict wire shape.
fn epoch_millis() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// Trim and validate a rename alias: 1..=50 characters after trimming, the
/// same transform as the protocol's `renameRemoteControlDeviceRequestSchema`.
pub fn normalize_device_alias(value: &JsonValue) -> std::result::Result<String, String> {
    let alias = value
        .as_str()
        .ok_or_else(|| "Field 'alias' must be a string.".to_string())?
        .trim();
    if alias.is_empty() {
        return Err("Field 'alias' must not be empty.".to_string());
    }
    if alias.chars().count() > 50 {
        return Err("Field 'alias' must be at most 50 characters.".to_string());
    }
    Ok(alias.to_string())
}

/// The in-process equivalent of upstream's `server/rc.json` start gate.
///
/// The fork runtime has a single owner (`remote_control_runtime`) and no
/// cross-process lock, so the slot mutex is the mutual-exclusion primitive.
/// Logout still needs a short-lived flag: it keeps a start from racing the
/// credential wipe and the delayed teardown that follows the logout response.
#[derive(Default)]
pub struct RemoteControlLogoutGate {
    active: std::sync::atomic::AtomicBool,
    started_at: std::sync::atomic::AtomicU64,
}

/// Origin marker carried by a logout-held gate, mirroring upstream's
/// `REMOTE_CONTROL_LOGOUT_GATE_ORIGIN`.
pub const REMOTE_CONTROL_LOGOUT_GATE_ORIGIN: &str = "logout in progress";

const LOGOUT_TUNNEL_TEARDOWN_DELAY_MS: u64 = 250;

impl RemoteControlLogoutGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the gate held and return its `started_at`.
    pub fn begin(&self) -> u64 {
        let started_at = epoch_millis();
        self.started_at
            .store(started_at, std::sync::atomic::Ordering::SeqCst);
        self.active.store(true, std::sync::atomic::Ordering::SeqCst);
        started_at
    }

    pub fn end(&self) {
        self.active
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// The holder's `started_at` while the gate is held.
    pub fn active_since(&self) -> Option<u64> {
        if self.active.load(std::sync::atomic::Ordering::SeqCst) {
            Some(self.started_at.load(std::sync::atomic::Ordering::SeqCst))
        } else {
            None
        }
    }
}

/// Remote-control cleanup the server wires into the managed Kimi logout: drop
/// the relay session cache around the credential wipe, then stop the local
/// tunnel after the response (upstream `registerOAuthRoutes` logout branch).
pub struct RemoteControlLogoutCleanup {
    runtime: Arc<tokio::sync::Mutex<Option<RemoteControlHandle>>>,
    state: Arc<tokio::sync::Mutex<crate::server::RemoteControlStatusWire>>,
    devices: Arc<RemoteControlDeviceClient>,
    gate: Arc<RemoteControlLogoutGate>,
    /// Serialises concurrent logouts, like upstream's `kimiLogoutQueue`.
    serial: std::sync::Mutex<()>,
}

impl RemoteControlLogoutCleanup {
    pub fn new(
        runtime: Arc<tokio::sync::Mutex<Option<RemoteControlHandle>>>,
        state: Arc<tokio::sync::Mutex<crate::server::RemoteControlStatusWire>>,
        devices: Arc<RemoteControlDeviceClient>,
        gate: Arc<RemoteControlLogoutGate>,
    ) -> Self {
        Self {
            runtime,
            state,
            devices,
            gate,
            serial: std::sync::Mutex::new(()),
        }
    }
}

impl crate::server::oauth::RemoteControlLogoutHook for RemoteControlLogoutCleanup {
    fn around_logout(&self, logout: &mut dyn FnMut()) {
        let _serial = self
            .serial
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.gate.begin();
        self.devices.clear_sessions();
        logout();
        self.devices.clear_sessions();

        let Ok(tokio_handle) = tokio::runtime::Handle::try_current() else {
            // No async runtime to schedule the delayed teardown on: the
            // credential wipe already makes the cached cookie unreachable.
            self.gate.end();
            return;
        };
        let runtime = self.runtime.clone();
        let state = self.state.clone();
        let devices = self.devices.clone();
        let gate = self.gate.clone();
        tokio_handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(LOGOUT_TUNNEL_TEARDOWN_DELAY_MS)).await;
            let taken = runtime.lock().await.take();
            if let Some(handle) = taken {
                handle.close().await;
            }
            devices.clear_sessions();
            *state.lock().await = crate::server::RemoteControlStatusWire::off();
            gate.end();
        });
    }
}

/// Resolve the relay origin: the `KIMI_CODE_REMOTE_CONTROL_RELAY_URL` env
/// override, else `fallback` (ts:33-38, v2 `resolveRemoteControlRelayOrigin`).
pub fn resolve_remote_control_relay_origin(fallback: &str) -> String {
    match std::env::var(REMOTE_CONTROL_RELAY_URL_ENV) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ if fallback.trim().is_empty() => REMOTE_CONTROL_RELAY_ORIGIN.to_string(),
        _ => fallback.to_string(),
    }
}

// ----------------------------------------------------------------------------
// Wire-message shapes
// ----------------------------------------------------------------------------

/// Register message sent on the management socket right after connect (ts:443-454).
#[derive(Serialize)]
pub struct RegisterMessage<'a> {
    #[serde(rename = "type")]
    pub r#type: &'a str,
    pub payload: RegisterPayload<'a>,
}

#[derive(Serialize)]
pub struct RegisterPayload<'a> {
    pub device_id: &'a str,
    pub alias: &'a str,
    pub platform: &'a str,
    pub client_version: &'a str,
    pub local_base_url: &'a str,
}

/// Tunneled HTTP request received on the `http` socket (ts:552-587).
#[derive(serde::Deserialize)]
struct TunnelHttpRequest {
    #[serde(rename = "request_id")]
    request_id: String,
    #[serde(rename = "type")]
    r#type: String,
    #[serde(rename = "is_last")]
    is_last: bool,
    #[serde(rename = "body_base64")]
    body_base64: String,
}

/// Tunneled HTTP response sent back on the `http` socket (ts:606-616).
#[derive(Serialize)]
struct TunnelHttpResponse<'a> {
    #[serde(rename = "request_id")]
    request_id: &'a str,
    #[serde(rename = "type")]
    r#type: &'a str,
    #[serde(rename = "is_last")]
    is_last: bool,
    #[serde(rename = "body_base64")]
    body_base64: String,
}

/// `open_ws_result` payload acking a stream open (ts:678-696).
#[derive(Serialize)]
struct OpenWsResultPayload<'a> {
    #[serde(rename = "stream_id")]
    stream_id: &'a str,
    success: bool,
    #[serde(rename = "error_code")]
    error_code: Option<&'a str>,
    #[serde(rename = "error_message")]
    error_message: Option<&'a str>,
}

#[derive(Serialize)]
struct OpenWsResult<'a> {
    #[serde(rename = "type")]
    r#type: &'a str,
    payload: OpenWsResultPayload<'a>,
}

/// Parsed raw HTTP/1.1 request (ts:89-94, 200-234).
pub struct ParsedHttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Error returned while forwarding an HTTP tunnel request; carries the HTTP
/// status to report back to the relay.
#[derive(Debug)]
struct ForwardError {
    status: u16,
    reason: String,
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "forward error {}: {}", self.status, self.reason)
    }
}

impl std::error::Error for ForwardError {}

/// Error raised when the relay rejects registration (ts:125, 456-459).
#[derive(Debug)]
struct RegistrationError(String);

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RegistrationError {}

// ----------------------------------------------------------------------------
// URL helpers
// ----------------------------------------------------------------------------

/// Build the public Remote Control URL (ts:183-198).
pub fn build_remote_control_url(
    device_id: &str,
    session_id: Option<&str>,
    relay_origin: &str,
) -> String {
    let mut url = match Url::parse(relay_origin) {
        Ok(u) => u,
        Err(_) => match Url::parse(REMOTE_CONTROL_RELAY_ORIGIN) {
            Ok(u) => u,
            Err(_) => return format!("https://code-rc.kimi.com/devices/{}/", pct(device_id)),
        },
    };
    let relay_path = url.path().trim_end_matches('/');
    let device_path = format!("{relay_path}/devices/{}", pct(device_id));
    let path = match session_id {
        None => format!("{device_path}/"),
        Some(s) => format!("{device_path}/sessions/{}", pct(s)),
    };
    url.set_path(&path);
    url.set_query(Some("rc=1&from=kimi_code_cli"));
    url.set_fragment(None);
    url.to_string()
}

/// Convert an http(s) origin + relay path into a wss/ws URL (ts:1039-1048).
pub fn relay_websocket_url(origin: &str, path: &str) -> Result<String> {
    let mut url = Url::parse(origin).map_err(|e| anyhow!("invalid relay origin: {e}"))?;
    url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
        .map_err(|_| anyhow!("cannot set ws scheme"))?;
    let relay_path = url.path().trim_end_matches('/');
    let (pathname, query) = match path.find('?') {
        Some(i) => (&path[..i], Some(&path[i + 1..])),
        None => (path, None),
    };
    url.set_path(&format!("{relay_path}{pathname}"));
    url.set_query(query);
    url.set_fragment(None);
    Ok(url.to_string())
}

/// Convert a local http(s) origin + path into a ws/wss URL (ts:1050-1058).
fn local_websocket_url(origin: &str, path: &str) -> Result<String> {
    let mut url = Url::parse(origin).map_err(|e| anyhow!("invalid local origin: {e}"))?;
    url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
        .map_err(|_| anyhow!("cannot set ws scheme"))?;
    let pathname = match path.find('?') {
        Some(i) => &path[..i],
        None => path,
    };
    url.set_path(pathname);
    let query = path.find('?').map(|i| &path[i + 1..]);
    url.set_query(query);
    url.set_fragment(None);
    Ok(url.to_string())
}

fn pct(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

// ----------------------------------------------------------------------------
// Reconnect backoff (ts:430-435)
// ----------------------------------------------------------------------------

/// Exponential-backoff reconnect delay, capped at [`MAX_RECONNECT_DELAY_MS`].
///
/// Mirrors `Math.min(MAX_RECONNECT_DELAY_MS, 1000 * 2 ** min(attempt - 1, 5))`
/// (ts:431-434). `reconnect_attempt` is the 1-based attempt count after a
/// failure: attempt 1 → 1s, 2 → 2s, … 6 → 32s (clamped to 30s), 7+ → 30s.
pub fn compute_reconnect_delay(reconnect_attempt: u32) -> u64 {
    let exp = (reconnect_attempt.saturating_sub(1)).min(5);
    let base = 1000u64.saturating_mul(1u64 << exp);
    base.min(MAX_RECONNECT_DELAY_MS)
}

// ----------------------------------------------------------------------------
// Header filtering (ts:236-254, 957-977)
// ----------------------------------------------------------------------------

/// Filter forward request headers, dropping [`BLOCKED_REQUEST_HEADERS`] and any
/// header named in a `Connection` directive, then re-injecting
/// `Authorization: Bearer <server_token>` (ts:236-254). Returns name/value
/// pairs (not the TS flat array).
pub fn filter_forward_request_headers(
    headers: &[(String, String)],
    server_token: &str,
) -> Vec<(String, String)> {
    let mut connection = std::collections::HashSet::new();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("connection") {
            for token in value.split(',') {
                connection.insert(token.trim().to_ascii_lowercase());
            }
        }
    }
    let mut out = Vec::new();
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if BLOCKED_REQUEST_HEADERS
            .iter()
            .any(|b| b.eq_ignore_ascii_case(&lower))
        {
            continue;
        }
        if connection.contains(&lower) {
            continue;
        }
        out.push((name.clone(), value.clone()));
    }
    out.push((
        "Authorization".to_string(),
        format!("Bearer {server_token}"),
    ));
    out
}

/// Filter response headers before tunneling back, dropping
/// [`BLOCKED_RESPONSE_HEADERS`] and any `Connection`-listed header (ts:957-977).
fn filter_response_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    let mut connection = std::collections::HashSet::new();
    for (name, value) in headers.iter() {
        if name.as_str().eq_ignore_ascii_case("connection")
            && let Ok(v) = value.to_str()
        {
            for token in v.split(',') {
                connection.insert(token.trim().to_ascii_lowercase());
            }
        }
    }
    let mut out = Vec::new();
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if BLOCKED_RESPONSE_HEADERS
            .iter()
            .any(|b| b.eq_ignore_ascii_case(&lower))
        {
            continue;
        }
        if connection.contains(&lower) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            out.push((name.as_str().to_string(), v.to_string()));
        }
    }
    out
}

// ----------------------------------------------------------------------------
// Raw HTTP request parsing (ts:200-234)
// ----------------------------------------------------------------------------

fn request_line_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^([!#$%&'*+.^_`|~0-9A-Za-z-]+) (/[^\x00-\x20]*) HTTP/1\.[01]$").unwrap()
    })
}

/// Parse a raw HTTP/1.1 request buffer into method/path/headers/body (ts:200-234).
pub fn parse_raw_http_request(raw: &[u8]) -> Result<ParsedHttpRequest> {
    let sep = memchr::memmem::find(raw, b"\r\n\r\n")
        .ok_or_else(|| anyhow!("invalid HTTP request headers"))?;
    if sep > MAX_HTTP_HEADER_BYTES {
        bail!("invalid HTTP request headers");
    }
    let head =
        std::str::from_utf8(&raw[..sep]).map_err(|_| anyhow!("invalid HTTP request headers"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("invalid HTTP request line"))?;
    let caps = request_line_re()
        .captures(request_line)
        .ok_or_else(|| anyhow!("invalid HTTP request line"))?;
    let method = caps.get(1).unwrap().as_str().to_string();
    let path = caps.get(2).unwrap().as_str().to_string();
    if path.starts_with("//") {
        bail!("invalid HTTP request line");
    }
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let colon = line
            .find(':')
            .ok_or_else(|| anyhow!("invalid HTTP request header"))?;
        if colon == 0 {
            bail!("invalid HTTP request header");
        }
        let name = line[..colon].trim().to_string();
        let value = line[colon + 1..].trim().to_string();
        headers.push((name, value));
    }
    let body = raw[sep + 4..].to_vec();
    Ok(ParsedHttpRequest {
        method,
        path,
        headers,
        body,
    })
}

// ----------------------------------------------------------------------------
// Register message builder (ts:443-454)
// ----------------------------------------------------------------------------

/// Build the `register` management message (ts:443-454).
pub fn build_register_message(opts: &RemoteControlOptions) -> RegisterMessage<'_> {
    RegisterMessage {
        r#type: "register",
        payload: RegisterPayload {
            device_id: &opts.device_id,
            alias: &opts.device_name,
            platform: &opts.platform,
            client_version: &opts.client_version,
            local_base_url: &opts.local_base_url,
        },
    }
}

// ----------------------------------------------------------------------------
// WebSocket connect (ts:747-824)
// ----------------------------------------------------------------------------

fn is_ws_protocol_token(token: &str) -> bool {
    !token.is_empty()
        && token.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '!' | '#'
                        | '$'
                        | '%'
                        | '&'
                        | '\''
                        | '*'
                        | '+'
                        | '-'
                        | '.'
                        | '^'
                        | '_'
                        | '`'
                        | '|'
                        | '~'
                )
        })
}

/// Open a relay WebSocket, sending the bearer token via the
/// `kimi-code.bearer.<token>` subprotocol when valid, else an `Authorization`
/// header (ts:747-768).
async fn connect_relay_url(
    url: &str,
    token: &str,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let mut builder = ws_http::Request::builder().uri(url);
    if is_ws_protocol_token(token) {
        builder = builder.header(
            "Sec-WebSocket-Protocol",
            format!("kimi-code.bearer.{token}"),
        );
    } else {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let request = builder
        .body(())
        .map_err(|e| anyhow!("invalid ws request: {e}"))?;
    let (stream, _resp) = tokio_tungstenite::connect_async(request).await?;
    Ok(stream)
}

async fn connect_relay(
    origin: &str,
    path: &str,
    token: &str,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let url = relay_websocket_url(origin, path)?;
    connect_relay_url(&url, token).await
}

/// Pump outgoing messages from a channel into a WS write half until the channel
/// closes or the socket errors.
async fn ws_writer<S>(
    mut write: futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    mut rx: mpsc::Receiver<Message>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    while let Some(msg) = rx.recv().await {
        if write.send(msg).await.is_err() {
            break;
        }
    }
}

// ----------------------------------------------------------------------------
// State machine
// ----------------------------------------------------------------------------

/// Shared, mutable state for one connection cycle.
struct CycleCtx<'a> {
    opts: &'a Arc<RemoteControlOptions>,
    mgmt_tx: &'a mpsc::Sender<Message>,
    http_tx: &'a mpsc::Sender<Message>,
    status: &'a Arc<Mutex<RemoteControlStatusWire>>,
    streams: &'a Arc<Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    pending: &'a Arc<Mutex<HashMap<String, Vec<u8>>>>,
    shutdown: &'a tokio_util::sync::CancellationToken,
    reconnect_immediately: &'a Arc<AtomicBool>,
}

/// Top-level reconnect loop (ts:404-437).
async fn run_state_machine(
    opts: Arc<RemoteControlOptions>,
    status: Arc<Mutex<RemoteControlStatusWire>>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let connected = Arc::new(AtomicBool::new(false));
    let reconnect_immediately = Arc::new(AtomicBool::new(false));
    let mut reconnect_attempt: u32 = 0;

    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let result = serve_cycle(
            &opts,
            &status,
            &shutdown,
            &connected,
            &reconnect_immediately,
        )
        .await;
        match result {
            Ok(()) => {
                // Clean end (e.g. `disconnect` with server_shutting_down): reconnect.
            }
            Err(e) => {
                if !connected.load(Ordering::SeqCst)
                    && e.downcast_ref::<RegistrationError>().is_some()
                {
                    let mut s = status.lock().unwrap();
                    s.enabled = true;
                    s.state = "error".to_string();
                    s.error = Some(e.to_string());
                    break;
                }
                tracing::warn!("remote control cycle ended: {e}");
            }
        }
        if shutdown.is_cancelled() {
            break;
        }
        if reconnect_immediately.load(Ordering::SeqCst) {
            reconnect_immediately.store(false, Ordering::SeqCst);
            reconnect_attempt = 0;
        } else {
            reconnect_attempt += 1;
            let delay = compute_reconnect_delay(reconnect_attempt);
            {
                let mut s = status.lock().unwrap();
                if s.state != "error" {
                    s.state = "relay_disconnected".to_string();
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                _ = shutdown.cancelled() => break,
            }
        }
    }

    let mut s = status.lock().unwrap();
    *s = RemoteControlStatusWire::off();
}

/// One connection cycle: register → connect http tunnel → serve until a socket
/// closes (ts:439-489). Returns `Ok(())` for a clean stop that should reconnect
/// immediately, `Err` for a failure / drop that should back off.
async fn serve_cycle(
    opts: &Arc<RemoteControlOptions>,
    status: &Arc<Mutex<RemoteControlStatusWire>>,
    shutdown: &tokio_util::sync::CancellationToken,
    connected: &Arc<AtomicBool>,
    reconnect_immediately: &Arc<AtomicBool>,
) -> Result<()> {
    // 1. Management socket + register (ts:440-463).
    let mgmt = connect_relay(&opts.relay_origin, "/v1/remote/create", &opts.refresh_token).await?;
    let (mgmt_write, mut mgmt_read) = mgmt.split();
    let (mgmt_tx, mgmt_rx) = mpsc::channel::<Message>(32);
    tokio::spawn(ws_writer(mgmt_write, mgmt_rx));

    let register = build_register_message(opts);
    mgmt_tx
        .send(Message::Text(serde_json::to_string(&register)?))
        .await
        .map_err(|_| anyhow!("management socket closed before register"))?;

    let registration = tokio::time::timeout(
        Duration::from_millis(REGISTER_TIMEOUT_MS),
        next_json(&mut mgmt_read),
    )
    .await
    .map_err(|_| anyhow!("registration timed out"))??;
    match registration.get("type").and_then(|v| v.as_str()) {
        Some("register_nak") => {
            let code = registration
                .get("payload")
                .and_then(|p| p.get("error_code"))
                .and_then(|v| v.as_str())
                .unwrap_or("REGISTRATION_REJECTED");
            let message = registration
                .get("payload")
                .and_then(|p| p.get("error_message"))
                .and_then(|v| v.as_str())
                .unwrap_or("registration rejected");
            return Err(RegistrationError(format!(
                "Remote Control registration failed ({code}): {message}"
            ))
            .into());
        }
        Some("register_ack") => {}
        other => {
            return Err(anyhow!(
                "expected register_ack, received {}",
                other.unwrap_or("<none>")
            ));
        }
    }

    // 2. HTTP tunnel socket (ts:466-473).
    let http = connect_relay(
        &opts.relay_origin,
        &format!("/v1/remote/http?device_id={}", pct(&opts.device_id)),
        &opts.refresh_token,
    )
    .await?;
    let (http_write, mut http_read) = http.split();
    let (http_tx, http_rx) = mpsc::channel::<Message>(32);
    tokio::spawn(ws_writer(http_write, http_rx));

    connected.store(true, Ordering::SeqCst);
    {
        let mut s = status.lock().unwrap();
        s.enabled = true;
        s.state = "relay_connected".to_string();
        s.device_id = Some(opts.device_id.clone());
        s.device_name = Some(opts.device_name.clone());
        s.error = None;
    }

    let streams: Arc<Mutex<HashMap<String, tokio_util::sync::CancellationToken>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let pending: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));

    let ctx = CycleCtx {
        opts,
        mgmt_tx: &mgmt_tx,
        http_tx: &http_tx,
        status,
        streams: &streams,
        pending: &pending,
        shutdown,
        reconnect_immediately,
    };

    // 3. Serve until either socket closes or shutdown (ts:487-488).
    let mut last_activity = Instant::now();
    let mut ping_interval = tokio::time::interval(Duration::from_millis(opts.ping_interval_ms));
    let silence = Duration::from_millis(opts.silence_timeout_ms);
    loop {
        tokio::select! {
            m = mgmt_read.next() => {
                match m {
                    Some(Ok(msg)) => {
                        last_activity = Instant::now();
                        handle_mgmt(msg, &ctx).await?;
                    }
                    Some(Err(e)) => return Err(anyhow!("management socket error: {e}")),
                    None => return Err(anyhow!("management socket closed")),
                }
            }
            m = http_read.next() => {
                match m {
                    Some(Ok(msg)) => {
                        last_activity = Instant::now();
                        handle_http(msg, &ctx).await?;
                    }
                    Some(Err(e)) => return Err(anyhow!("http tunnel socket error: {e}")),
                    None => return Err(anyhow!("http tunnel socket closed")),
                }
            }
            _ = ping_interval.tick() => {
                let _ = mgmt_tx.send(Message::Ping(Vec::new())).await;
                let _ = http_tx.send(Message::Ping(Vec::new())).await;
            }
            _ = shutdown.cancelled() => {
                let _ = mgmt_tx
                    .send(Message::Text(
                        serde_json::json!({"type":"disconnect","payload":{"reason":"local_server_stopped"}})
                            .to_string(),
                    ))
                    .await;
                return Ok(());
            }
        }
        if last_activity.elapsed() > silence {
            return Err(anyhow!(
                "relay connection silent for {}ms; reconnecting",
                opts.silence_timeout_ms
            ));
        }
    }
}

/// Read the next text/binary WS message and parse it as JSON (ts:898-903).
async fn next_json<S>(
    read: &mut futures_util::stream::SplitStream<WebSocketStream<S>>,
) -> Result<JsonValue>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    loop {
        match read.next().await {
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(&t)?),
            Some(Ok(Message::Binary(b))) => {
                return Ok(serde_json::from_str(std::str::from_utf8(&b)?)?);
            }
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) => return Err(anyhow!("socket closed")),
            Some(Ok(Message::Frame(_))) => continue,
            Some(Err(e)) => return Err(anyhow!("socket error: {e}")),
            None => return Err(anyhow!("socket closed")),
        }
    }
}

/// Handle a management-socket message (ts:527-550).
async fn handle_mgmt(msg: Message, ctx: &CycleCtx<'_>) -> Result<()> {
    let json = match msg {
        Message::Text(t) => serde_json::from_str::<JsonValue>(&t)?,
        Message::Binary(b) => serde_json::from_str::<JsonValue>(std::str::from_utf8(&b)?)?,
        _ => return Ok(()),
    };
    let r#type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match r#type {
        "open_ws" => {
            let payload = json.get("payload").cloned().unwrap_or(JsonValue::Null);
            let stream_id = payload
                .get("stream_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let path = payload
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            match (stream_id, path) {
                (Some(stream_id), Some(path))
                    if path.starts_with('/') && !path.starts_with("//") =>
                {
                    let token = tokio_util::sync::CancellationToken::new();
                    ctx.streams
                        .lock()
                        .unwrap()
                        .insert(stream_id.clone(), token.clone());
                    let opts = ctx.opts.clone();
                    let mgmt_tx = ctx.mgmt_tx.clone();
                    tokio::spawn(bridge_stream(
                        stream_id,
                        path,
                        opts,
                        mgmt_tx,
                        token,
                        ctx.status.clone(),
                    ));
                    let mut s = ctx.status.lock().unwrap();
                    s.state = "device_connected".to_string();
                }
                (Some(stream_id), _) => {
                    send_open_ws_result(
                        ctx.mgmt_tx,
                        &stream_id,
                        false,
                        Some("LOCAL_WS_FAILED"),
                        Some("invalid local WebSocket path"),
                    )
                    .await?;
                }
                _ => {}
            }
        }
        "close_ws" => {
            if let Some(stream_id) = json
                .get("payload")
                .and_then(|p| p.get("stream_id"))
                .and_then(|v| v.as_str())
            {
                if let Some(token) = ctx.streams.lock().unwrap().remove(stream_id) {
                    token.cancel();
                }
                let mut s = ctx.status.lock().unwrap();
                s.state = "device_disconnected".to_string();
            }
        }
        "disconnect" => {
            let reason = json
                .get("payload")
                .and_then(|p| p.get("reason"))
                .and_then(|v| v.as_str());
            match reason {
                Some("user_requested") => {
                    ctx.shutdown.clone().cancel();
                }
                Some("server_shutting_down") => {
                    ctx.reconnect_immediately.store(true, Ordering::SeqCst);
                    return Ok(());
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(())
}

/// Handle an HTTP-tunnel socket message: accumulate chunks, forward on last
/// (ts:552-604).
async fn handle_http(msg: Message, ctx: &CycleCtx<'_>) -> Result<()> {
    let text = match msg {
        Message::Text(t) => t,
        Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
        _ => return Ok(()),
    };
    // Non-request frames (or malformed JSON) are ignored; only `type: "request"`
    // tunnel messages carry a forwardable HTTP payload (ts:552-559).
    let parsed: TunnelHttpRequest = match serde_json::from_str::<TunnelHttpRequest>(&text) {
        Ok(v) if v.r#type == "request" => v,
        _ => return Ok(()),
    };
    let request_id = parsed.request_id;
    let is_last = parsed.is_last;
    let body_b64 = parsed.body_base64;

    let chunk = base64::engine::general_purpose::STANDARD
        .decode(body_b64)
        .map_err(|_| anyhow!("invalid tunnel request base64"))?;
    if !is_last {
        // N.B. the pending-bytes mutex guard must NOT be held across the
        // `.await` below (a `std::sync::MutexGuard` is `!Send`, which would
        // make this future non-`Send` and break `tokio::spawn`).
        let oversized = {
            let mut pending = ctx.pending.lock().unwrap();
            let entry = pending.entry(request_id.clone()).or_default();
            entry.extend_from_slice(&chunk);
            let over = entry.len() > MAX_HTTP_REQUEST_BYTES;
            if over {
                pending.remove(&request_id);
            }
            over
        };
        if oversized {
            send_error_response(ctx.http_tx, &request_id, 400).await?;
        }
        return Ok(());
    }

    let mut raw = ctx
        .pending
        .lock()
        .unwrap()
        .remove(&request_id)
        .unwrap_or_default();
    raw.extend_from_slice(&chunk);

    match forward_http_request(ctx.opts, &raw, ctx.http_tx, &request_id).await {
        Ok(()) => {}
        Err(e) => tracing::warn!("remote control http forward failed: {e}"),
    }
    Ok(())
}

/// Forward one raw HTTP request to the local loopback service and send the
/// response back through the tunnel (ts:589-604, 905-955).
async fn forward_http_request(
    opts: &Arc<RemoteControlOptions>,
    raw: &[u8],
    http_tx: &mpsc::Sender<Message>,
    request_id: &str,
) -> Result<()> {
    let response_bytes = match do_forward(opts, raw).await {
        Ok(b) => b,
        Err(e) => build_error_response(e.status),
    };
    let out = TunnelHttpResponse {
        request_id,
        r#type: "response",
        is_last: true,
        body_base64: base64::engine::general_purpose::STANDARD.encode(&response_bytes),
    };
    http_tx
        .send(Message::Text(serde_json::to_string(&out)?))
        .await
        .map_err(|_| anyhow!("http tunnel socket closed"))?;
    Ok(())
}

/// Perform the local HTTP request; on any error build an error response.
async fn do_forward(
    opts: &Arc<RemoteControlOptions>,
    raw: &[u8],
) -> std::result::Result<Vec<u8>, ForwardError> {
    let parsed = parse_raw_http_request(raw).map_err(|e| ForwardError {
        status: 400,
        reason: e.to_string(),
    })?;

    let url = format!(
        "{}{}",
        opts.local_base_url.trim_end_matches('/'),
        parsed.path
    );
    let method = std::str::FromStr::from_str(&parsed.method).map_err(|_| ForwardError {
        status: 400,
        reason: format!("invalid method {}", parsed.method),
    })?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(HTTP_REQUEST_TIMEOUT_MS))
        .build()
        .map_err(|e| ForwardError {
            status: 502,
            reason: e.to_string(),
        })?;

    let mut req = client.request(method, &url);
    let filtered = filter_forward_request_headers(&parsed.headers, &opts.local_server_token);
    for (k, v) in filtered {
        req = req.header(&k, &v);
    }
    if let Ok(parsed_url) = Url::parse(&opts.local_base_url)
        && let Some(h) = parsed_url.host().map(|h| h.to_string())
    {
        req = req.header("Host", h);
    }
    req = req.body(parsed.body);

    let resp = req.send().await.map_err(|e| ForwardError {
        status: 502,
        reason: e.to_string(),
    })?;

    let status = resp.status();
    let headers = filter_response_headers(resp.headers());
    let body = resp.bytes().await.map_err(|e| ForwardError {
        status: 502,
        reason: e.to_string(),
    })?;

    Ok(build_http_response(status, &headers, &body))
}

/// Build a raw `HTTP/1.1` response buffer (ts:940-947).
fn build_http_response(
    status: reqwest::StatusCode,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let reason = status.canonical_reason().unwrap_or("");
    let mut out = format!("HTTP/1.1 {} {}\r\n", status.as_u16(), reason).into_bytes();
    for (k, v) in headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body);
    out
}

/// Build an empty error response (ts:1068-1071).
fn build_error_response(status: u16) -> Vec<u8> {
    let reason = if status == 400 {
        "Bad Request"
    } else {
        "Bad Gateway"
    };
    format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\n\r\n").into_bytes()
}

async fn send_error_response(
    http_tx: &mpsc::Sender<Message>,
    request_id: &str,
    status: u16,
) -> Result<()> {
    let out = TunnelHttpResponse {
        request_id,
        r#type: "response",
        is_last: true,
        body_base64: base64::engine::general_purpose::STANDARD.encode(build_error_response(status)),
    };
    http_tx
        .send(Message::Text(serde_json::to_string(&out)?))
        .await
        .map_err(|_| anyhow!("http tunnel socket closed"))?;
    Ok(())
}

async fn send_open_ws_result(
    mgmt_tx: &mpsc::Sender<Message>,
    stream_id: &str,
    success: bool,
    error_code: Option<&str>,
    error_message: Option<&str>,
) -> Result<()> {
    let msg = OpenWsResult {
        r#type: "open_ws_result",
        payload: OpenWsResultPayload {
            stream_id,
            success,
            error_code,
            error_message,
        },
    };
    mgmt_tx
        .send(Message::Text(serde_json::to_string(&msg)?))
        .await
        .map_err(|_| anyhow!("management socket closed"))?;
    Ok(())
}

// ----------------------------------------------------------------------------
// WebSocket stream bridging (ts:618-705, 995-1026)
// ----------------------------------------------------------------------------

/// Open a local WS and a relay tunnel WS for `stream_id`, bridge them, then ack
/// with `open_ws_result` (ts:618-676).
async fn bridge_stream(
    stream_id: String,
    path: String,
    opts: Arc<RemoteControlOptions>,
    mgmt_tx: mpsc::Sender<Message>,
    token: tokio_util::sync::CancellationToken,
    status: Arc<Mutex<RemoteControlStatusWire>>,
) {
    let result: Result<()> = async {
        let local_url = local_websocket_url(&opts.local_base_url, &path)?;
        let tunnel_url = relay_websocket_url(
            &opts.relay_origin,
            &format!("/v1/remote/stream/{}", pct(&stream_id)),
        )?;
        let local = connect_relay_url(&local_url, &opts.local_server_token).await?;
        let tunnel = connect_relay_url(&tunnel_url, &opts.refresh_token).await?;
        bridge_two(local, tunnel, token).await?;
        Ok(())
    }
    .await;

    match &result {
        Ok(()) => {
            let _ = send_open_ws_result(&mgmt_tx, &stream_id, true, None, None).await;
        }
        Err(e) => {
            let code = if e.to_string().contains("LOCAL_WS") {
                "LOCAL_WS_FAILED"
            } else {
                "TUNNEL_STREAM_FAILED"
            };
            let _ = send_open_ws_result(
                &mgmt_tx,
                &stream_id,
                false,
                Some(code),
                Some(&e.to_string()),
            )
            .await;
        }
    }
    let mut s = status.lock().unwrap();
    s.state = "device_disconnected".to_string();
}

/// Bridge two WebSocket streams in both directions until either closes or the
/// token is cancelled (ts:995-1026).
async fn bridge_two<A, B>(
    a: WebSocketStream<A>,
    b: WebSocketStream<B>,
    token: tokio_util::sync::CancellationToken,
) -> Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    B: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (a_write, mut a_read) = a.split();
    let (b_write, mut b_read) = b.split();
    let (a_tx, a_rx) = mpsc::channel::<Message>(32);
    let (b_tx, b_rx) = mpsc::channel::<Message>(32);
    let w1 = tokio::spawn(ws_writer(a_write, a_rx));
    let w2 = tokio::spawn(ws_writer(b_write, b_rx));

    loop {
        tokio::select! {
            m = a_read.next() => match m {
                Some(Ok(msg)) => {
                    if matches!(msg, Message::Close(_)) { break; }
                    let _ = b_tx.send(msg).await;
                }
                _ => break,
            },
            m = b_read.next() => match m {
                Some(Ok(msg)) => {
                    if matches!(msg, Message::Close(_)) { break; }
                    let _ = a_tx.send(msg).await;
                }
                _ => break,
            },
            _ = token.cancelled() => break,
        }
    }
    drop(a_tx);
    drop(b_tx);
    let _ = w1.await;
    let _ = w2.await;
    Ok(())
}

// ----------------------------------------------------------------------------
// Device management cloud client (v2 remote-control/src/device-client.ts)
// ----------------------------------------------------------------------------

const DEVICE_REQUEST_TIMEOUT_MS: u64 = 15_000;
const UNEXPECTED_LIST_RESPONSE: &str =
    "Remote Control device list returned an unexpected response.";

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteControlDeviceStatus {
    Online,
    Offline,
}

/// The documented device fields. Unknown cloud-only fields are discarded by
/// deserialization, matching the explicit projection in the TS client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteControlDevice {
    pub device_id: String,
    pub alias: String,
    pub status: RemoteControlDeviceStatus,
    pub platform: String,
    pub client_version: String,
    pub local_base_url: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_remote_access_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteControlDeviceList {
    pub devices: Vec<RemoteControlDevice>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_devices: Option<serde_json::Number>,
}

fn deserialize_present_number<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<serde_json::Number>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // `Option<Number>` alone would accept an explicit null as an absent value;
    // upstream accepts a missing key but rejects every present non-number.
    serde_json::Number::deserialize(deserializer).map(Some)
}

/// Credential slot and relay selected from the same configured OAuth ref.
pub struct RemoteControlDeviceAuth {
    pub relay_origin: String,
    pub storage_name: String,
    /// The standalone Web UI's historical `kimi.json` alias is valid only when
    /// config has no explicit slot; an explicit custom/global slot never falls
    /// back to a different account.
    pub allow_managed_aliases: bool,
}

/// Relay HTTP session client. Cookies are keyed by relay and credential, and a
/// `OnceCell` per key coalesces concurrent exchanges just like the TS promise
/// map. A cloud 401 drops that cell, exchanges once, and retries once.
pub struct RemoteControlDeviceClient {
    client: reqwest::Client,
    sessions: Arc<Mutex<HashMap<String, Arc<tokio::sync::OnceCell<String>>>>>,
}

impl Default for RemoteControlDeviceClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteControlDeviceClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_millis(DEVICE_REQUEST_TIMEOUT_MS))
                .build()
                .unwrap_or_default(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn list_devices(
        &self,
        oauth: &crate::server::oauth::OAuthManager,
        auth: &RemoteControlDeviceAuth,
    ) -> std::result::Result<RemoteControlDeviceList, String> {
        let response = self
            .request(
                oauth,
                auth,
                reqwest::Method::GET,
                "/v1/remote/devices",
                None,
            )
            .await?;
        let body = response
            .json::<JsonValue>()
            .await
            .map_err(|_| "Remote Control device list returned invalid JSON.".to_string())?;
        serde_json::from_value(body).map_err(|_| UNEXPECTED_LIST_RESPONSE.to_string())
    }

    /// `PATCH /v1/remote/devices/{id}` with the trimmed alias (ts `renameDevice`).
    pub async fn rename_device(
        &self,
        oauth: &crate::server::oauth::OAuthManager,
        auth: &RemoteControlDeviceAuth,
        device_id: &str,
        alias: &str,
    ) -> std::result::Result<(), String> {
        self.request(
            oauth,
            auth,
            reqwest::Method::PATCH,
            &format!("/v1/remote/devices/{}", pct(device_id)),
            Some(&serde_json::json!({ "alias": alias })),
        )
        .await?;
        Ok(())
    }

    /// `POST /v1/remote/devices/{id}/deactivate` (ts `deactivateDevice`).
    pub async fn deactivate_device(
        &self,
        oauth: &crate::server::oauth::OAuthManager,
        auth: &RemoteControlDeviceAuth,
        device_id: &str,
    ) -> std::result::Result<(), String> {
        self.request(
            oauth,
            auth,
            reqwest::Method::POST,
            &format!("/v1/remote/devices/{}/deactivate", pct(device_id)),
            None,
        )
        .await?;
        Ok(())
    }

    /// Drop every cached relay session, so the next call exchanges again with
    /// whatever credential is current (ts `clear`, called around logout).
    pub fn clear_sessions(&self) {
        self.sessions.lock().unwrap().clear();
    }

    pub async fn delete_device(
        &self,
        oauth: &crate::server::oauth::OAuthManager,
        auth: &RemoteControlDeviceAuth,
        device_id: &str,
    ) -> std::result::Result<(), String> {
        self.request(
            oauth,
            auth,
            reqwest::Method::DELETE,
            &format!("/v1/remote/devices/{}", pct(device_id)),
            None,
        )
        .await?;
        Ok(())
    }

    fn context(
        &self,
        oauth: &crate::server::oauth::OAuthManager,
        auth: &RemoteControlDeviceAuth,
    ) -> std::result::Result<DeviceContext, String> {
        let token =
            oauth.managed_remote_control_token(&auth.storage_name, auth.allow_managed_aliases)?;
        let relay_origin = auth.relay_origin.trim_end_matches('/').to_string();
        let cache_key = format!(
            "{}\0{}\0{}",
            token.storage_name, relay_origin, token.refresh_token
        );
        Ok(DeviceContext {
            relay_origin,
            cache_key,
            token,
        })
    }

    async fn request(
        &self,
        oauth: &crate::server::oauth::OAuthManager,
        auth: &RemoteControlDeviceAuth,
        method: reqwest::Method,
        path: &str,
        body: Option<&JsonValue>,
    ) -> std::result::Result<reqwest::Response, String> {
        let context = self.context(oauth, auth)?;
        let cookie = self.session_cookie(&context, false).await?;
        let mut response = self
            .send(&context, method.clone(), path, body, &cookie)
            .await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let cookie = self.session_cookie(&context, true).await?;
            response = self.send(&context, method, path, body, &cookie).await?;
        }
        if !response.status().is_success() {
            return Err(
                device_response_error("Remote Control device request failed", response).await,
            );
        }
        Ok(response)
    }

    async fn send(
        &self,
        context: &DeviceContext,
        method: reqwest::Method,
        path: &str,
        body: Option<&JsonValue>,
        cookie: &str,
    ) -> std::result::Result<reqwest::Response, String> {
        let url = relay_http_url(&context.relay_origin, path)
            .map_err(|error| format!("Remote Control device request failed: {error}"))?;
        let mut request = self
            .client
            .request(method, url)
            .header(reqwest::header::COOKIE, cookie);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(body);
        }
        request
            .send()
            .await
            .map_err(|error| format!("Remote Control device request failed: {error}"))
    }

    async fn session_cookie(
        &self,
        context: &DeviceContext,
        refresh: bool,
    ) -> std::result::Result<String, String> {
        if refresh {
            self.sessions.lock().unwrap().remove(&context.cache_key);
        }
        let cell = {
            let mut sessions = self.sessions.lock().unwrap();
            sessions
                .entry(context.cache_key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
                .clone()
        };
        match cell.get_or_try_init(|| self.exchange(context)).await {
            Ok(cookie) => Ok(cookie.clone()),
            Err(error) => {
                let mut sessions = self.sessions.lock().unwrap();
                if sessions
                    .get(&context.cache_key)
                    .is_some_and(|current| Arc::ptr_eq(current, &cell))
                {
                    sessions.remove(&context.cache_key);
                }
                Err(error.clone())
            }
        }
    }

    async fn exchange(&self, context: &DeviceContext) -> std::result::Result<String, String> {
        let url = relay_http_url(&context.relay_origin, "/auth/exchange")
            .map_err(|error| format!("Remote Control session exchange failed: {error}"))?;
        let response = self
            .client
            .post(url)
            .json(&serde_json::json!({
                "access_token": context.token.access_token,
                "refresh_token": context.token.refresh_token,
                "expires_in": context.token.expires_in,
            }))
            .send()
            .await
            .map_err(|error| format!("Remote Control session exchange failed: {error}"))?;
        if response.status() != reqwest::StatusCode::NO_CONTENT {
            return Err(
                device_response_error("Remote Control session exchange failed", response).await,
            );
        }
        let cookie = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        if cookie.is_empty() {
            return Err(
                "Remote Control session exchange did not return a session cookie.".to_string(),
            );
        }
        Ok(cookie)
    }
}

struct DeviceContext {
    relay_origin: String,
    cache_key: String,
    token: crate::server::oauth::ManagedToken,
}

fn relay_http_url(origin: &str, path: &str) -> std::result::Result<String, String> {
    let mut url = Url::parse(origin).map_err(|error| error.to_string())?;
    let relay_path = url.path().trim_end_matches('/').to_string();
    url.set_path(&format!("{relay_path}{path}"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

async fn device_response_error(action: &str, response: reqwest::Response) -> String {
    let status = response.status().as_u16();
    let detail = response
        .json::<JsonValue>()
        .await
        .ok()
        .and_then(|body| cloud_error_message(&body));
    match detail {
        Some(detail) => format!("{action} (HTTP {status}): {detail}"),
        None => format!("{action} (HTTP {status})."),
    }
}

fn cloud_error_message(value: &JsonValue) -> Option<String> {
    let raw = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(JsonValue::as_str)
        .or_else(|| value.get("message").and_then(JsonValue::as_str))?;
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let message = normalized.chars().take(300).collect::<String>();
    (!message.is_empty()).then_some(message)
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backoff_sequence_matches_ts() {
        // ts:431-434 — Math.min(30000, 1000 * 2 ** min(attempt-1, 5))
        assert_eq!(compute_reconnect_delay(1), 1000);
        assert_eq!(compute_reconnect_delay(2), 2000);
        assert_eq!(compute_reconnect_delay(3), 4000);
        assert_eq!(compute_reconnect_delay(4), 8000);
        assert_eq!(compute_reconnect_delay(5), 16_000);
        assert_eq!(compute_reconnect_delay(6), 30_000); // 32_000 clamped
        assert_eq!(compute_reconnect_delay(7), 30_000);
        assert_eq!(compute_reconnect_delay(20), 30_000);
    }

    #[test]
    fn forward_header_filtering_drops_blocked_and_reinjects_auth() {
        let headers = vec![
            ("Authorization".to_string(), "old".to_string()),
            ("Cookie".to_string(), "sess=1".to_string()),
            ("X-Custom".to_string(), "keep".to_string()),
            ("Connection".to_string(), "close, X-Foo".to_string()),
            ("X-Foo".to_string(), "drop-me".to_string()),
            ("Host".to_string(), "ignored".to_string()),
        ];
        let out = filter_forward_request_headers(&headers, "tok");

        // Blocked headers must be gone.
        assert!(!out.iter().any(|(k, _)| k.eq_ignore_ascii_case("cookie")));
        assert!(!out.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")));
        // Connection-listed header removed.
        assert!(!out.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-foo")));
        // Non-blocked custom header preserved.
        assert!(
            out.iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-custom") && v == "keep")
        );
        // Exactly one Authorization, the reinjected bearer token.
        let auths: Vec<&String> = out
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v)
            .collect();
        assert_eq!(auths.len(), 1);
        assert_eq!(auths[0], "Bearer tok");
    }

    #[test]
    fn register_message_shape_matches_ts() {
        // ts:443-454
        let opts = RemoteControlOptions {
            device_id: "dev-123".to_string(),
            device_name: "my-host".to_string(),
            platform: "win32".to_string(),
            client_version: "kimi-code/1.2.3".to_string(),
            local_base_url: "http://127.0.0.1:3461".to_string(),
            ..Default::default()
        };
        let msg = build_register_message(&opts);
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "register");
        let p = &json["payload"];
        assert_eq!(p["device_id"], "dev-123");
        assert_eq!(p["alias"], "my-host");
        assert_eq!(p["platform"], "win32");
        assert_eq!(p["client_version"], "kimi-code/1.2.3");
        assert_eq!(p["local_base_url"], "http://127.0.0.1:3461");
    }

    #[test]
    fn parse_raw_http_request_basics() {
        let raw =
            b"POST /api/v1/foo?x=1 HTTP/1.1\r\nHost: example\r\nContent-Length: 5\r\nX-Test: a\r\n\r\nhello";
        let parsed = parse_raw_http_request(raw).unwrap();
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/api/v1/foo?x=1");
        assert!(
            parsed
                .headers
                .iter()
                .any(|(k, v)| k == "Host" && v == "example")
        );
        assert!(
            parsed
                .headers
                .iter()
                .any(|(k, v)| k == "X-Test" && v == "a")
        );
        assert_eq!(parsed.body, b"hello");

        // Invalid: no header terminator.
        assert!(parse_raw_http_request(b"GET / HTTP/1.1").is_err());
        // Invalid: double-slash path.
        assert!(parse_raw_http_request(b"GET //evil HTTP/1.1\r\n\r\n").is_err());
    }

    /// v2 #3969 precedence: the env override wins over the region-resolved
    /// fallback, and an explicitly passed origin wins over both (the
    /// `options.relayOrigin ?? resolveRemoteControlRelayOrigin(env, auth…)`
    /// shape).
    #[test]
    fn relay_origin_precedence_is_env_then_region_then_default() {
        let env = std::env::var(REMOTE_CONTROL_RELAY_URL_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_string());
        assert_eq!(
            resolve_remote_control_relay_origin("https://code-rc.kimi.ai"),
            env.clone()
                .unwrap_or_else(|| "https://code-rc.kimi.ai".to_string()),
        );
        assert_eq!(
            resolve_remote_control_relay_origin(REMOTE_CONTROL_RELAY_ORIGIN),
            env.unwrap_or_else(|| REMOTE_CONTROL_RELAY_ORIGIN.to_string()),
        );
        // An empty fallback falls back to the built-in default rather than
        // producing an origin the URL builder cannot parse.
        assert_eq!(
            resolve_remote_control_relay_origin("  "),
            std::env::var(REMOTE_CONTROL_RELAY_URL_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_string())
                .unwrap_or_else(|| REMOTE_CONTROL_RELAY_ORIGIN.to_string()),
        );
    }

    #[test]
    fn relay_websocket_url_converts_scheme_and_path() {
        let url = relay_websocket_url("https://code-rc.kimi.com", "/v1/remote/create").unwrap();
        assert!(url.starts_with("wss://code-rc.kimi.com/v1/remote/create"));
        let url2 = relay_websocket_url("https://code-rc.kimi.com", "/v1/remote/http?device_id=abc")
            .unwrap();
        assert!(url2.contains("wss://"));
        assert!(url2.contains("device_id=abc"));
    }

    #[tokio::test]
    async fn state_machine_exits_cleanly_when_shutdown_cancelled() {
        // No network is touched: run_state_machine checks `shutdown` before the
        // first cycle, then resets status to `off` (ts:422-425).
        let opts = Arc::new(RemoteControlOptions::default());
        let status = Arc::new(Mutex::new(RemoteControlStatusWire::off()));
        let shutdown = tokio_util::sync::CancellationToken::new();
        shutdown.cancel();
        run_state_machine(opts, status.clone(), shutdown).await;
        let s = status.lock().unwrap();
        assert!(!s.enabled);
        assert_eq!(s.state, "off");
    }

    #[derive(Clone, Debug)]
    struct CloudRequest {
        method: String,
        path: String,
        raw: String,
    }

    struct DeviceCloud {
        origin: String,
        requests: Arc<Mutex<Vec<CloudRequest>>>,
        list_responses: Arc<Mutex<std::collections::VecDeque<(u16, String)>>>,
        exchanges: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl DeviceCloud {
        async fn spawn() -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let list_responses = Arc::new(Mutex::new(std::collections::VecDeque::new()));
            let exchanges = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let server_requests = requests.clone();
            let server_lists = list_responses.clone();
            let server_exchanges = exchanges.clone();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let requests = server_requests.clone();
                    let lists = server_lists.clone();
                    let exchanges = server_exchanges.clone();
                    tokio::spawn(async move {
                        let mut data = Vec::new();
                        let expected = loop {
                            if let Some(head_end) = memchr::memmem::find(&data, b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&data[..head_end]);
                                let content_length = head
                                    .split("\r\n")
                                    .find_map(|line| {
                                        let (name, value) = line.split_once(':')?;
                                        name.eq_ignore_ascii_case("content-length")
                                            .then(|| value.trim().parse::<usize>().ok())
                                            .flatten()
                                    })
                                    .unwrap_or(0);
                                let expected = head_end + 4 + content_length;
                                if data.len() >= expected {
                                    break expected;
                                }
                            }
                            let mut chunk = [0u8; 4096];
                            let n = socket.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            data.extend_from_slice(&chunk[..n]);
                        };
                        data.truncate(expected);
                        let raw = String::from_utf8_lossy(&data).into_owned();
                        let head_end = memchr::memmem::find(data.as_slice(), b"\r\n\r\n").unwrap();
                        let head = String::from_utf8_lossy(&data[..head_end]);
                        let request_line = head.split("\r\n").next().unwrap_or_default();
                        let mut parts = request_line.split_whitespace();
                        let method = parts.next().unwrap_or_default().to_string();
                        let path = parts.next().unwrap_or_default().to_string();
                        requests.lock().unwrap().push(CloudRequest {
                            method: method.clone(),
                            path: path.clone(),
                            raw: raw.clone(),
                        });

                        let (status, response_body, cookies) =
                            if path == "/coding-relay/auth/exchange" && method == "POST" {
                                let generation = exchanges.fetch_add(1, Ordering::SeqCst) + 1;
                                (
                                    204,
                                    String::new(),
                                    vec![
                                        format!("rc_session={generation}; Path=/; HttpOnly"),
                                        "theme=dark; Path=/".to_string(),
                                    ],
                                )
                            } else if path == "/coding-relay/v1/remote/devices" && method == "GET" {
                                let (status, body) = lists
                                    .lock()
                                    .unwrap()
                                    .pop_front()
                                    .unwrap_or((200, "{\"devices\":[]}".to_string()));
                                (status, body, Vec::new())
                            } else if path.starts_with("/coding-relay/v1/remote/devices/")
                                && path.contains("device-broken")
                            {
                                (
                                    500,
                                    "{\"error\":{\"message\":\"relay rejected the mutation\"}}"
                                        .to_string(),
                                    Vec::new(),
                                )
                            } else if path.starts_with("/coding-relay/v1/remote/devices/")
                                && (method == "DELETE"
                                    || method == "PATCH"
                                    || (method == "POST" && path.ends_with("/deactivate")))
                            {
                                (204, String::new(), Vec::new())
                            } else {
                                (404, String::new(), Vec::new())
                            };
                        let reason = if status == 204 { "No Content" } else { "OK" };
                        let mut response = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nConnection: close\r\n"
                        );
                        for cookie in cookies {
                            response.push_str(&format!("Set-Cookie: {cookie}\r\n"));
                        }
                        response.push_str(&format!(
                            "Content-Length: {}\r\n\r\n{response_body}",
                            response_body.len()
                        ));
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.flush().await;
                    });
                }
            });
            Self {
                origin: format!("http://{addr}/coding-relay/"),
                requests,
                list_responses,
                exchanges,
            }
        }

        fn queue_list(&self, status: u16, body: serde_json::Value) {
            self.list_responses
                .lock()
                .unwrap()
                .push_back((status, body.to_string()));
        }

        fn requests(&self) -> Vec<CloudRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    static RELAY_ENV_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    struct RelayEnvGuard(Option<std::ffi::OsString>);

    impl RelayEnvGuard {
        fn set(origin: &str) -> Self {
            let previous = std::env::var_os(REMOTE_CONTROL_RELAY_URL_ENV);
            unsafe { std::env::set_var(REMOTE_CONTROL_RELAY_URL_ENV, origin) };
            Self(previous)
        }
    }

    impl Drop for RelayEnvGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => unsafe { std::env::set_var(REMOTE_CONTROL_RELAY_URL_ENV, value) },
                None => unsafe { std::env::remove_var(REMOTE_CONTROL_RELAY_URL_ENV) },
            }
        }
    }

    fn write_managed_credential(
        directory: &std::path::Path,
        name: &str,
        access_token: &str,
        refresh_token: &str,
    ) {
        std::fs::write(
            directory.join(format!("{name}.json")),
            serde_json::json!({
                "access_token": access_token,
                "refresh_token": refresh_token,
                "expires_at": chrono::Utc::now().timestamp() + 3600,
                "scope": "kimi",
                "token_type": "Bearer",
                "expires_in": 3600,
            })
            .to_string(),
        )
        .unwrap();
    }

    fn device_server(cloud: &DeviceCloud) -> (tempfile::TempDir, super::super::HttpServer) {
        let credentials = tempfile::tempdir().unwrap();
        write_managed_credential(credentials.path(), "kimi", "access-1", "refresh-1");
        let oauth = crate::server::oauth::OAuthManager::with_hosts(
            cloud.origin.clone(),
            cloud.origin.clone(),
            Some(credentials.path().to_path_buf()),
        );
        let server = super::super::HttpServer::in_memory()
            .unwrap()
            .with_oauth_manager(Arc::new(oauth))
            .with_config(crate::config::KimiConfig::default());
        (credentials, server)
    }

    fn device_request(
        method: &str,
        path: impl Into<String>,
        body: Option<JsonValue>,
    ) -> super::super::HttpRequest {
        super::super::HttpRequest {
            method: method.to_string(),
            path: path.into(),
            query: None,
            headers: HashMap::new(),
            body: body
                .map(|value| serde_json::to_vec(&value).unwrap())
                .unwrap_or_default(),
        }
    }

    #[tokio::test]
    async fn device_routes_exchange_once_and_proxy_list_and_delete() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);
        cloud.queue_list(
            200,
            serde_json::json!({
                "devices": [{
                    "device_id": "device/one",
                    "alias": "Workstation",
                    "status": "online",
                    "platform": "win32",
                    "client_version": "kimi-code/1.0.0",
                    "local_base_url": "http://127.0.0.1:3461",
                    "created_at": "2026-01-01T00:00:00.000Z",
                    "updated_at": "2026-01-01T00:00:00.000Z",
                    "last_remote_access_at": "2026-01-01T00:00:00.000Z",
                    "internal_note": "cloud-only"
                }],
                "max_devices": 3
            }),
        );

        let listed = server
            .handle_request(&device_request(
                "GET",
                "/api/v1/remote-control/devices",
                None,
            ))
            .await;
        assert_eq!(listed.status, 200);
        let listed: JsonValue = serde_json::from_slice(&listed.body).unwrap();
        assert_eq!(listed["devices"].as_array().unwrap().len(), 1);
        assert_eq!(listed["devices"][0]["device_id"], "device/one");
        assert_eq!(listed["max_devices"], 3);
        assert!(listed["devices"][0].get("internal_note").is_none());

        let deleted = server
            .handle_request(&device_request(
                "DELETE",
                "/api/v1/remote-control/devices/device%2Fone",
                None,
            ))
            .await;
        assert_eq!(deleted.status, 200);
        let deleted: JsonValue = serde_json::from_slice(&deleted.body).unwrap();
        assert_eq!(deleted, serde_json::json!({"ok": true}));

        let requests = cloud.requests();
        assert_eq!(requests.len(), 3, "{requests:?}");
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/coding-relay/auth/exchange");
        assert!(requests[0].raw.contains("\"access_token\":\"access-1\""));
        assert!(requests[0].raw.contains("\"refresh_token\":\"refresh-1\""));
        assert!(requests[0].raw.contains("\"expires_in\":3600"));
        assert_eq!(requests[1].method, "GET");
        assert_eq!(requests[2].method, "DELETE");
        assert_eq!(
            requests[2].path,
            "/coding-relay/v1/remote/devices/device%2Fone"
        );
        for request in &requests[1..] {
            assert!(
                request
                    .raw
                    .to_ascii_lowercase()
                    .contains("cookie: rc_session=1; theme=dark"),
                "{}",
                request.raw
            );
        }
        assert_eq!(cloud.exchanges.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn device_list_reexchanges_and_retries_once_after_401() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);
        cloud.queue_list(
            401,
            serde_json::json!({"error": {"message": "session expired"}}),
        );
        cloud.queue_list(200, serde_json::json!({"devices": []}));

        let listed = server
            .handle_request(&device_request(
                "GET",
                "/api/v1/remote-control/devices",
                None,
            ))
            .await;
        assert_eq!(listed.status, 200);
        let requests = cloud.requests();
        assert_eq!(requests.len(), 4, "{requests:?}");
        assert_eq!(requests[0].path, "/coding-relay/auth/exchange");
        assert_eq!(requests[2].path, "/coding-relay/auth/exchange");
        assert!(
            requests[1]
                .raw
                .to_ascii_lowercase()
                .contains("rc_session=1")
        );
        assert!(
            requests[3]
                .raw
                .to_ascii_lowercase()
                .contains("rc_session=2")
        );
        assert_eq!(cloud.exchanges.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn device_list_rejects_malformed_cloud_payload() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);
        cloud.queue_list(200, serde_json::json!({"devices": [{"alias": "missing"}]}));

        let listed = server
            .handle_request(&device_request(
                "GET",
                "/api/v1/remote-control/devices",
                None,
            ))
            .await;
        assert_eq!(listed.status, 500);
        let error: JsonValue = serde_json::from_slice(&listed.body).unwrap();
        assert!(
            error["error"]
                .as_str()
                .unwrap_or_default()
                .contains("unexpected response"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn device_list_uses_configured_oauth_slot() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (credentials, server) = device_server(&cloud);
        write_managed_credential(credentials.path(), "kimi", "wrong-access", "wrong-refresh");
        write_managed_credential(
            credentials.path(),
            "kimi-code-env-0e4f99c69cc27850",
            "custom-access",
            "custom-refresh",
        );
        let config: crate::config::KimiConfig = concat!(
            "[providers.\"managed:kimi-code\"]\n",
            "type = \"kimi\"\n\n",
            "[providers.\"managed:kimi-code\".oauth]\n",
            "key = \"oauth/kimi-code-env-0e4f99c69cc27850\"\n",
            "oauthHost = \"https://auth.kimi.ai\"\n",
        )
        .parse()
        .unwrap();
        let server = server.with_config(config);
        cloud.queue_list(200, serde_json::json!({"devices": []}));

        let listed = server
            .handle_request(&device_request(
                "GET",
                "/api/v1/remote-control/devices",
                None,
            ))
            .await;
        assert_eq!(listed.status, 200);
        let requests = cloud.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(
            requests[0]
                .raw
                .contains("\"access_token\":\"custom-access\"")
        );
        assert!(
            requests[0]
                .raw
                .contains("\"refresh_token\":\"custom-refresh\"")
        );
        assert!(!requests[0].raw.contains("wrong-refresh"));
    }

    #[tokio::test]
    async fn device_list_reexchanges_when_stored_credential_changes() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (credentials, server) = device_server(&cloud);
        cloud.queue_list(200, serde_json::json!({"devices": []}));
        cloud.queue_list(200, serde_json::json!({"devices": []}));

        let first = server
            .handle_request(&device_request(
                "GET",
                "/api/v1/remote-control/devices",
                None,
            ))
            .await;
        assert_eq!(first.status, 200);
        write_managed_credential(credentials.path(), "kimi", "access-1", "refresh-2");
        let second = server
            .handle_request(&device_request(
                "GET",
                "/api/v1/remote-control/devices",
                None,
            ))
            .await;
        assert_eq!(second.status, 200);

        let requests = cloud.requests();
        assert_eq!(requests.len(), 4, "{requests:?}");
        assert!(requests[0].raw.contains("\"refresh_token\":\"refresh-1\""));
        assert!(requests[2].raw.contains("\"refresh_token\":\"refresh-2\""));
        assert!(
            requests[1]
                .raw
                .to_ascii_lowercase()
                .contains("rc_session=1")
        );
        assert!(
            requests[3]
                .raw
                .to_ascii_lowercase()
                .contains("rc_session=2")
        );
        assert_eq!(cloud.exchanges.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn deleting_current_device_closes_the_local_runtime() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);

        let enabled = server
            .handle_request(&device_request(
                "POST",
                "/api/v1/remote-control",
                Some(serde_json::json!({"enabled": true, "refresh_token": "refresh-1"})),
            ))
            .await;
        assert_eq!(enabled.status, 200);
        let enabled: JsonValue = serde_json::from_slice(&enabled.body).unwrap();
        let device_id = enabled["device_id"].as_str().unwrap().to_string();

        let deleted = server
            .handle_request(&device_request(
                "DELETE",
                format!("/api/v1/remote-control/devices/{device_id}"),
                None,
            ))
            .await;
        assert_eq!(deleted.status, 200);
        assert!(server.remote_control_runtime.lock().await.is_none());

        let status = server
            .handle_request(&device_request("GET", "/api/v1/remote-control", None))
            .await;
        let status: JsonValue = serde_json::from_slice(&status.body).unwrap();
        assert_eq!(status["enabled"], false);
        assert_eq!(status["state"], "off");
        assert!(
            cloud.requests().iter().any(|request| {
                request.method == "DELETE"
                    && request.path == format!("/coding-relay/v1/remote/devices/{device_id}")
            }),
            "{:?}",
            cloud.requests()
        );
    }

    #[tokio::test]
    async fn renaming_a_device_patches_the_alias_through_the_relay_session() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);

        let renamed = server
            .handle_request(&device_request(
                "PATCH",
                "/api/v1/remote-control/devices/device%2Fone",
                Some(serde_json::json!({"alias": "  Lab machine  "})),
            ))
            .await;
        assert_eq!(renamed.status, 200);
        let body: JsonValue = serde_json::from_slice(&renamed.body).unwrap();
        assert_eq!(body, serde_json::json!({"ok": true}));

        let requests = cloud.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[1].method, "PATCH");
        assert_eq!(
            requests[1].path,
            "/coding-relay/v1/remote/devices/device%2Fone"
        );
        assert!(
            requests[1]
                .raw
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert!(requests[1].raw.contains(r#"{"alias":"Lab machine"}"#));
    }

    #[tokio::test]
    async fn renaming_rejects_an_empty_or_overlong_alias() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);

        for alias in [
            serde_json::json!(""),
            serde_json::json!("   "),
            serde_json::json!("x".repeat(51)),
            serde_json::json!(null),
        ] {
            let rejected = server
                .handle_request(&device_request(
                    "PATCH",
                    "/api/v1/remote-control/devices/device-1",
                    Some(serde_json::json!({ "alias": alias })),
                ))
                .await;
            assert_eq!(rejected.status, 400);
            let body: JsonValue = serde_json::from_slice(&rejected.body).unwrap();
            assert_eq!(body["code"], 40001u32);
        }
        assert!(cloud.requests().is_empty(), "{:?}", cloud.requests());
    }

    #[tokio::test]
    async fn a_rejected_rename_reports_the_internal_error_code() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);

        let rejected = server
            .handle_request(&device_request(
                "PATCH",
                "/api/v1/remote-control/devices/device-broken",
                Some(serde_json::json!({"alias": "Lab machine"})),
            ))
            .await;
        assert_eq!(rejected.status, 500);
        let body: JsonValue = serde_json::from_slice(&rejected.body).unwrap();
        assert_eq!(body["code"], 50001u32);
        assert!(
            body["msg"]
                .as_str()
                .unwrap_or_default()
                .contains("relay rejected the mutation"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn deactivating_a_foreign_device_posts_to_the_deactivate_endpoint() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);

        let deactivated = server
            .handle_request(&device_request(
                "POST",
                "/api/v1/remote-control/devices/device%2Ftwo/deactivate",
                None,
            ))
            .await;
        assert_eq!(deactivated.status, 200);
        let body: JsonValue = serde_json::from_slice(&deactivated.body).unwrap();
        assert_eq!(body, serde_json::json!({"ok": true}));

        let requests = cloud.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(requests[1].method, "POST");
        assert_eq!(
            requests[1].path,
            "/coding-relay/v1/remote/devices/device%2Ftwo/deactivate"
        );
    }

    #[tokio::test]
    async fn deactivating_the_current_device_stops_the_local_runtime_and_the_cloud_entry() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);

        let enabled = server
            .handle_request(&device_request(
                "POST",
                "/api/v1/remote-control",
                Some(serde_json::json!({"enabled": true, "refresh_token": "refresh-1"})),
            ))
            .await;
        assert_eq!(enabled.status, 200);
        let enabled: JsonValue = serde_json::from_slice(&enabled.body).unwrap();
        let device_id = enabled["device_id"].as_str().unwrap().to_string();

        let deactivated = server
            .handle_request(&device_request(
                "POST",
                format!("/api/v1/remote-control/devices/{device_id}/deactivate"),
                None,
            ))
            .await;
        assert_eq!(deactivated.status, 200);
        assert!(server.remote_control_runtime.lock().await.is_none());
        let status = server
            .handle_request(&device_request("GET", "/api/v1/remote-control", None))
            .await;
        let status: JsonValue = serde_json::from_slice(&status.body).unwrap();
        assert_eq!(status["state"], "off");
        assert!(
            cloud.requests().iter().any(|request| {
                request.method == "POST"
                    && request.path
                        == format!("/coding-relay/v1/remote/devices/{device_id}/deactivate")
            }),
            "{:?}",
            cloud.requests()
        );
    }

    #[tokio::test]
    async fn a_second_enable_reports_already_running_with_holder_details() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);

        let first = server
            .handle_request(&device_request(
                "POST",
                "/api/v1/remote-control",
                Some(serde_json::json!({"enabled": true, "refresh_token": "refresh-1"})),
            ))
            .await;
        assert_eq!(first.status, 200);
        let first: JsonValue = serde_json::from_slice(&first.body).unwrap();
        assert!(first.get("already_running").is_none());

        let second = server
            .handle_request(&device_request(
                "POST",
                "/api/v1/remote-control",
                Some(serde_json::json!({"enabled": true, "refresh_token": "refresh-1"})),
            ))
            .await;
        assert_eq!(second.status, 409);
        let second: JsonValue = serde_json::from_slice(&second.body).unwrap();
        assert_eq!(second["code"], 40928u32);
        assert!(
            second["msg"]
                .as_str()
                .unwrap_or_default()
                .contains("already running"),
            "{second}"
        );
        let details = &second["details"];
        assert_eq!(details["device_id"], first["device_id"]);
        assert_eq!(details["url"], first["url"]);
        assert_eq!(details["local_origin"], "http://127.0.0.1");
        assert_eq!(details["pid"], serde_json::json!(std::process::id()));
        assert!(details["started_at"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn a_held_logout_gate_rejects_starts_and_shows_up_as_the_status_conflict() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);

        let started_at = server.remote_control_logout_gate.begin();
        let rejected = server
            .handle_request(&device_request(
                "POST",
                "/api/v1/remote-control",
                Some(serde_json::json!({"enabled": true, "refresh_token": "refresh-1"})),
            ))
            .await;
        assert_eq!(rejected.status, 409);
        let rejected: JsonValue = serde_json::from_slice(&rejected.body).unwrap();
        assert_eq!(rejected["code"], 40928u32);
        assert_eq!(rejected["details"]["local_origin"], "logout in progress");
        assert_eq!(rejected["details"]["started_at"], started_at);

        let status = server
            .handle_request(&device_request("GET", "/api/v1/remote-control", None))
            .await;
        let status: JsonValue = serde_json::from_slice(&status.body).unwrap();
        assert_eq!(status["conflict"]["local_origin"], "logout in progress");
        assert_eq!(status["conflict"]["started_at"], started_at);

        server.remote_control_logout_gate.end();
        assert!(cloud.requests().is_empty(), "{:?}", cloud.requests());
    }

    #[tokio::test]
    async fn logout_clears_the_device_session_and_stops_the_local_tunnel() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);

        let enabled = server
            .handle_request(&device_request(
                "POST",
                "/api/v1/remote-control",
                Some(serde_json::json!({"enabled": true, "refresh_token": "refresh-1"})),
            ))
            .await;
        assert_eq!(enabled.status, 200);

        server.oauth_manager().logout("kimi");
        tokio::time::sleep(Duration::from_millis(400)).await;

        assert!(server.remote_control_runtime.lock().await.is_none());
        let status = server
            .handle_request(&device_request("GET", "/api/v1/remote-control", None))
            .await;
        let status: JsonValue = serde_json::from_slice(&status.body).unwrap();
        assert_eq!(status["state"], "off");
        assert_eq!(status["enabled"], false);

        // Logging out again is a no-op and must not resurrect the tunnel.
        assert!(!server.oauth_manager().logout("kimi"));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(server.remote_control_runtime.lock().await.is_none());
    }

    #[tokio::test]
    async fn logging_out_another_provider_leaves_the_tunnel_running() {
        let _lock = RELAY_ENV_LOCK.lock().await;
        let cloud = DeviceCloud::spawn().await;
        let _env = RelayEnvGuard::set(&cloud.origin);
        let (_credentials, server) = device_server(&cloud);

        let enabled = server
            .handle_request(&device_request(
                "POST",
                "/api/v1/remote-control",
                Some(serde_json::json!({"enabled": true, "refresh_token": "refresh-1"})),
            ))
            .await;
        assert_eq!(enabled.status, 200);

        server.oauth_manager().logout("other-provider");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(server.remote_control_runtime.lock().await.is_some());
    }
}
