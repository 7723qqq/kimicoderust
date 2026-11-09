use super::super::*;
use std::collections::HashMap;

#[tokio::test]
async fn shutdown_route_cancels_the_serve_token() {
    let server = HttpServer::in_memory().unwrap();
    let token = server.shutdown_token();
    assert!(!token.is_cancelled());

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/shutdown".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
    assert!(
        token.is_cancelled(),
        "POST /shutdown must stop the accept loop"
    );
}

#[test]
fn loopback_hosts_are_recognized_and_wildcards_are_not() {
    for host in ["127.0.0.1", "127.0.0.53", "localhost", "::1", "[::1]"] {
        assert!(is_loopback_host(host), "{host} is loopback");
    }
    for host in ["0.0.0.0", "::", "192.168.1.5", "kimi.internal", ""] {
        assert!(!is_loopback_host(host), "{host} is not loopback");
    }
}

#[tokio::test]
async fn debug_routes_need_the_flag_and_a_loopback_bind() {
    let request = || HttpRequest {
        method: "GET".into(),
        path: "/api/v1/debug/channels".into(),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };

    // Off by default: the reflection surface is not a product route.
    let off = HttpServer::in_memory().unwrap();
    assert_eq!(off.handle_request(&request()).await.status, 404);

    // Asked for, on loopback: mounted.
    let on = HttpServer::in_memory().unwrap().with_debug_endpoints(true);
    assert_eq!(on.handle_request(&request()).await.status, 200);

    // Asked for, but reachable from the network: still not mounted.
    let remote = HttpServer::in_memory()
        .unwrap()
        .with_debug_endpoints(true)
        .with_bind_host("0.0.0.0");
    assert_eq!(remote.handle_request(&request()).await.status, 404);
}

#[tokio::test]
async fn shutdown_is_unregistered_on_a_non_loopback_bind() {
    let request = || HttpRequest {
        method: "POST".into(),
        path: "/api/v1/shutdown".into(),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };

    let remote = HttpServer::in_memory().unwrap().with_bind_host("0.0.0.0");
    let token = remote.shutdown_token();
    assert_eq!(remote.handle_request(&request()).await.status, 404);
    assert!(!token.is_cancelled(), "a 404 must not stop the server");

    let opted_in = HttpServer::in_memory()
        .unwrap()
        .with_bind_host("0.0.0.0")
        .with_allow_remote_shutdown(true);
    let token = opted_in.shutdown_token();
    assert_eq!(opted_in.handle_request(&request()).await.status, 200);
    assert!(token.is_cancelled());
}

#[tokio::test]
async fn a_host_outside_the_allowlist_is_refused_before_the_credential() {
    let server = HttpServer::in_memory()
        .unwrap()
        .with_host_guard(HostGuard::new(
            "127.0.0.1",
            vec!["kimi.example".to_string()],
        ));

    let with_host = |host: Option<&str>| {
        let mut headers = HashMap::new();
        if let Some(host) = host {
            headers.insert("host".to_string(), host.to_string());
        }
        HttpRequest {
            method: "GET".into(),
            path: "/api/v1/healthz".into(),
            query: None,
            headers,
            body: Vec::new(),
        }
    };

    // `/health` answers unauthenticated, so a 403 here can only come from
    // the host guard.
    assert_eq!(
        server
            .handle_request(&with_host(Some("127.0.0.1:58627")))
            .await
            .status,
        200
    );
    assert_eq!(
        server
            .handle_request(&with_host(Some("kimi.example")))
            .await
            .status,
        200
    );

    let refused = server
        .handle_request(&with_host(Some("evil.example")))
        .await;
    assert_eq!(refused.status, 403);
    let body = String::from_utf8_lossy(&refused.body);
    assert!(body.contains("--allowed-host"), "{body}");
    assert!(body.contains("KIMI_CODE_ALLOWED_HOSTS"), "{body}");

    // A missing `Host` is refused too: HTTP/1.1 requires it.
    assert_eq!(server.handle_request(&with_host(None)).await.status, 403);
}

#[tokio::test]
async fn an_in_process_server_without_a_guard_ignores_the_host_header() {
    let server = HttpServer::in_memory().unwrap();
    let mut headers = HashMap::new();
    headers.insert("host".to_string(), "evil.example".to_string());
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/healthz".into(),
            query: None,
            headers,
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
}
