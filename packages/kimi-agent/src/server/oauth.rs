use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_MANAGED_BASE_URL: &str = "https://api.kimi.com/coding/v1";
const DEVICE_AUTH_PATH: &str = "/api/oauth/device_authorization";
const TOKEN_PATH: &str = "/api/oauth/token";
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
const DEFAULT_FLOW_TTL_SECS: u64 = 900;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_MAX_ATTEMPTS: u32 = 3;

const REFRESH_RETRYABLE_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];

fn oauth_host() -> String {
    std::env::var("KIMI_CODE_OAUTH_HOST")
        .or_else(|_| std::env::var("KIMI_OAUTH_HOST"))
        .unwrap_or_else(|_| DEFAULT_OAUTH_HOST.to_string())
}

fn credentials_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("KIMI_CODE_CREDENTIALS_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)?;
    Some(home.join(".kimi-code").join("credentials"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthFlowSnapshot {
    pub provider: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    scope: String,
    token_type: String,
    expires_in: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedToken {
    pub storage_name: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

struct FlowState {
    snapshot: OAuthFlowSnapshot,
    device_code: String,
    interval: u64,
    ttl_secs: u64,

    generation: u64,
}

#[derive(Default)]
struct OAuthManagerInner {
    flows: Mutex<HashMap<String, FlowState>>,
    tokens: Mutex<HashMap<String, StoredToken>>,

    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    generation: AtomicU64,
}

pub trait RemoteControlLogoutHook: Send + Sync {
    fn around_logout(&self, logout: &mut dyn FnMut());
}

pub struct OAuthManager {
    inner: OAuthManagerInner,
    client: reqwest::Client,
    oauth_host: String,
    managed_base_url: String,
    credentials_dir: Option<PathBuf>,
    remote_control_logout: std::sync::OnceLock<Arc<dyn RemoteControlLogoutHook>>,
}

impl Default for OAuthManager {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthManager {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            inner: OAuthManagerInner::default(),
            client,
            oauth_host: oauth_host(),
            managed_base_url: std::env::var("KIMI_CODE_MANAGED_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_MANAGED_BASE_URL.to_string()),
            credentials_dir: credentials_dir(),
            remote_control_logout: std::sync::OnceLock::new(),
        }
    }

    pub fn set_remote_control_logout_hook(&self, hook: Arc<dyn RemoteControlLogoutHook>) {
        let _ = self.remote_control_logout.set(hook);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_hosts(
        oauth_host: String,
        managed_base_url: String,
        credentials_dir: Option<PathBuf>,
    ) -> Self {
        let mut manager = Self::new();
        manager.oauth_host = oauth_host;
        manager.managed_base_url = managed_base_url;
        manager.credentials_dir = credentials_dir;
        manager
    }

    async fn post_form(&self, path: &str, params: &[(&str, &str)]) -> Result<(u16, Value), String> {
        if let Some(host) = self.oauth_host.strip_prefix("http://") {
            let authority = host.split('/').next().unwrap_or(host);
            let loopback = authority.starts_with("localhost")
                || authority.starts_with("127.0.0.1")
                || authority.starts_with("[::1]");
            if !loopback {
                return Err(format!(
                    "Refusing to send credentials to non-HTTPS OAuth endpoint: {}",
                    self.oauth_host
                ));
            }
        }
        let url = format!("{}{}", self.oauth_host.trim_end_matches('/'), path);
        let response = self
            .client
            .post(url)
            .header("Accept", "application/json")
            .form(params)
            .send()
            .await
            .map_err(|e| format!("OAuth request failed: {e}"))?;
        let status = response.status().as_u16();
        let data = response.json::<Value>().await.unwrap_or(Value::Null);
        Ok((status, data))
    }

    fn error_detail(data: &Value) -> String {
        data.get("error_description")
            .and_then(|v| v.as_str())
            .or_else(|| data.get("message").and_then(|v| v.as_str()))
            .or_else(|| data.get("error").and_then(|v| v.as_str()))
            .unwrap_or("unknown")
            .to_string()
    }

    fn token_from_response(data: &Value) -> Result<StoredToken, String> {
        let access_token = data
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("OAuth response missing access_token")?;
        let refresh_token = data
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("OAuth response missing refresh_token")?;
        let expires_in = data
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .filter(|v| *v > 0)
            .ok_or("OAuth response missing or invalid expires_in")?;
        Ok(StoredToken {
            access_token: access_token.to_string(),
            refresh_token: refresh_token.to_string(),
            expires_at: chrono::Utc::now().timestamp() + expires_in,
            scope: data
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            token_type: data
                .get("token_type")
                .and_then(|v| v.as_str())
                .unwrap_or("Bearer")
                .to_string(),
            expires_in,
        })
    }

    fn credentials_path(&self, provider: &str) -> Option<PathBuf> {
        let dir = self.credentials_dir.as_ref()?;

        let safe: String = provider
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if safe.is_empty() {
            return None;
        }
        Some(dir.join(format!("{safe}.json")))
    }

    fn save_token(&self, provider: &str, token: &StoredToken) {
        self.inner
            .tokens
            .lock()
            .unwrap()
            .insert(provider.to_string(), token.clone());
        let Some(path) = self.credentials_path(provider) else {
            return;
        };
        let Ok(body) = serde_json::to_string_pretty(token) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension(format!("tmp.{}", fastrand::u32(..)));
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    fn load_token(&self, provider: &str) -> Option<StoredToken> {
        if let Some(token) = self.inner.tokens.lock().unwrap().get(provider) {
            return Some(token.clone());
        }
        let path = self.credentials_path(provider)?;
        let body = std::fs::read_to_string(path).ok()?;
        let token: StoredToken = serde_json::from_str(&body).ok()?;
        self.inner
            .tokens
            .lock()
            .unwrap()
            .insert(provider.to_string(), token.clone());
        Some(token)
    }

    pub fn has_cached_token(&self, provider: &str) -> bool {
        self.load_token(provider).is_some()
    }

    pub const MANAGED_TOKEN_NAMES: [&'static str; 2] = ["kimi", "kimi-code"];

    pub fn has_managed_token(&self) -> bool {
        Self::MANAGED_TOKEN_NAMES
            .iter()
            .any(|name| self.has_cached_token(name))
    }

    pub(crate) fn managed_remote_control_token(
        &self,
        preferred_storage_name: &str,
        allow_managed_aliases: bool,
    ) -> Result<ManagedToken, String> {
        let mut candidates = vec![preferred_storage_name];
        if allow_managed_aliases {
            candidates.extend(Self::MANAGED_TOKEN_NAMES);
        }
        for name in candidates {
            let Some(path) = self.credentials_path(name) else {
                continue;
            };
            let Ok(body) = std::fs::read_to_string(path) else {
                continue;
            };
            let Ok(token) = serde_json::from_str::<StoredToken>(&body) else {
                continue;
            };
            if !token.refresh_token.is_empty() {
                return Ok(ManagedToken {
                    storage_name: name.to_string(),
                    access_token: token.access_token,
                    refresh_token: token.refresh_token,
                    expires_in: token.expires_in,
                });
            }
        }
        Err("Remote Control requires a Kimi login.".into())
    }

    pub async fn managed_access_token(&self) -> Result<String, String> {
        for name in Self::MANAGED_TOKEN_NAMES {
            if let Some(token) = self.load_usable_token(name) {
                return Ok(token.access_token);
            }
            if let Some(stored) = self.load_token(name)
                && let Ok(refreshed) = self.refresh_token(name, &stored).await
            {
                return Ok(refreshed.access_token);
            }
        }
        Err("no cached managed credential; sign in first".into())
    }

    pub fn managed_base_url(&self) -> &str {
        &self.managed_base_url
    }

    pub async fn start_login(
        self: Arc<Self>,
        provider: &str,
        _region: Option<&str>,
    ) -> OAuthFlowSnapshot {
        let generation = self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (status, data) = match self
            .post_form(DEVICE_AUTH_PATH, &[("client_id", KIMI_CLIENT_ID)])
            .await
        {
            Ok(result) => result,
            Err(e) => {
                let snapshot = OAuthFlowSnapshot {
                    provider: provider.to_string(),
                    status: "error".into(),
                    user_code: None,
                    verification_uri: None,
                    verification_uri_complete: None,
                    expires_in: None,
                    error_message: Some(e),
                };
                self.inner.flows.lock().unwrap().insert(
                    provider.to_string(),
                    FlowState {
                        snapshot: snapshot.clone(),
                        device_code: String::new(),
                        interval: DEFAULT_POLL_INTERVAL_SECS,
                        ttl_secs: DEFAULT_FLOW_TTL_SECS,
                        generation,
                    },
                );
                return snapshot;
            }
        };

        let field = |name: &str| {
            data.get(name)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let user_code = field("user_code");
        let device_code = field("device_code");
        let verification_uri_complete = field("verification_uri_complete");
        if user_code.is_none() || device_code.is_none() || verification_uri_complete.is_none() {
            let snapshot = OAuthFlowSnapshot {
                provider: provider.to_string(),
                status: "error".into(),
                user_code,
                verification_uri: field("verification_uri"),
                verification_uri_complete,
                expires_in: None,
                error_message: Some(format!(
                    "Device authorization failed (HTTP {status}): {}",
                    Self::error_detail(&data)
                )),
            };
            self.inner.flows.lock().unwrap().insert(
                provider.to_string(),
                FlowState {
                    snapshot: snapshot.clone(),
                    device_code: String::new(),
                    interval: DEFAULT_POLL_INTERVAL_SECS,
                    ttl_secs: DEFAULT_FLOW_TTL_SECS,
                    generation,
                },
            );
            return snapshot;
        }

        let interval = data
            .get("interval")
            .and_then(|v| v.as_u64())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);
        let expires_in = data
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_FLOW_TTL_SECS);

        let snapshot = OAuthFlowSnapshot {
            provider: provider.to_string(),
            status: "pending".into(),
            verification_uri: field("verification_uri"),
            user_code,
            verification_uri_complete,
            expires_in: Some(expires_in),
            error_message: None,
        };
        self.inner.flows.lock().unwrap().insert(
            provider.to_string(),
            FlowState {
                snapshot: snapshot.clone(),
                device_code: device_code.unwrap_or_default(),
                interval,
                ttl_secs: expires_in,
                generation,
            },
        );

        let manager = self.clone();
        let provider_owned = provider.to_string();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            loop {
                let interval = manager.poll_interval(&provider_owned);
                tokio::time::sleep(Duration::from_secs(interval)).await;
                let Some(state_generation) = manager.current_generation(&provider_owned) else {
                    return;
                };
                if state_generation != generation {
                    return;
                }
                if started.elapsed().as_secs() > manager.poll_ttl(&provider_owned) {
                    manager.finish_flow(&provider_owned, generation, "expired", None);
                    return;
                }
                let device_code = manager.device_code(&provider_owned);
                match manager
                    .post_form(
                        TOKEN_PATH,
                        &[
                            ("client_id", KIMI_CLIENT_ID),
                            ("device_code", device_code.as_str()),
                            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ],
                    )
                    .await
                {
                    Err(_) => continue,
                    Ok((status, data)) => {
                        if status == 200 && data.get("access_token").is_some() {
                            match Self::token_from_response(&data) {
                                Ok(token) => {
                                    manager.save_token(&provider_owned, &token);
                                    manager.finish_flow(
                                        &provider_owned,
                                        generation,
                                        "success",
                                        None,
                                    );
                                }
                                Err(e) => {
                                    manager.finish_flow(
                                        &provider_owned,
                                        generation,
                                        "error",
                                        Some(&e),
                                    );
                                }
                            }
                            return;
                        }
                        if status >= 500 {
                            continue;
                        }
                        match data.get("error").and_then(|v| v.as_str()).unwrap_or("") {
                            "authorization_pending" | "slow_down" => continue,
                            "expired_token" => {
                                manager.finish_flow(&provider_owned, generation, "expired", None);
                                return;
                            }
                            "access_denied" => {
                                manager.finish_flow(
                                    &provider_owned,
                                    generation,
                                    "denied",
                                    Some(&Self::error_detail(&data)),
                                );
                                return;
                            }
                            other => {
                                manager.finish_flow(
                                    &provider_owned,
                                    generation,
                                    "error",
                                    Some(&format!(
                                        "Device token polling failed (HTTP {status}): {other}"
                                    )),
                                );
                                return;
                            }
                        }
                    }
                }
            }
        });

        snapshot
    }

    fn poll_interval(&self, provider: &str) -> u64 {
        self.inner
            .flows
            .lock()
            .unwrap()
            .get(provider)
            .map(|s| s.interval)
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
    }

    fn poll_ttl(&self, provider: &str) -> u64 {
        self.inner
            .flows
            .lock()
            .unwrap()
            .get(provider)
            .map(|s| s.ttl_secs)
            .unwrap_or(DEFAULT_FLOW_TTL_SECS)
    }

    fn device_code(&self, provider: &str) -> String {
        self.inner
            .flows
            .lock()
            .unwrap()
            .get(provider)
            .map(|s| s.device_code.clone())
            .unwrap_or_default()
    }

    fn current_generation(&self, provider: &str) -> Option<u64> {
        self.inner
            .flows
            .lock()
            .unwrap()
            .get(provider)
            .map(|s| s.generation)
    }

    fn finish_flow(
        &self,
        provider: &str,
        generation: u64,
        status: &str,
        error_message: Option<&str>,
    ) {
        let mut flows = self.inner.flows.lock().unwrap();
        let Some(state) = flows.get_mut(provider) else {
            return;
        };
        if state.generation != generation {
            return;
        }
        state.snapshot.status = status.to_string();
        state.snapshot.error_message = error_message.map(str::to_string);
    }

    pub fn get_flow(&self, provider: &str) -> Option<OAuthFlowSnapshot> {
        self.inner
            .flows
            .lock()
            .unwrap()
            .get(provider)
            .map(|s| s.snapshot.clone())
    }

    pub fn cancel_login(&self, provider: &str) -> bool {
        let generation = self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut flows = self.inner.flows.lock().unwrap();
        let Some(state) = flows.get_mut(provider) else {
            return false;
        };
        state.generation = generation;
        state.snapshot.status = "cancelled".into();
        true
    }

    pub fn logout(&self, provider: &str) -> bool {
        if let Some(hook) = self.remote_control_logout.get()
            && Self::MANAGED_TOKEN_NAMES.contains(&provider)
        {
            let mut dropped = false;
            hook.around_logout(&mut || dropped = self.logout_inner(provider));
            return dropped;
        }
        self.logout_inner(provider)
    }

    fn logout_inner(&self, provider: &str) -> bool {
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        let had_flow = self.inner.flows.lock().unwrap().remove(provider).is_some();
        let had_token = self.inner.tokens.lock().unwrap().remove(provider).is_some();
        if let Some(path) = self.credentials_path(provider) {
            let _ = std::fs::remove_file(path);
        }
        had_flow || had_token
    }

    fn refresh_lock(&self, provider: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .refresh_locks
            .lock()
            .unwrap()
            .entry(provider.to_string())
            .or_default()
            .clone()
    }

    async fn refresh_token(
        &self,
        provider: &str,
        token: &StoredToken,
    ) -> Result<StoredToken, String> {
        let lock = self.refresh_lock(provider);
        let _guard = lock.lock().await;

        if let Some(current) = self.load_usable_token(provider)
            && current.access_token != token.access_token
        {
            return Ok(current);
        }
        let mut last_error = String::from("Token refresh failed.");
        for attempt in 0..REFRESH_MAX_ATTEMPTS {
            let (status, data) = self
                .post_form(
                    TOKEN_PATH,
                    &[
                        ("client_id", KIMI_CLIENT_ID),
                        ("grant_type", "refresh_token"),
                        ("refresh_token", token.refresh_token.as_str()),
                    ],
                )
                .await?;
            if status == 200 && data.get("access_token").is_some() {
                let refreshed = Self::token_from_response(&data)?;
                self.save_token(provider, &refreshed);
                return Ok(refreshed);
            }
            let error_code = data.get("error").and_then(|v| v.as_str()).unwrap_or("");
            if status == 401 || status == 403 || error_code == "invalid_grant" {
                self.inner.tokens.lock().unwrap().remove(provider);
                if let Some(path) = self.credentials_path(provider) {
                    let _ = std::fs::remove_file(path);
                }
                return Err("Token refresh unauthorized.".into());
            }
            last_error = format!(
                "Token refresh failed (HTTP {status}): {}",
                Self::error_detail(&data)
            );
            if REFRESH_RETRYABLE_STATUSES.contains(&status) && attempt < REFRESH_MAX_ATTEMPTS - 1 {
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                continue;
            }
            if !REFRESH_RETRYABLE_STATUSES.contains(&status) {
                break;
            }
        }
        Err(last_error)
    }

    fn load_usable_token(&self, provider: &str) -> Option<StoredToken> {
        let token = self.load_token(provider)?;

        let refresh_deadline = token.expires_at.saturating_sub(60);
        if refresh_deadline < chrono::Utc::now().timestamp() && !token.refresh_token.is_empty() {
            return None;
        }
        Some(token)
    }

    async fn managed_get(&self, provider: &str, path: &str) -> Value {
        let Some(token) = self.load_usable_token(provider) else {
            if let Some(stored) = self.load_token(provider)
                && let Ok(refreshed) = self.refresh_token(provider, &stored).await
            {
                return self.managed_get_with(provider, path, &refreshed).await;
            }
            return json!({ "authenticated": false, "provider": provider });
        };
        self.managed_get_with(provider, path, &token).await
    }

    async fn managed_get_with(&self, provider: &str, path: &str, token: &StoredToken) -> Value {
        let url = format!("{}/{}", self.managed_base_url.trim_end_matches('/'), path);
        let mut response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token.access_token))
            .header("Accept", "application/json")
            .send()
            .await;

        if let Ok(res) = &response
            && res.status().as_u16() == 401
        {
            match self.refresh_token(provider, token).await {
                Ok(refreshed) => {
                    response = self
                        .client
                        .get(&url)
                        .header(
                            "Authorization",
                            format!("Bearer {}", refreshed.access_token),
                        )
                        .header("Accept", "application/json")
                        .send()
                        .await;
                }
                Err(e) => {
                    return json!({ "authenticated": false, "provider": provider, "error": e });
                }
            }
        }
        match response {
            Ok(res) if res.status().is_success() => res.json::<Value>().await.unwrap_or(json!({
                "authenticated": true,
                "provider": provider,
                "error": "Malformed managed API response.",
            })),
            Ok(res) => json!({
                "authenticated": true,
                "provider": provider,
                "error": format!("Managed API request failed (HTTP {})", res.status().as_u16()),
            }),
            Err(e) => {
                json!({ "authenticated": true, "provider": provider, "error": e.to_string() })
            }
        }
    }

    pub async fn get_usage(&self, provider: &str) -> Value {
        self.managed_get(provider, "usages").await
    }

    pub async fn get_user_info(&self, provider: &str) -> Value {
        self.managed_get(provider, "me").await
    }
}

pub const KIMI_REGION_MARKER_FILENAME: &str = "region";

const OAUTH_HOST_MAINLAND_CN: &str = "https://auth.kimi.com";
const OAUTH_HOST_GLOBAL: &str = "https://auth.kimi.ai";

fn normalize_host(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

pub fn region_for_oauth_host(oauth_host: &str) -> Option<&'static str> {
    let normalized = normalize_host(oauth_host);
    match normalized {
        OAUTH_HOST_MAINLAND_CN => Some("mainland-cn"),
        OAUTH_HOST_GLOBAL => Some("global"),
        _ => None,
    }
}

fn read_region_marker(home_dir: &std::path::Path) -> Option<&'static str> {
    let raw = std::fs::read_to_string(home_dir.join(KIMI_REGION_MARKER_FILENAME)).ok()?;
    match raw.trim() {
        "mainland-cn" => Some("mainland-cn"),
        "global" => Some("global"),
        _ => None,
    }
}

pub fn resolve_kimi_region(
    env_oauth_host: Option<&str>,
    configured_oauth_host: Option<&str>,
    configured_oauth_key: Option<&str>,
    home_dir: &std::path::Path,
) -> &'static str {
    if let Some(host) = env_oauth_host.filter(|h| !h.trim().is_empty()) {
        return region_for_oauth_host(host).unwrap_or("mainland-cn");
    }
    if let Some(host) = configured_oauth_host.filter(|h| !h.trim().is_empty())
        && let Some(region) = region_for_oauth_host(host)
    {
        return region;
    }
    if configured_oauth_key == Some("oauth/kimi-code") {
        return "mainland-cn";
    }
    read_region_marker(home_dir).unwrap_or("mainland-cn")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker_home(region: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(region) = region {
            std::fs::write(dir.path().join(KIMI_REGION_MARKER_FILENAME), region).unwrap();
        }
        dir
    }

    #[test]
    fn env_host_matching_a_profile_pins_the_region() {
        let dir = marker_home(Some("global"));
        assert_eq!(
            resolve_kimi_region(Some("https://auth.kimi.ai"), None, None, dir.path()),
            "global"
        );
        assert_eq!(
            resolve_kimi_region(Some("https://auth.kimi.com/"), None, None, dir.path()),
            "mainland-cn"
        );
    }

    #[test]
    fn an_unknown_env_host_skips_config_and_marker() {
        let dir = marker_home(Some("global"));
        assert_eq!(
            resolve_kimi_region(
                Some("https://oauth.internal.example"),
                Some("https://auth.kimi.ai"),
                None,
                dir.path()
            ),
            "mainland-cn"
        );
    }

    #[test]
    fn configured_host_and_default_credential_key_win_over_the_marker() {
        let dir = marker_home(None);
        assert_eq!(
            resolve_kimi_region(None, Some("https://auth.kimi.ai"), None, dir.path()),
            "global"
        );

        assert_eq!(
            resolve_kimi_region(None, None, Some("oauth/kimi-code"), dir.path()),
            "mainland-cn"
        );
    }

    #[test]
    fn marker_and_default_apply_last() {
        let global = marker_home(Some("global"));
        assert_eq!(
            resolve_kimi_region(None, None, None, global.path()),
            "global"
        );

        let junk = marker_home(Some("antarctica"));
        assert_eq!(
            resolve_kimi_region(None, None, None, junk.path()),
            "mainland-cn"
        );

        let empty = marker_home(None);
        assert_eq!(
            resolve_kimi_region(None, None, None, empty.path()),
            "mainland-cn"
        );
    }

    async fn spawn_mock_oauth_server(deny: bool) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut token_polls = 0u32;
            async fn respond(status: &str, reason: &str, body: String) -> String {
                format!(
                    "HTTP/1.1 {status} {reason}
content-type: application/json
connection: close
content-length: {}

{body}",
                    body.len()
                )
            }
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 8192];
                let Ok(n) = sock.read(&mut buf).await else {
                    break;
                };
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let response = if request.starts_with("POST /api/oauth/device_authorization") {
                    respond(
                        "200",
                        "OK",
                        json!({
                            "user_code": "WDJB-MJHT",
                            "device_code": "dev-123",
                            "verification_uri": "https://example.test/device",
                            "verification_uri_complete": "https://example.test/device?code=WDJB-MJHT",
                            "expires_in": 900,
                            "interval": 1
                        })
                        .to_string(),
                    )
                    .await
                } else if request.starts_with("POST /api/oauth/token") {
                    token_polls += 1;
                    if deny {
                        respond(
                            "400",
                            "Bad Request",
                            json!({"error": "access_denied", "error_description": "The user denied the request"})
                                .to_string(),
                        )
                        .await
                    } else if token_polls < 3 {
                        respond(
                            "400",
                            "Bad Request",
                            json!({"error": "authorization_pending"}).to_string(),
                        )
                        .await
                    } else {
                        respond(
                            "200",
                            "OK",
                            json!({
                                "access_token": "at-real-1",
                                "refresh_token": "rt-real-1",
                                "expires_in": 3600,
                                "token_type": "Bearer",
                                "scope": "read write"
                            })
                            .to_string(),
                        )
                        .await
                    }
                } else if request.starts_with("GET /me") {
                    if request.contains("Bearer at-real-1") {
                        respond(
                            "200",
                            "OK",
                            json!({"user_id": "user-1", "name": "Mock User", "authenticated": true})
                                .to_string(),
                        )
                        .await
                    } else {
                        respond("401", "Unauthorized", String::from("{}")).await
                    }
                } else if request.starts_with("GET /usages") {
                    respond(
                        "200",
                        "OK",
                        json!({"quota_status": "ok", "usage": {"total_tokens": 42}}).to_string(),
                    )
                    .await
                } else {
                    respond("404", "Not Found", String::from("{}")).await
                };
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        addr
    }

    fn temp_credentials_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kimi-oauth-test-{tag}-{}", fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn concurrent_refresh_hits_the_token_endpoint_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 8192];
                let Ok(n) = sock.read(&mut buf).await else {
                    break;
                };
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if request.starts_with("POST /api/oauth/token") {
                    observed.fetch_add(1, Ordering::SeqCst);

                    tokio::time::sleep(Duration::from_millis(100)).await;
                    json!({
                        "access_token": "at-refreshed",
                        "refresh_token": "rt-refreshed",
                        "expires_in": 3600,
                        "scope": "read write",
                        "token_type": "Bearer"
                    })
                    .to_string()
                } else {
                    String::from("{}")
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        let manager = Arc::new(OAuthManager::with_hosts(
            format!("http://{addr}"),
            "http://127.0.0.1:9".into(),
            Some(temp_credentials_dir("single-flight")),
        ));
        manager.save_token(
            OAuthManager::MANAGED_TOKEN_NAMES[0],
            &StoredToken {
                access_token: "at-stale".into(),
                refresh_token: "rt-stale".into(),
                expires_at: chrono::Utc::now().timestamp() - 3600,
                scope: "read write".into(),
                token_type: "Bearer".into(),
                expires_in: 3600,
            },
        );

        let (first, second) = tokio::join!(
            manager.managed_access_token(),
            manager.managed_access_token()
        );

        assert_eq!(first.unwrap(), "at-refreshed");
        assert_eq!(second.unwrap(), "at-refreshed");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    async fn wait_for_terminal_status(manager: &Arc<OAuthManager>, provider: &str) -> String {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if let Some(flow) = manager.get_flow(provider)
                && flow.status != "pending"
            {
                return flow.status;
            }
        }
        "pending".into()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn device_flow_completes_with_real_exchange_and_persists_token() {
        let addr = spawn_mock_oauth_server(false).await;
        let dir = temp_credentials_dir("success");
        let manager = Arc::new(OAuthManager::with_hosts(
            format!("http://{addr}"),
            format!("http://{addr}"),
            Some(dir.clone()),
        ));

        let snapshot = manager.clone().start_login("kimi", None).await;
        assert_eq!(snapshot.status, "pending");
        assert_eq!(snapshot.user_code.as_deref(), Some("WDJB-MJHT"));
        assert_eq!(
            snapshot.verification_uri_complete.as_deref(),
            Some("https://example.test/device?code=WDJB-MJHT")
        );

        let status = wait_for_terminal_status(&manager, "kimi").await;
        assert_eq!(
            status, "success",
            "flow must reach success via real exchange"
        );

        let body = std::fs::read_to_string(dir.join("kimi.json")).expect("credentials file");
        let wire: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(wire["access_token"], "at-real-1");
        assert_eq!(wire["refresh_token"], "rt-real-1");
        assert!(wire["expires_at"].is_number());

        let user = manager.get_user_info("kimi").await;
        assert_eq!(user["user_id"], "user-1");
        assert_eq!(user["name"], "Mock User");
        let usage = manager.get_usage("kimi").await;
        assert_eq!(usage["usage"]["total_tokens"], 42);

        assert!(manager.logout("kimi"));
        assert!(manager.get_flow("kimi").is_none());
        assert!(!dir.join("kimi.json").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn denied_flow_reports_denied_and_stores_nothing() {
        let addr = spawn_mock_oauth_server(true).await;
        let dir = temp_credentials_dir("denied");
        let manager = Arc::new(OAuthManager::with_hosts(
            format!("http://{addr}"),
            format!("http://{addr}"),
            Some(dir.clone()),
        ));

        manager.clone().start_login("kimi", None).await;
        let status = wait_for_terminal_status(&manager, "kimi").await;
        assert_eq!(status, "denied");
        assert!(
            !dir.join("kimi.json").exists(),
            "no token may be stored on denial"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_https_non_loopback_host_is_refused() {
        let dir = temp_credentials_dir("refused");
        let manager = Arc::new(OAuthManager::with_hosts(
            "http://auth.example.test".into(),
            "https://api.example.test".into(),
            Some(dir),
        ));
        let snapshot = manager.clone().start_login("kimi", None).await;
        assert_eq!(snapshot.status, "error");
        assert!(
            snapshot
                .error_message
                .as_deref()
                .unwrap_or_default()
                .contains("non-HTTPS"),
            "credentials must be refused on non-HTTPS remote hosts"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_stops_poller_and_marks_cancelled() {
        let addr = spawn_mock_oauth_server(false).await;
        let dir = temp_credentials_dir("cancel");
        let manager = Arc::new(OAuthManager::with_hosts(
            format!("http://{addr}"),
            format!("http://{addr}"),
            Some(dir),
        ));
        manager.clone().start_login("kimi", None).await;

        let poller_generation = manager.current_generation("kimi").unwrap();
        assert!(manager.cancel_login("kimi"));

        assert_ne!(
            manager.current_generation("kimi"),
            Some(poller_generation),
            "a cancel must make the poller's captured generation stale"
        );

        manager.finish_flow("kimi", poller_generation, "success", None);
        let flow = manager.get_flow("kimi").unwrap();
        assert_eq!(
            flow.status, "cancelled",
            "poller must not resurrect a cancelled flow"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unauthenticated_reads_are_honest() {
        let dir = temp_credentials_dir("anon");
        let manager = OAuthManager::with_hosts(
            "https://auth.example.test".into(),
            "https://api.example.test".into(),
            Some(dir),
        );

        let user = manager.get_user_info("kimi").await;
        assert_eq!(user["authenticated"], false);
        let usage = manager.get_usage("kimi").await;
        assert_eq!(usage["authenticated"], false);
    }
}
