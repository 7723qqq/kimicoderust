use std::sync::Arc;

use tokio::sync::Mutex;

/// A minimal authorization server: origin-level AS metadata, dynamic
/// registration, and a token endpoint. The protected-resource well-known
/// answers 404, which exercises the fallback to the origin-level metadata.
pub(crate) async fn spawn_mock_as() -> (String, Arc<Mutex<Vec<String>>>) {
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
                    "/.well-known/oauth-authorization-server" => (
                        200,
                        format!(
                            r#"{{"authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","registration_endpoint":"{base}/register"}}"#
                        ),
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
