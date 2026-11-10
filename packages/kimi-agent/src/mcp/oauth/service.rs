use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};

use super::auth_code::{
    AuthCodeRegistry, AuthCodeStart, AuthCodeStatus, exchange_code, start_callback_listener,
};
use super::store::McpOAuthFileStore;

const REFRESH_SKEW_MS: i64 = 60_000;

const TOKENS_SUFFIX: &str = "-tokens";

fn tokens_key(key: &str) -> String {
    format!("{key}{TOKENS_SUFFIX}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obtained_at_ms: Option<i64>,
}

impl McpOAuthTokens {
    fn is_expired(&self, now_ms: i64) -> bool {
        match self.expires_at_ms {
            Some(expiry) => now_ms + REFRESH_SKEW_MS >= expiry,
            None => false,
        }
    }

    fn same_grant(&self, other: &Self) -> bool {
        self.access_token == other.access_token
            && self.refresh_token == other.refresh_token
            && self.expires_at_ms == other.expires_at_ms
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOAuthTokenState {
    pub has_tokens: bool,
    pub has_refresh_token: bool,
    pub expired: bool,
    pub access_token: Option<String>,
}

const CONCURRENT_GRANT_GRACE_MS: i64 = 10_000;

fn is_concurrent_grant(tokens: &McpOAuthTokens, now: i64, connected_at_ms: Option<i64>) -> bool {
    let Some(obtained_at) = tokens.obtained_at_ms else {
        return false;
    };
    let age = now - obtained_at;
    if !(0..CONCURRENT_GRANT_GRACE_MS).contains(&age) {
        return false;
    }
    match connected_at_ms {
        Some(connected_at) => obtained_at >= connected_at,
        None => true,
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

struct ActiveAuthorization {
    start: AuthCodeStart,
    result: watch::Sender<Option<Result<(), String>>>,
    callback: watch::Sender<bool>,
    attached: AtomicUsize,
    cancel: tokio_util::sync::CancellationToken,
}

#[derive(Debug)]
pub enum BeginAuthorizationError {
    AlreadyAuthorized,
    Failed(String),
}

struct AuthorizationWait {
    key: String,
    active: Arc<ActiveAuthorization>,
    waiter: tokio_util::sync::CancellationToken,
    detached: AtomicBool,
}

fn detach_authorization_wait(wait: &AuthorizationWait) -> bool {
    if wait.detached.swap(true, Ordering::SeqCst) {
        return false;
    }
    wait.waiter.cancel();
    let last = wait.active.attached.fetch_sub(1, Ordering::SeqCst) == 1;
    if last && wait.active.result.borrow().is_none() {
        wait.active.cancel.cancel();
    }
    last
}

pub struct AuthorizationFlowHandle {
    pub wait_id: String,
    pub authorization_url: String,
    server_name: String,
    wait: Arc<AuthorizationWait>,
    service: Weak<McpOAuthService>,
}

impl AuthorizationFlowHandle {
    pub async fn complete(
        &mut self,
        cancelled: Option<&(dyn Fn() -> bool + Sync)>,
        timeout: Duration,
    ) -> Result<(), String> {
        if self.wait.detached.load(Ordering::SeqCst) {
            return Err("OAuth flow already completed or cancelled".into());
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut result = self.wait.active.result.subscribe();
        let mut callback = self.wait.active.callback.subscribe();
        let outcome = loop {
            if let Some(outcome) = result.borrow().clone() {
                break outcome;
            }
            let callback_settled = *callback.borrow();
            tokio::select! {
                changed = result.changed() => {
                    if changed.is_err() {
                        break Err("OAuth callback listener closed".into());
                    }
                }
                _ = callback.changed() => {}
                _ = self.wait.waiter.cancelled() => {
                    break Err("OAuth authorization wait cancelled".into());
                }
                _ = tokio::time::sleep_until(deadline), if !callback_settled => {
                    break Err("OAuth callback timed out".into());
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if cancelled.is_some_and(|probe| probe()) {
                        break Err("OAuth authorization wait cancelled".into());
                    }
                }
            }
        };
        self.detach();
        outcome.map_err(|error| format!("OAuth flow for \"{}\" failed: {error}", self.server_name))
    }

    fn detach(&mut self) {
        let last = detach_authorization_wait(&self.wait);
        let Some(service) = self.service.upgrade() else {
            return;
        };
        let wait_id = self.wait_id.clone();
        let key = self.wait.key.clone();
        let active = self.wait.active.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                service.authorization_waits.lock().await.remove(&wait_id);
                if last {
                    let mut flows = service.authorizations.lock().await;
                    if flows
                        .get(&key)
                        .is_some_and(|current| Arc::ptr_eq(current, &active))
                    {
                        flows.remove(&key);
                    }
                }
            });
        }
    }
}

impl Drop for AuthorizationFlowHandle {
    fn drop(&mut self) {
        self.detach();
    }
}

pub struct McpOAuthService {
    store: Arc<McpOAuthFileStore>,
    client: reqwest::Client,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    auth_code: Arc<AuthCodeRegistry>,
    authorizations: Mutex<HashMap<String, Arc<ActiveAuthorization>>>,
    authorization_waits: Mutex<HashMap<String, Arc<AuthorizationWait>>>,
}

impl McpOAuthService {
    pub fn new(store: Arc<McpOAuthFileStore>) -> Self {
        Self {
            store,
            client: reqwest::Client::new(),
            locks: Mutex::new(HashMap::new()),
            auth_code: Arc::new(AuthCodeRegistry::default()),
            authorizations: Mutex::new(HashMap::new()),
            authorization_waits: Mutex::new(HashMap::new()),
        }
    }

    pub async fn begin_authorization(
        self: &Arc<Self>,
        server_name: &str,
        server_url: &str,
        client_name: &str,
    ) -> Result<AuthorizationFlowHandle, BeginAuthorizationError> {
        let key = super::mcp_oauth_store_key(server_name, server_url)
            .map_err(BeginAuthorizationError::Failed)?;
        if self.access_token(&key).await.is_some() {
            return Err(BeginAuthorizationError::AlreadyAuthorized);
        }
        let mut flows = self.authorizations.lock().await;
        if let Some(active) = flows.get(&key).cloned()
            && active.result.borrow().is_none()
        {
            return Ok(self.attach_authorization(server_name, key, active).await);
        }
        flows.remove(&key);
        let mut listener = start_callback_listener()
            .await
            .map_err(BeginAuthorizationError::Failed)?;
        let start = self
            .auth_code
            .begin(
                &self.client,
                server_name,
                server_url,
                listener.redirect_uri(),
                None,
                client_name,
            )
            .await
            .map_err(|error| {
                BeginAuthorizationError::Failed(format!(
                    "failed to start OAuth flow for \"{server_name}\": {error}"
                ))
            })?;
        let (result_tx, _) = watch::channel(None);
        let (callback_tx, _) = watch::channel(false);
        let active = Arc::new(ActiveAuthorization {
            start: start.clone(),
            result: result_tx,
            callback: callback_tx,
            attached: AtomicUsize::new(0),
            cancel: listener.cancellation_token(),
        });
        flows.insert(key.clone(), active.clone());
        let service = self.clone();
        let active_task = active.clone();
        let start_task = start.clone();
        let key_task = key.clone();
        tokio::spawn(async move {
            let callback = tokio::select! {
                _ = active_task.cancel.cancelled() => {
                    Err("OAuth authorization wait cancelled".to_string())
                }
                result = listener.wait() => result,
            };
            active_task.callback.send_replace(true);
            let outcome = match callback {
                Ok(callback) => {
                    if callback.state.as_deref() != Some(start_task.state.as_str()) {
                        Err("OAuth state mismatch — possible CSRF; refusing token exchange".into())
                    } else {
                        match service.auth_code.take_by_state(&start_task.state).await {
                            Some(flow) if flow.flow_id == start_task.flow_id => {
                                match exchange_code(&service.client, &flow, &callback.code).await {
                                    Ok(tokens) => service.store_tokens(&key_task, &tokens),
                                    Err(error) => Err(error),
                                }
                            }
                            _ => Err(
                                "OAuth state mismatch — possible CSRF; refusing token exchange"
                                    .into(),
                            ),
                        }
                    }
                }
                Err(error) => Err(error),
            };
            service
                .auth_code
                .finish(
                    &start_task.flow_id,
                    AuthCodeStatus {
                        done: true,
                        authenticated: outcome.is_ok(),
                        key: outcome.as_ref().ok().map(|_| key_task.clone()),
                        error: outcome.as_ref().err().cloned(),
                    },
                )
                .await;
            active_task.result.send_replace(Some(outcome));
            active_task.cancel.cancel();
        });
        Ok(self.attach_authorization(server_name, key, active).await)
    }

    async fn attach_authorization(
        self: &Arc<Self>,
        server_name: &str,
        key: String,
        active: Arc<ActiveAuthorization>,
    ) -> AuthorizationFlowHandle {
        active.attached.fetch_add(1, Ordering::SeqCst);
        let wait_id = ulid::Ulid::new().to_string();
        let wait = Arc::new(AuthorizationWait {
            key,
            active: active.clone(),
            waiter: tokio_util::sync::CancellationToken::new(),
            detached: AtomicBool::new(false),
        });
        self.authorization_waits
            .lock()
            .await
            .insert(wait_id.clone(), wait.clone());
        AuthorizationFlowHandle {
            wait_id,
            authorization_url: active.start.authorization_url.clone(),
            server_name: server_name.to_string(),
            wait,
            service: Arc::downgrade(self),
        }
    }

    pub async fn cancel_authorization_wait(&self, wait_id: &str) -> bool {
        let Some(wait) = self.authorization_waits.lock().await.remove(wait_id) else {
            return false;
        };
        let last = detach_authorization_wait(&wait);
        if last {
            let mut flows = self.authorizations.lock().await;
            if flows
                .get(&wait.key)
                .is_some_and(|current| Arc::ptr_eq(current, &wait.active))
            {
                flows.remove(&wait.key);
            }
        }
        true
    }

    pub fn has_tokens(&self, key: &str) -> bool {
        self.read_tokens(key).is_some()
    }

    fn read_tokens(&self, key: &str) -> Option<McpOAuthTokens> {
        self.store
            .read::<McpOAuthTokens>(&tokens_key(key))
            .or_else(|| self.store.read::<McpOAuthTokens>(key))
    }

    fn remove_tokens(&self, key: &str) -> Result<bool, String> {
        let suffixed = self.store.remove(&tokens_key(key))?;
        let legacy = self.store.remove(key)?;
        Ok(suffixed || legacy)
    }

    pub fn list_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .store
            .list(None)
            .into_iter()
            .filter_map(|name| {
                if let Some(base) = name.strip_suffix(TOKENS_SUFFIX) {
                    return Some(base.to_string());
                }
                if name.ends_with("-meta")
                    || name.ends_with("-client")
                    || name.ends_with("-discovery")
                {
                    return None;
                }
                Some(name)
            })
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    pub fn store_tokens(&self, key: &str, tokens: &McpOAuthTokens) -> Result<(), String> {
        let mut stamped = tokens.clone();
        if stamped.obtained_at_ms.is_none() {
            stamped.obtained_at_ms = Some(now_ms());
        }
        self.store.write(&tokens_key(key), &stamped)
    }

    pub fn now(&self) -> i64 {
        now_ms()
    }

    pub fn peek_rejected_grant(
        &self,
        key: &str,
        connected_at_ms: Option<i64>,
    ) -> Option<(McpOAuthTokens, bool)> {
        let tokens = self.read_tokens(key)?;
        let concurrent = is_concurrent_grant(&tokens, now_ms(), connected_at_ms);
        Some((tokens, concurrent))
    }

    pub fn clear_tokens_if_current(
        &self,
        key: &str,
        expected: &McpOAuthTokens,
    ) -> Result<bool, String> {
        let Some(stored) = self.read_tokens(key) else {
            return Ok(false);
        };
        if !stored.same_grant(expected) {
            return Ok(false);
        }
        self.remove_tokens(key)
    }

    pub fn remove(&self, key: &str) -> Result<bool, String> {
        self.remove_tokens(key)
    }

    pub fn token_state(&self, key: &str) -> McpOAuthTokenState {
        let Some(tokens) = self.read_tokens(key) else {
            return McpOAuthTokenState {
                has_tokens: false,
                has_refresh_token: false,
                expired: false,
                access_token: None,
            };
        };
        McpOAuthTokenState {
            has_tokens: true,
            has_refresh_token: tokens
                .refresh_token
                .as_deref()
                .is_some_and(|v| !v.is_empty()),
            expired: tokens.is_expired(now_ms()),
            access_token: Some(tokens.access_token),
        }
    }

    pub async fn access_token(&self, key: &str) -> Option<String> {
        let tokens = self.read_tokens(key)?;
        if !tokens.is_expired(now_ms()) {
            return Some(tokens.access_token);
        }

        let lock = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        let tokens = self.read_tokens(key)?;
        if !tokens.is_expired(now_ms()) {
            return Some(tokens.access_token);
        }

        let refreshed = self.refresh(&tokens).await?;
        let _ = self.store_tokens(key, &refreshed);
        Some(refreshed.access_token)
    }

    async fn refresh(&self, tokens: &McpOAuthTokens) -> Option<McpOAuthTokens> {
        let endpoint = tokens.token_endpoint.as_deref()?;
        let refresh_token = tokens.refresh_token.as_deref()?;
        let client_id = tokens.client_id.clone().unwrap_or_default();
        let client_secret = tokens.client_secret.clone().unwrap_or_default();
        let resource = tokens.resource.clone().unwrap_or_default();
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        if !client_id.is_empty() {
            form.push(("client_id", &client_id));
        }
        if !client_secret.is_empty() {
            form.push(("client_secret", &client_secret));
        }
        if !resource.is_empty() {
            form.push(("resource", &resource));
        }

        let response = self.client.post(endpoint).form(&form).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        let access_token = body.get("access_token")?.as_str()?.to_string();
        let expires_in = body.get("expires_in").and_then(|value| value.as_i64());
        Some(McpOAuthTokens {
            access_token,
            refresh_token: body
                .get("refresh_token")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .or_else(|| tokens.refresh_token.clone()),
            expires_at_ms: expires_in.map(|secs| now_ms() + secs * 1000),
            token_endpoint: tokens.token_endpoint.clone(),
            client_id: tokens.client_id.clone(),
            client_secret: tokens.client_secret.clone(),
            resource: tokens.resource.clone(),
            obtained_at_ms: Some(now_ms()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn spawn_token_endpoint(
        body: &'static str,
    ) -> (String, Arc<AtomicUsize>, tokio::sync::oneshot::Sender<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/token");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((mut socket, _)) = accepted else { break };
                        let hits = hits_task.clone();
                        tokio::spawn(async move {
                            let mut buffer = vec![0u8; 4096];
                            let _ = socket.read(&mut buffer).await;
                            hits.fetch_add(1, Ordering::SeqCst);
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.flush().await;
                        });
                    }
                }
            }
        });
        (url, hits, shutdown_tx)
    }

    fn expired_tokens(endpoint: &str) -> McpOAuthTokens {
        McpOAuthTokens {
            access_token: "stale".into(),
            refresh_token: Some("refresh-1".into()),
            expires_at_ms: Some(now_ms() - 1_000),
            token_endpoint: Some(endpoint.into()),
            client_id: Some("client-1".into()),
            client_secret: None,
            resource: None,
            obtained_at_ms: None,
        }
    }

    #[tokio::test]
    async fn test_valid_token_is_returned_without_refreshing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(McpOAuthFileStore::new(dir.path()));
        let service = McpOAuthService::new(store.clone());
        store
            .write(
                "srv",
                &McpOAuthTokens {
                    access_token: "fresh".into(),
                    refresh_token: Some("r".into()),
                    expires_at_ms: Some(now_ms() + 3_600_000),
                    token_endpoint: None,
                    client_id: None,
                    client_secret: None,
                    resource: None,
                    obtained_at_ms: None,
                },
            )
            .unwrap();

        assert!(service.has_tokens("srv"));
        assert_eq!(service.access_token("srv").await.as_deref(), Some("fresh"));
        assert!(!service.has_tokens("missing"));
        assert_eq!(service.access_token("missing").await, None);
    }

    #[tokio::test]
    async fn test_expired_token_refreshes_and_persists() {
        let (url, hits, _shutdown) =
            spawn_token_endpoint(r#"{"access_token":"new-token","expires_in":3600}"#).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(McpOAuthFileStore::new(dir.path()));
        store.write("srv", &expired_tokens(&url)).unwrap();
        let service = McpOAuthService::new(store.clone());

        assert_eq!(
            service.access_token("srv").await.as_deref(),
            Some("new-token")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        assert_eq!(
            service.access_token("srv").await.as_deref(),
            Some("new-token")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let stored = store.read::<McpOAuthTokens>("srv-tokens").unwrap();
        assert_eq!(stored.access_token, "new-token");
        assert_eq!(stored.refresh_token.as_deref(), Some("refresh-1"));
    }

    #[tokio::test]
    async fn test_concurrent_callers_share_one_refresh() {
        let (url, hits, _shutdown) =
            spawn_token_endpoint(r#"{"access_token":"shared","expires_in":3600}"#).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(McpOAuthFileStore::new(dir.path()));
        store.write("srv", &expired_tokens(&url)).unwrap();
        let service = Arc::new(McpOAuthService::new(store));

        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.access_token("srv").await })
        };
        let second = {
            let service = service.clone();
            tokio::spawn(async move { service.access_token("srv").await })
        };

        assert_eq!(first.await.unwrap().as_deref(), Some("shared"));
        assert_eq!(second.await.unwrap().as_deref(), Some("shared"));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "refresh must be single-flight"
        );
    }

    #[tokio::test]
    async fn test_refresh_without_material_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(McpOAuthFileStore::new(dir.path()));
        store
            .write(
                "srv",
                &McpOAuthTokens {
                    access_token: "stale".into(),
                    refresh_token: None,
                    expires_at_ms: Some(now_ms() - 1_000),
                    token_endpoint: None,
                    client_id: None,
                    client_secret: None,
                    resource: None,
                    obtained_at_ms: None,
                },
            )
            .unwrap();
        let service = McpOAuthService::new(store);
        assert_eq!(service.access_token("srv").await, None);
    }

    fn grant(access_token: &str, obtained_at_ms: Option<i64>) -> McpOAuthTokens {
        McpOAuthTokens {
            access_token: access_token.into(),
            refresh_token: None,
            expires_at_ms: None,
            token_endpoint: None,
            client_id: None,
            client_secret: None,
            resource: None,
            obtained_at_ms,
        }
    }

    #[test]
    fn test_store_tokens_stamps_obtained_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(McpOAuthFileStore::new(dir.path()));
        let service = McpOAuthService::new(store.clone());

        service.store_tokens("srv", &grant("a", None)).unwrap();
        let stamped = store.read::<McpOAuthTokens>("srv-tokens").unwrap();
        let obtained = stamped.obtained_at_ms.expect("write must stamp a grant");
        assert!((now_ms() - obtained).abs() < 5_000);

        let earlier = now_ms() - 30_000;
        service
            .store_tokens("srv", &grant("b", Some(earlier)))
            .unwrap();
        assert_eq!(
            store
                .read::<McpOAuthTokens>("srv-tokens")
                .unwrap()
                .obtained_at_ms,
            Some(earlier)
        );
    }

    #[test]
    fn test_token_state_reports_stored_grant() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(McpOAuthFileStore::new(dir.path()));
        let service = McpOAuthService::new(store);

        assert!(!service.token_state("missing").has_tokens);

        let mut fresh = grant("fresh", None);
        fresh.refresh_token = Some("refresh".into());
        fresh.expires_at_ms = Some(now_ms() + 3_600_000);
        service.store_tokens("fresh-key", &fresh).unwrap();
        let state = service.token_state("fresh-key");
        assert!(state.has_tokens && state.has_refresh_token && !state.expired);
        assert_eq!(state.access_token.as_deref(), Some("fresh"));

        let mut expired = grant("stale", None);
        expired.expires_at_ms = Some(now_ms() - 1);
        service.store_tokens("stale-key", &expired).unwrap();
        let state = service.token_state("stale-key");
        assert!(state.has_tokens && state.expired && !state.has_refresh_token);
    }

    #[test]
    fn test_concurrent_grant_window() {
        let now = now_ms();

        assert!(is_concurrent_grant(
            &grant("a", Some(now - 5_000)),
            now,
            Some(now - 20_000)
        ));

        assert!(is_concurrent_grant(
            &grant("a", Some(now - 5_000)),
            now,
            None
        ));

        assert!(!is_concurrent_grant(
            &grant("a", Some(now - 60_000)),
            now,
            Some(now - 120_000)
        ));

        assert!(!is_concurrent_grant(
            &grant("a", Some(now - 5_000)),
            now,
            Some(now - 1_000)
        ));

        assert!(!is_concurrent_grant(
            &grant("a", Some(now + 5_000)),
            now,
            Some(now - 1_000)
        ));

        assert!(!is_concurrent_grant(&grant("a", None), now, None));
    }

    #[test]
    fn test_clear_tokens_if_current() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(McpOAuthFileStore::new(dir.path()));
        let service = McpOAuthService::new(store.clone());
        service
            .store_tokens("srv", &grant("rejected", Some(now_ms() - 60_000)))
            .unwrap();
        let rejected = store.read::<McpOAuthTokens>("srv-tokens").unwrap();

        service
            .store_tokens("srv", &grant("renewed", None))
            .unwrap();
        assert!(!service.clear_tokens_if_current("srv", &rejected).unwrap());
        assert_eq!(
            store
                .read::<McpOAuthTokens>("srv-tokens")
                .unwrap()
                .access_token,
            "renewed"
        );

        let stored = store.read::<McpOAuthTokens>("srv-tokens").unwrap();
        assert!(service.clear_tokens_if_current("srv", &stored).unwrap());
        assert!(!service.has_tokens("srv"));
        assert!(!service.clear_tokens_if_current("srv", &stored).unwrap());
    }

    #[test]
    fn test_peek_rejected_grant() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(McpOAuthFileStore::new(dir.path()));
        let service = McpOAuthService::new(store.clone());
        assert!(service.peek_rejected_grant("srv", None).is_none());

        service.store_tokens("srv", &grant("fresh", None)).unwrap();
        let now = now_ms();

        let (tokens, concurrent) = service
            .peek_rejected_grant("srv", Some(now - 1_000))
            .unwrap();
        assert_eq!(tokens.access_token, "fresh");
        assert!(concurrent);

        service
            .store_tokens("srv", &grant("old", Some(now - 5_000)))
            .unwrap();
        let (_, concurrent) = service
            .peek_rejected_grant("srv", Some(now - 1_000))
            .unwrap();
        assert!(!concurrent);
    }

    #[tokio::test]
    async fn joined_cancellation_leaves_the_shared_authorization_alive() {
        let (base, log) = crate::mcp::oauth::auth_code::test_helpers::spawn_mock_as().await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(McpOAuthFileStore::new(dir.path()));
        let service = Arc::new(McpOAuthService::new(store));
        let server_url = format!("{base}/mcp");
        let mut first = service
            .begin_authorization("srv", &server_url, "Kimi Code")
            .await
            .unwrap();
        let mut second = service
            .begin_authorization("srv", &server_url, "Kimi Code")
            .await
            .unwrap();
        assert_eq!(first.authorization_url, second.authorization_url);
        assert_ne!(first.wait_id, second.wait_id);
        assert!(service.cancel_authorization_wait(&second.wait_id).await);
        let error = second
            .complete(None, Duration::from_secs(10))
            .await
            .expect_err("a cancelled handle cannot complete again");
        assert!(error.contains("already completed or cancelled"), "{error}");

        let parsed = url::Url::parse(&first.authorization_url).unwrap();
        let params: HashMap<String, String> = parsed
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        let completion =
            tokio::spawn(async move { first.complete(None, Duration::from_secs(10)).await });
        let response = reqwest::Client::new()
            .get(format!(
                "{}?code=the-code&state={}",
                params["redirect_uri"], params["state"]
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        completion.await.unwrap().unwrap();
        let key = crate::mcp::oauth::mcp_oauth_store_key("srv", &server_url).unwrap();
        assert_eq!(service.access_token(&key).await.as_deref(), Some("at-1"));
        assert_eq!(
            log.lock()
                .await
                .iter()
                .filter(|request| request.starts_with("POST /token "))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancelling_the_last_waiter_starts_a_fresh_authorization() {
        let (base, _log) = crate::mcp::oauth::auth_code::test_helpers::spawn_mock_as().await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(McpOAuthFileStore::new(dir.path()));
        let service = Arc::new(McpOAuthService::new(store));
        let server_url = format!("{base}/mcp");
        let first = service
            .begin_authorization("srv", &server_url, "Kimi Code")
            .await
            .unwrap();
        let second = service
            .begin_authorization("srv", &server_url, "Kimi Code")
            .await
            .unwrap();
        assert!(service.cancel_authorization_wait(&first.wait_id).await);
        assert!(service.cancel_authorization_wait(&second.wait_id).await);

        let third = service
            .begin_authorization("srv", &server_url, "Kimi Code")
            .await
            .unwrap();
        assert_ne!(third.authorization_url, first.authorization_url);
        assert!(service.cancel_authorization_wait(&third.wait_id).await);
    }

    #[tokio::test]
    async fn callback_timeout_does_not_limit_an_in_flight_token_exchange() {
        let (base, _log, exchange_started, release_exchange) =
            crate::mcp::oauth::auth_code::test_helpers::spawn_gated_exchange_as().await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(McpOAuthFileStore::new(dir.path()));
        let service = Arc::new(McpOAuthService::new(store));
        let server_url = format!("{base}/mcp");
        let mut flow = service
            .begin_authorization("srv", &server_url, "Kimi Code")
            .await
            .unwrap();
        let parsed = url::Url::parse(&flow.authorization_url).unwrap();
        let params: HashMap<String, String> = parsed
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        let completion =
            tokio::spawn(async move { flow.complete(None, Duration::from_millis(50)).await });
        let response = reqwest::Client::new()
            .get(format!(
                "{}?code=the-code&state={}",
                params["redirect_uri"], params["state"]
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(2), exchange_started.notified())
            .await
            .expect("the token exchange starts");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let finished_early = completion.is_finished();
        release_exchange.notify_one();
        assert!(
            !finished_early,
            "the callback deadline must stop at callback receipt"
        );
        tokio::time::timeout(Duration::from_secs(2), completion)
            .await
            .expect("the released exchange completes")
            .unwrap()
            .unwrap();
        let key = crate::mcp::oauth::mcp_oauth_store_key("srv", &server_url).unwrap();
        assert_eq!(service.access_token(&key).await.as_deref(), Some("at-1"));
    }
}
