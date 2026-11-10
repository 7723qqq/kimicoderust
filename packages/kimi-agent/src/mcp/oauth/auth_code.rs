use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, watch};

use super::{McpOAuthTokens, canonical_mcp_oauth_resource};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

const FLOW_TTL: Duration = Duration::from_secs(15 * 60);

const RESULT_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone)]
pub struct DiscoveredAuthServer {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,

    pub resource: String,

    pub scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingAuthCodeFlow {
    pub flow_id: String,

    pub name: String,
    pub server_url: String,
    pub resource: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub code_verifier: String,
    pub state: String,
    pub redirect_uri: String,
    pub scope: Option<String>,
    created: Instant,
}

impl PendingAuthCodeFlow {
    fn expired(&self) -> bool {
        self.created.elapsed() > FLOW_TTL
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthCodeStart {
    pub flow_id: String,
    pub authorization_url: String,
    pub state: String,
    pub redirect_uri: String,
    pub expires_in: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AuthCodeStatus {
    pub done: bool,
    pub authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Default)]
pub struct AuthCodeRegistry {
    pending_by_state: Mutex<HashMap<String, PendingAuthCodeFlow>>,
    state_by_flow: Mutex<HashMap<String, String>>,
    results: Mutex<HashMap<String, (AuthCodeStatus, Instant)>>,
}

impl AuthCodeRegistry {
    pub async fn begin(
        &self,
        client: &reqwest::Client,
        name: &str,
        server_url: &str,
        redirect_uri: &str,
        scope: Option<&str>,
        client_name: &str,
    ) -> Result<AuthCodeStart, String> {
        let discovered = discover_authorization_server(client, server_url).await?;
        let Some(registration_endpoint) = discovered.registration_endpoint.as_deref() else {
            return Err(
                "MCP OAuth authorization server offers no dynamic client registration".into(),
            );
        };
        let scope = scope
            .map(str::to_string)
            .or_else(|| discovered.scope.clone());
        let (client_id, client_secret) = register_client(
            client,
            registration_endpoint,
            redirect_uri,
            client_name,
            scope.as_deref(),
        )
        .await?;
        let code_verifier = random_b64url(64)?;
        let code_challenge = pkce_challenge(&code_verifier);
        let state = random_b64url(32)?;
        let flow_id = random_b64url(16)?;
        let authorization_url = authorization_url(
            &discovered.authorization_endpoint,
            &client_id,
            redirect_uri,
            &code_challenge,
            &state,
            &discovered.resource,
            scope.as_deref(),
        )?;
        let flow = PendingAuthCodeFlow {
            flow_id: flow_id.clone(),
            name: name.to_string(),
            server_url: server_url.to_string(),
            resource: discovered.resource,
            token_endpoint: discovered.token_endpoint,
            client_id,
            client_secret,
            code_verifier,
            state: state.clone(),
            redirect_uri: redirect_uri.to_string(),
            scope,
            created: Instant::now(),
        };
        {
            let mut pending = self.pending_by_state.lock().await;
            prune_expired(&mut pending);
            pending.insert(state.clone(), flow);
        }
        self.state_by_flow
            .lock()
            .await
            .insert(flow_id.clone(), state.clone());
        Ok(AuthCodeStart {
            flow_id,
            authorization_url,
            state,
            redirect_uri: redirect_uri.to_string(),
            expires_in: FLOW_TTL.as_secs(),
        })
    }

    pub async fn take_by_state(&self, state: &str) -> Option<PendingAuthCodeFlow> {
        let mut pending = self.pending_by_state.lock().await;
        prune_expired(&mut pending);
        let flow = pending.remove(state)?;
        self.state_by_flow.lock().await.remove(&flow.flow_id);
        Some(flow)
    }

    pub async fn finish(&self, flow_id: &str, status: AuthCodeStatus) {
        if let Some(state) = self.state_by_flow.lock().await.remove(flow_id) {
            self.pending_by_state.lock().await.remove(&state);
        }
        let mut results = self.results.lock().await;
        prune_results(&mut results);
        results.insert(flow_id.to_string(), (status, Instant::now()));
    }

    pub async fn status(&self, flow_id: &str) -> AuthCodeStatus {
        {
            let mut results = self.results.lock().await;
            prune_results(&mut results);
            if let Some((status, _)) = results.get(flow_id) {
                return status.clone();
            }
        }
        let state = self.state_by_flow.lock().await.get(flow_id).cloned();
        match state {
            Some(state) => {
                let pending = self.pending_by_state.lock().await;
                match pending.get(&state) {
                    Some(flow) if !flow.expired() => AuthCodeStatus::default(),
                    _ => AuthCodeStatus {
                        done: true,
                        error: Some("the authorization request expired".into()),
                        ..Default::default()
                    },
                }
            }
            None => AuthCodeStatus {
                done: true,
                error: Some(format!("unknown or expired MCP OAuth flow: {flow_id}")),
                ..Default::default()
            },
        }
    }

    pub async fn cancel(&self, flow_id: &str) -> bool {
        let Some(state) = self.state_by_flow.lock().await.remove(flow_id) else {
            return false;
        };
        self.pending_by_state.lock().await.remove(&state);
        self.results.lock().await.insert(
            flow_id.to_string(),
            (
                AuthCodeStatus {
                    done: true,
                    error: Some("the authorization request was cancelled".into()),
                    ..Default::default()
                },
                Instant::now(),
            ),
        );
        true
    }
}

pub async fn exchange_code(
    client: &reqwest::Client,
    flow: &PendingAuthCodeFlow,
    code: &str,
) -> Result<McpOAuthTokens, String> {
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", flow.redirect_uri.as_str()),
        ("client_id", flow.client_id.as_str()),
        ("code_verifier", flow.code_verifier.as_str()),
        ("resource", flow.resource.as_str()),
    ];
    if let Some(secret) = flow.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let response = client
        .post(&flow.token_endpoint)
        .header("accept", "application/json")
        .form(&form)
        .timeout(DISCOVERY_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("MCP OAuth token exchange failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(200).collect();
        return Err(if detail.is_empty() {
            format!("MCP OAuth token exchange failed (HTTP {status})")
        } else {
            format!("MCP OAuth token exchange failed (HTTP {status}): {detail}")
        });
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("MCP OAuth token response is not JSON: {error}"))?;
    let access_token = body
        .get("access_token")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "MCP OAuth token response carried no access_token".to_string())?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let expires_in = body.get("expires_in").and_then(|value| value.as_i64());
    Ok(McpOAuthTokens {
        access_token: access_token.to_string(),
        refresh_token: body
            .get("refresh_token")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        expires_at_ms: expires_in.map(|seconds| now_ms + seconds.saturating_mul(1000)),
        token_endpoint: Some(flow.token_endpoint.clone()),
        client_id: Some(flow.client_id.clone()),
        client_secret: flow.client_secret.clone(),
        resource: Some(flow.resource.clone()),
        obtained_at_ms: Some(now_ms),
    })
}

pub async fn discover_authorization_server(
    client: &reqwest::Client,
    server_url: &str,
) -> Result<DiscoveredAuthServer, String> {
    let resource_url =
        url::Url::parse(server_url).map_err(|error| format!("Invalid MCP server url: {error}"))?;
    let resource = canonical_mcp_oauth_resource(server_url)?;
    let path = resource_url.path().trim_end_matches('/');
    let origin = resource_url.origin().ascii_serialization();

    let mut protected_resource = None;
    for candidate in [
        format!("{origin}/.well-known/oauth-protected-resource{path}"),
        format!("{origin}/.well-known/oauth-protected-resource"),
    ] {
        if let Some(value) = fetch_json(client, &candidate).await {
            protected_resource = Some(value);
            break;
        }
    }

    let mut issuers: Vec<String> = Vec::new();
    if let Some(advertised) = protected_resource
        .as_ref()
        .and_then(|value| value.get("authorization_servers"))
        .and_then(|value| value.as_array())
    {
        issuers.extend(
            advertised
                .iter()
                .filter_map(|value| value.as_str())
                .map(str::to_string),
        );
    }
    issuers.push(origin.clone());
    let scope = protected_resource
        .as_ref()
        .and_then(|value| value.get("scope"))
        .and_then(|value| value.as_str())
        .map(str::to_string);

    for issuer in issuers {
        let Ok(base) = url::Url::parse(&issuer) else {
            continue;
        };
        let prefix = base.path().trim_end_matches('/');
        let base_origin = base.origin().ascii_serialization();
        let candidates = [
            format!("{base_origin}/.well-known/oauth-authorization-server{prefix}"),
            format!("{base_origin}/.well-known/oauth-authorization-server"),
            format!("{base_origin}/.well-known/openid-configuration{prefix}"),
            format!("{base_origin}/.well-known/openid-configuration"),
        ];
        let mut seen = std::collections::HashSet::new();
        for candidate in candidates {
            if !seen.insert(candidate.clone()) {
                continue;
            }
            let Some(metadata) = fetch_json(client, &candidate).await else {
                continue;
            };
            let authorization_endpoint = metadata
                .get("authorization_endpoint")
                .and_then(|value| value.as_str());
            let token_endpoint = metadata
                .get("token_endpoint")
                .and_then(|value| value.as_str());
            let (Some(authorization_endpoint), Some(token_endpoint)) =
                (authorization_endpoint, token_endpoint)
            else {
                continue;
            };
            let advertised_offline = metadata
                .get("scopes_supported")
                .and_then(|value| value.as_array())
                .is_some_and(|scopes| {
                    scopes
                        .iter()
                        .any(|scope| scope.as_str() == Some("offline_access"))
                });
            let scope = if advertised_offline {
                let protected_scopes = protected_resource
                    .as_ref()
                    .and_then(|value| value.get("scopes_supported"))
                    .and_then(|value| value.as_array());
                let mut scopes = protected_scopes
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|| {
                        scope
                            .as_deref()
                            .map(|value| {
                                value
                                    .split_whitespace()
                                    .map(str::to_string)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    });
                if !scopes.iter().any(|value| value == "offline_access") {
                    scopes.push("offline_access".into());
                }
                Some(scopes.join(" "))
            } else {
                scope
            };
            return Ok(DiscoveredAuthServer {
                authorization_endpoint: authorization_endpoint.to_string(),
                token_endpoint: token_endpoint.to_string(),
                registration_endpoint: metadata
                    .get("registration_endpoint")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                resource,
                scope,
            });
        }
    }
    Err(format!("MCP OAuth discovery failed for {server_url}"))
}

pub async fn register_client(
    client: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
    client_name: &str,
    scope: Option<&str>,
) -> Result<(String, Option<String>), String> {
    let mut registration = serde_json::json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if let Some(scope) = scope.filter(|scope| !scope.is_empty()) {
        registration["scope"] = serde_json::Value::String(scope.to_string());
    }
    let response = client
        .post(registration_endpoint)
        .header("accept", "application/json")
        .json(&registration)
        .timeout(DISCOVERY_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("MCP OAuth client registration failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "MCP OAuth client registration failed (HTTP {status})"
        ));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("MCP OAuth registration response is not JSON: {error}"))?;
    let client_id = body
        .get("client_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "MCP OAuth registration returned no client_id".to_string())?;
    Ok((
        client_id.to_string(),
        body.get("client_secret")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    ))
}

pub fn authorization_url(
    endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
    resource: &str,
    scope: Option<&str>,
) -> Result<String, String> {
    let mut url = url::Url::parse(endpoint)
        .map_err(|error| format!("Invalid authorization endpoint: {error}"))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("resource", resource);
        if let Some(scope) = scope.filter(|value| !value.is_empty()) {
            query.append_pair("scope", scope);
        }
    }
    Ok(url.to_string())
}

pub fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64_url(&digest)
}

fn base64_url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

const SUCCESS_HTML: &str = concat!(
    "<!doctype html><html><head><meta charset=\"utf-8\"><title>Authorized</title></head>",
    "<body style=\"font-family:system-ui,sans-serif;padding:2rem;\">",
    "<h1>Sign-in complete</h1>",
    "<p>You can close this tab and return to the application.</p>",
    "</body></html>"
);

const ERROR_HTML: &str = concat!(
    "<!doctype html><html><head><meta charset=\"utf-8\"><title>OAuth error</title></head>",
    "<body style=\"font-family:system-ui,sans-serif;padding:2rem;\">",
    "<h1>Sign-in failed</h1>",
    "<p>The authorization server reported an error. Return to the application for details.</p>",
    "</body></html>"
);

#[derive(Debug, Clone)]
pub struct CallbackResult {
    pub code: String,
    pub state: Option<String>,
}

pub struct CallbackListener {
    redirect_uri: String,
    outcome: watch::Receiver<Option<Result<CallbackResult, String>>>,
    cancel: tokio_util::sync::CancellationToken,
}

impl CallbackListener {
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    pub fn cancellation_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel.clone()
    }

    pub async fn wait(&mut self) -> Result<CallbackResult, String> {
        loop {
            if let Some(result) = self.outcome.borrow().clone() {
                return result;
            }
            self.outcome
                .changed()
                .await
                .map_err(|_| "OAuth callback listener closed".to_string())?;
        }
    }
}

impl Drop for CallbackListener {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub async fn start_callback_listener() -> Result<CallbackListener, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| format!("failed to start OAuth callback listener: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("failed to start OAuth callback listener: {error}"))?;
    let expected_host = address.to_string();
    let redirect_uri = format!("http://{expected_host}/callback");
    let (outcome_tx, outcome_rx) = watch::channel(None);
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_task = cancel.clone();
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = cancel_task.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            let Ok((mut socket, _)) = accepted else {
                break;
            };
            let handled = tokio::select! {
                _ = cancel_task.cancelled() => break,
                handled = handle_callback_request(&mut socket, &expected_host) => handled,
            };
            if let Some(outcome) = handled {
                outcome_tx.send_replace(Some(outcome));
                break;
            }
        }
    });
    Ok(CallbackListener {
        redirect_uri,
        outcome: outcome_rx,
        cancel,
    })
}

async fn handle_callback_request(
    socket: &mut tokio::net::TcpStream,
    expected_host: &str,
) -> Option<Result<CallbackResult, String>> {
    use tokio::io::AsyncReadExt;

    let mut buffer = vec![0u8; 16 * 1024];
    let mut total = 0;
    let header_end = loop {
        let read = socket.read(&mut buffer[total..]).await.ok()?;
        if read == 0 {
            return None;
        }
        total += read;
        if let Some(position) = buffer[..total]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            break position;
        }
        if total == buffer.len() {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = head.lines();
    let request = lines.next().unwrap_or_default();
    let mut request_parts = request.split_whitespace();
    let method = request_parts.next().unwrap_or_default();
    let target = request_parts.next().unwrap_or_default();
    let host = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("host")
            .then(|| value.trim().to_string())
    });
    if method != "GET" || host.as_deref() != Some(expected_host) {
        write_callback_response(socket, "404 Not Found", "", "")
            .await
            .ok()?;
        return None;
    }
    let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else {
        write_callback_response(socket, "404 Not Found", "", "")
            .await
            .ok()?;
        return None;
    };
    if url.path() != "/callback" {
        write_callback_response(socket, "404 Not Found", "", "")
            .await
            .ok()?;
        return None;
    }
    if let Some((_, error)) = url.query_pairs().find(|(key, _)| key == "error") {
        let description = url
            .query_pairs()
            .find(|(key, _)| key == "error_description")
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default();
        write_callback_response(
            socket,
            "400 Bad Request",
            "text/html; charset=utf-8",
            ERROR_HTML,
        )
        .await
        .ok()?;
        let suffix = if description.is_empty() {
            String::new()
        } else {
            format!(" — {description}")
        };
        return Some(Err(format!("OAuth error: {error}{suffix}")));
    }
    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned());
    let Some(code) = code.filter(|code| !code.is_empty()) else {
        write_callback_response(
            socket,
            "400 Bad Request",
            "text/html; charset=utf-8",
            ERROR_HTML,
        )
        .await
        .ok()?;
        return Some(Err("OAuth callback missing authorization code".into()));
    };
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned());
    write_callback_response(socket, "200 OK", "text/html; charset=utf-8", SUCCESS_HTML)
        .await
        .ok()?;
    Some(Ok(CallbackResult { code, state }))
}

async fn write_callback_response(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let content_type = if content_type.is_empty() {
        String::new()
    } else {
        format!("Content-Type: {content_type}\r\n")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await
}

fn random_b64url(len: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("MCP OAuth could not obtain randomness: {error}"))?;
    Ok(base64_url(&bytes))
}

async fn fetch_json(client: &reqwest::Client, url: &str) -> Option<serde_json::Value> {
    let response = client
        .get(url)
        .header("accept", "application/json")
        .timeout(DISCOVERY_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

fn prune_expired(pending: &mut HashMap<String, PendingAuthCodeFlow>) {
    pending.retain(|_, flow| !flow.expired());
}

fn prune_results(results: &mut HashMap<String, (AuthCodeStatus, Instant)>) {
    results.retain(|_, (_, at)| at.elapsed() <= RESULT_TTL);
}

#[cfg(test)]
pub(crate) mod test_helpers;
#[cfg(test)]
mod tests;
