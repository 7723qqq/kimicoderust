use super::test_helpers::spawn_mock_as;
use super::*;

/// Discovery to registration to PKCE to code exchange, end to end against a
/// mock AS. The authorization URL carries every parameter the AS needs, and the
/// token request replays the verifier and the resource indicator.
#[tokio::test]
async fn discovers_registers_and_exchanges_a_code() {
    let (base, log) = spawn_mock_as().await;
    let client = reqwest::Client::new();
    let registry = AuthCodeRegistry::default();
    let server_url = format!("{base}/mcp");
    let redirect_uri = format!("{base}/callback");

    let start = registry
        .begin(
            &client,
            "srv",
            &server_url,
            &redirect_uri,
            None,
            "Kimi Code",
        )
        .await
        .expect("begin");
    let url = url::Url::parse(&start.authorization_url).expect("authorization url");
    assert_eq!(url.path(), "/authorize");
    let params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "client-abc");
    assert_eq!(params["redirect_uri"], redirect_uri);
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(params["code_challenge"].len() >= 43);
    assert_eq!(params["state"], start.state);
    assert_eq!(params["resource"], server_url);

    let flow = registry
        .take_by_state(&start.state)
        .await
        .expect("the pending flow");
    assert_eq!(flow.client_secret.as_deref(), Some("secret-1"));
    assert!(
        registry.take_by_state(&start.state).await.is_none(),
        "a callback state is single use"
    );

    let tokens = exchange_code(&client, &flow, "the-code")
        .await
        .expect("token exchange");
    assert_eq!(tokens.access_token, "at-1");
    assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
    assert_eq!(tokens.client_id.as_deref(), Some("client-abc"));
    assert_eq!(tokens.client_secret.as_deref(), Some("secret-1"));
    assert_eq!(tokens.resource.as_deref(), Some(server_url.as_str()));
    assert!(tokens.expires_at_ms.is_some());
    assert!(tokens.obtained_at_ms.is_some());

    let requests = log.lock().await;
    let token_request = requests
        .iter()
        .find(|request| request.starts_with("POST /token "))
        .expect("the token endpoint was called");
    assert!(token_request.contains("grant_type=authorization_code"));
    assert!(token_request.contains("code=the-code"));
    assert!(token_request.contains("client_secret=secret-1"));
    assert!(token_request.contains("code_verifier="));
    assert!(token_request.contains("resource="));
}

/// Polling a flow reports pending until the callback finishes it, and an
/// unknown id is a finished-with-error answer rather than "still pending"
/// forever.
#[tokio::test]
async fn status_reports_pending_then_the_result() {
    let (base, _log) = spawn_mock_as().await;
    let client = reqwest::Client::new();
    let registry = AuthCodeRegistry::default();
    let start = registry
        .begin(
            &client,
            "srv",
            &format!("{base}/mcp"),
            &format!("{base}/callback"),
            None,
            "Kimi Code",
        )
        .await
        .expect("begin");

    assert!(!registry.status(&start.flow_id).await.done);

    registry
        .finish(
            &start.flow_id,
            AuthCodeStatus {
                done: true,
                authenticated: true,
                key: Some("srv-abcd".into()),
                error: None,
            },
        )
        .await;
    let status = registry.status(&start.flow_id).await;
    assert!(status.done && status.authenticated);
    assert_eq!(status.key.as_deref(), Some("srv-abcd"));
    assert!(
        registry.take_by_state(&start.state).await.is_none(),
        "finishing drops the pending entry"
    );

    let unknown = registry.status("no-such-flow").await;
    assert!(unknown.done);
    assert!(!unknown.authenticated);
    assert!(unknown.error.unwrap().contains("unknown or expired"));
}

/// The RFC 7636 appendix B verifier/challenge pair.
#[test]
fn pkce_challenge_matches_the_rfc_example() {
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

/// A server with no AS metadata is refused with the discovery error.
#[tokio::test]
async fn discovery_failure_is_reported() {
    let client = reqwest::Client::new();
    let registry = AuthCodeRegistry::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = [0u8; 4096];
                let _ = sock.read(&mut buffer).await;
                let response = "HTTP/1.1 404 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });
    let error = registry
        .begin(
            &client,
            "srv",
            &format!("http://{addr}/mcp"),
            &format!("http://{addr}/callback"),
            None,
            "Kimi Code",
        )
        .await
        .expect_err("no metadata");
    assert!(error.contains("discovery failed"), "{error}");
}
