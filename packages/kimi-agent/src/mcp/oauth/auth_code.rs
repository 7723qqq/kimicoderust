//! MCP OAuth 2.1 authorization-code + PKCE for the standalone server.
//!
//! The native host runs the browser round trip itself (`mcp-oauth.ts`): a
//! loopback listener in the same process that started the login. A standalone
//! server has no JS host, so it hosts the redirect as an HTTP route and does
//! the exchange here. Discovery (RFC 9728 / RFC 8414 / OIDC), dynamic client
//! registration (RFC 7591), PKCE S256 and the RFC 8707 resource indicator
//! mirror that host flow.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::{McpOAuthTokens, canonical_mcp_oauth_resource};

/// Bound on one discovery/registration/token HTTP call.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// A pending flow expires after this long; the login has to finish inside one
/// sitting (v2's host flow uses 5 minutes for the callback, 15 here because a
/// headless server may have to wait for a human to find the URL).
const FLOW_TTL: Duration = Duration::from_secs(15 * 60);

/// A finished flow's result stays pollable this long.
const RESULT_TTL: Duration = Duration::from_secs(10 * 60);

/// The endpoints discovery found for one MCP server's authorization server.
#[derive(Debug, Clone)]
pub struct DiscoveredAuthServer {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    /// RFC 8707 resource indicator: the MCP server URL without its fragment.
    pub resource: String,
    /// Scope the protected-resource metadata advertises, when it does.
    pub scope: Option<String>,
}

/// One started authorization-code flow, held until the browser comes back.
#[derive(Debug, Clone)]
pub struct PendingAuthCodeFlow {
    pub flow_id: String,
    /// The MCP server's name (the credential store key's first half).
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

/// The `auth:begin-auth-code` response.
#[derive(Debug, Clone, Serialize)]
pub struct AuthCodeStart {
    pub flow_id: String,
    pub authorization_url: String,
    pub state: String,
    pub redirect_uri: String,
    pub expires_in: u64,
}

/// What polling `auth:complete-auth-code` learns about one flow.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AuthCodeStatus {
    /// The callback has run (successfully or not).
    pub done: bool,
    pub authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Pending flows, shared by the begin/complete/cancel routes and the callback.
///
/// Keyed two ways: the browser only knows `state`, the client polling for the
/// result only knows `flow_id`. Results outlive the pending entry so a client
/// that polls after the exchange still sees the outcome.
#[derive(Default)]
pub struct AuthCodeRegistry {
    pending_by_state: Mutex<HashMap<String, PendingAuthCodeFlow>>,
    state_by_flow: Mutex<HashMap<String, String>>,
    results: Mutex<HashMap<String, (AuthCodeStatus, Instant)>>,
}

impl AuthCodeRegistry {
    /// Discover, register a public PKCE client, and mint one flow.
    ///
    /// `redirect_uri` must point back at this server's callback route; the
    /// route handler derives it from the request's `Host` unless the caller
    /// supplies one (an HTTPS proxy needs the caller's spelling).
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
        let (client_id, client_secret) =
            register_client(client, registration_endpoint, redirect_uri, client_name).await?;
        let code_verifier = random_b64url(64)?;
        let code_challenge = pkce_challenge(&code_verifier);
        let state = random_b64url(32)?;
        let flow_id = random_b64url(16)?;
        let scope = scope
            .map(str::to_string)
            .or_else(|| discovered.scope.clone());
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

    /// Take the pending flow the browser's `state` names. Single use: the
    /// entry is removed here, so a replayed callback cannot exchange twice.
    pub async fn take_by_state(&self, state: &str) -> Option<PendingAuthCodeFlow> {
        let mut pending = self.pending_by_state.lock().await;
        prune_expired(&mut pending);
        let flow = pending.remove(state)?;
        self.state_by_flow.lock().await.remove(&flow.flow_id);
        Some(flow)
    }

    /// Record the outcome of a callback and drop any still-pending entry.
    pub async fn finish(&self, flow_id: &str, status: AuthCodeStatus) {
        if let Some(state) = self.state_by_flow.lock().await.remove(flow_id) {
            self.pending_by_state.lock().await.remove(&state);
        }
        let mut results = self.results.lock().await;
        prune_results(&mut results);
        results.insert(flow_id.to_string(), (status, Instant::now()));
    }

    /// Poll one flow: pending, succeeded, or failed (including "unknown").
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

    /// Drop a pending flow (the client cancelled the login).
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

/// Exchange one callback code and return the flow plus the grant to persist.
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

/// RFC 9728 protected-resource metadata (optional), then RFC 8414 / OIDC
/// authorization-server metadata. The path-aware well-known variant is tried
/// before the origin-level one, so an AS mounted under a path is found.
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

/// RFC 7591 dynamic client registration for a public PKCE client.
pub async fn register_client(
    client: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
    client_name: &str,
) -> Result<(String, Option<String>), String> {
    let response = client
        .post(registration_endpoint)
        .header("accept", "application/json")
        .json(&serde_json::json!({
            "client_name": client_name,
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
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

/// The authorization URL the browser opens.
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

/// PKCE S256 challenge for one verifier (RFC 7636 §4.2).
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64_url(&digest)
}

fn base64_url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE verifiers and callback states must be unpredictable: a failed RNG
/// fails the login rather than falling back to any deterministic value.
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
