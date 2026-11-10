use std::sync::Arc;

use tokio::sync::{Mutex, Notify};

pub(crate) async fn spawn_mock_as() -> (String, Arc<Mutex<Vec<String>>>) {
    spawn_mock_as_inner(None, false).await
}

pub(crate) async fn spawn_offline_mock_as() -> (String, Arc<Mutex<Vec<String>>>) {
    spawn_mock_as_inner(None, true).await
}

pub(crate) async fn spawn_gated_exchange_as()
-> (String, Arc<Mutex<Vec<String>>>, Arc<Notify>, Arc<Notify>) {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (base, log) = spawn_mock_as_inner(Some((started.clone(), release.clone())), false).await;
    (base, log, started, release)
}

async fn spawn_mock_as_inner(
    exchange_gate: Option<(Arc<Notify>, Arc<Notify>)>,
    offline: bool,
) -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let log = Arc::new(Mutex::new(Vec::new()));
    let log_task = log.clone();
    let task_base = base.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let base = task_base.clone();
            let log = log_task.clone();
            let exchange_gate = exchange_gate.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 32768];
                let mut read = 0usize;
                loop {
                    let Ok(n) = sock.read(&mut buffer[read..]).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    read += n;
                    let text = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let Some(header_end) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let content_length = text[..header_end]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if read >= header_end + 4 + content_length {
                        break;
                    }
                    if read == buffer.len() {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                log.lock().await.push(request.clone());
                let (status, body) = match path.as_str() {
                    "/.well-known/oauth-protected-resource/mcp" if offline => (
                        200,
                        format!(
                            r#"{{"authorization_servers":["{base}"],"scopes_supported":["openid"]}}"#
                        ),
                    ),
                    "/.well-known/oauth-authorization-server" => (
                        200,
                        if offline {
                            format!(
                                r#"{{"authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","registration_endpoint":"{base}/register","scopes_supported":["openid","offline_access"]}}"#
                            )
                        } else {
                            format!(
                                r#"{{"authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","registration_endpoint":"{base}/register"}}"#
                            )
                        },
                    ),
                    "/register" => (
                        200,
                        r#"{"client_id":"client-abc","client_secret":"secret-1"}"#.to_string(),
                    ),
                    "/token" => (
                        200,
                        r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#
                            .to_string(),
                    ),
                    _ => (404, "{}".to_string()),
                };
                if path == "/token"
                    && let Some((started, release)) = exchange_gate
                {
                    started.notify_one();
                    release.notified().await;
                }
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    (base, log)
}
