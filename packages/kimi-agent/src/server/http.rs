use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};

use crate::server::HttpServer;
use crate::server::router::{HttpRequest, HttpResponse};
use crate::server::ws;

const MAX_HEADER_BYTES: usize = 16 * 1024;

const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

const READ_TIMEOUT: Duration = Duration::from_secs(30);

const CHUNK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectReason {
    Malformed(&'static str),
    HeaderTooLarge,
    TransferEncoding,
    ContentLengthInvalid,
    BodyTooLarge,
    OriginFormRequired,
}

impl RejectReason {
    fn message(&self) -> &'static str {
        match self {
            Self::Malformed(what) => what,
            Self::HeaderTooLarge => "header block exceeds the 16 KiB limit",
            Self::TransferEncoding => "Transfer-Encoding is not supported",
            Self::ContentLengthInvalid => "Content-Length is not a valid integer",
            Self::BodyTooLarge => "body exceeds the 4 MiB limit",
            Self::OriginFormRequired => "origin-form request target is required",
        }
    }
}

pub struct ServerHandle {
    pub local_addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    pub fn shutdown(self) {
        self.task.abort();
    }
}

pub async fn serve(addr: &str, server: Arc<HttpServer>) -> io::Result<ServerHandle> {
    let host = addr.rsplit_once(':').map_or(addr, |(host, _)| host);
    if !crate::server::is_loopback_host(host) && server.auth().is_disabled() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to bind {addr} without authentication"),
        ));
    }

    server.install_tower_wake_driver();
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;

    let shutdown = server.shutdown_token();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _peer)) => {
                            let server = server.clone();
                            tokio::spawn(async move {
                                let _ = serve_connection(stream, server).await;
                            });
                        }
                        Err(error) => {


                            tracing::warn!(%error, "accept failed");
                        }
                    }
                }
            }
        }
    });

    Ok(ServerHandle { local_addr, task })
}

async fn serve_connection(mut stream: TcpStream, server: Arc<HttpServer>) -> io::Result<()> {
    let received = match timeout(READ_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(Some(received))) => received,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(reason)) => {
            return write_response(&mut stream, &HttpResponse::bad_request(reason.message())).await;
        }
        Err(_elapsed) => {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "read timeout"));
        }
    };

    if ws::is_upgrade(&received.request) {
        let Received { request, leftover } = received;

        if !server.host_allowed(request.header("host")) {
            let response = HttpResponse::forbidden(crate::server::host_guard::rejection_message(
                request.header("host"),
            ));
            return write_response(&mut stream, &response).await;
        }
        let (decision, selected_protocol) = server.auth().check_upgrade(
            request.header("authorization"),
            request.header("sec-websocket-protocol"),
        );
        if !decision.is_allowed() {
            let response = HttpResponse::unauthorized("Unauthorized");
            return write_response(&mut stream, &response).await;
        }
        return match ws::serve_ws(
            stream,
            &request,
            leftover,
            ws::WsOptions {
                hub: server.hub(),
                auth: server.auth(),
                heartbeat: server.heartbeat(),
                selected_protocol,
                store: Some(server.store_arc()),
                engine: server.engine(),
                terminal_manager: Some(server.terminal_manager()),
                prompt_queue: Some(server.prompt_queue.clone()),
            },
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(ws::WsError::Io(error)) => Err(error),
            Err(error) => {
                tracing::debug!(?error, "websocket connection closed");
                Ok(())
            }
        };
    }

    let response = server.handle_request(&received.request).await;

    let response =
        crate::server::envelope::envelope_response(&received.request.request_id(), response);
    write_response(&mut stream, &response).await
}

struct Received {
    request: HttpRequest,
    leftover: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> Result<Option<Received>, RejectReason> {
    let mut buffered: Vec<u8> = Vec::with_capacity(CHUNK_BYTES);
    let mut chunk = vec![0_u8; CHUNK_BYTES];

    let header_end = loop {
        if let Some(position) = find_header_end(&buffered) {
            break position;
        }
        if buffered.len() >= MAX_HEADER_BYTES {
            return Err(RejectReason::HeaderTooLarge);
        }
        let read = stream.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return if buffered.is_empty() {
                Ok(None)
            } else {
                Err(RejectReason::Malformed("connection closed mid-request"))
            };
        }
        buffered.extend_from_slice(&chunk[..read]);
    };

    let (head, remainder) = buffered.split_at(header_end + 4);
    let mut request = parse_head(head)?;
    let body_limit = if request.path == "/api/v1/files" {
        MAX_UPLOAD_BYTES
    } else {
        MAX_BODY_BYTES
    };
    let expected = declared_body_len(&request.headers)?;
    if expected > body_limit {
        return Err(RejectReason::BodyTooLarge);
    }

    let mut body = remainder.to_vec();
    while body.len() < expected {
        let read = stream.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return Err(RejectReason::Malformed("connection closed mid-body"));
        }
        body.extend_from_slice(&chunk[..read]);
        if body.len() > body_limit {
            return Err(RejectReason::BodyTooLarge);
        }
    }

    let leftover = if body.len() > expected {
        body.split_off(expected)
    } else {
        Vec::new()
    };
    request.body = body;
    Ok(Some(Received { request, leftover }))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn has_bare_line_feed(buffer: &[u8]) -> bool {
    buffer.iter().enumerate().any(|(index, byte)| match byte {
        b'\n' => index == 0 || buffer[index - 1] != b'\r',
        b'\r' => buffer.get(index + 1) != Some(&b'\n'),
        _ => false,
    })
}

fn parse_head(head: &[u8]) -> Result<HttpRequest, RejectReason> {
    if has_bare_line_feed(head) {
        return Err(RejectReason::Malformed(
            "bare LF or lone CR in header block",
        ));
    }

    let mut header_slots = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut header_slots);

    match parsed.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => {
            return Err(RejectReason::Malformed("incomplete request head"));
        }
        Err(_) => return Err(RejectReason::Malformed("malformed request line or headers")),
    }

    let target = parsed
        .path
        .ok_or(RejectReason::Malformed("missing request target"))?;
    let (path, query) = match target.split_once(['?', '#']) {
        Some((path, q)) => (path, Some(q.to_string())),
        None => (target, None),
    };
    if !path.starts_with('/') {
        return Err(RejectReason::OriginFormRequired);
    }

    let method = parsed.method.unwrap_or("GET").to_ascii_uppercase();
    let mut headers = HashMap::new();
    for header in parsed.headers {
        headers.insert(
            header.name.to_ascii_lowercase(),
            String::from_utf8_lossy(header.value).into_owned(),
        );
    }

    Ok(HttpRequest {
        method,
        path: path.to_string(),
        query,
        headers,
        body: Vec::new(),
    })
}

fn declared_body_len(headers: &HashMap<String, String>) -> Result<usize, RejectReason> {
    if headers.contains_key("transfer-encoding") {
        return Err(RejectReason::TransferEncoding);
    }
    match headers.get("content-length") {
        None => Ok(0),
        Some(value) => value
            .trim()
            .parse::<usize>()
            .map_err(|_| RejectReason::ContentLengthInvalid),
    }
}

async fn write_response(stream: &mut TcpStream, response: &HttpResponse) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason_phrase(response.status),
        response.body.len(),
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.flush().await?;
    stream.shutdown().await
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Unknown Status",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::auth::ServerAuth;

    fn parse(head: &[u8]) -> Result<HttpRequest, RejectReason> {
        parse_head(head)
    }

    #[test]
    fn accepts_a_plain_origin_form_get() {
        let request = parse(b"GET /api/v1/healthz HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/api/v1/healthz");
        assert_eq!(request.headers.get("host").map(String::as_str), Some("x"));
    }

    #[test]
    fn lowercases_and_uppercases_for_case_insensitive_matching() {
        let request = parse(b"get /a HTTP/1.1\r\nCONTENT-LENGTH: 0\r\n\r\n").unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.headers.get("content-length").map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn strips_the_query_string_from_the_dispatched_path() {
        let request = parse(b"GET /api/v1/sessions?limit=1 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(request.path, "/api/v1/sessions");
    }

    #[test]
    fn rejects_a_bare_lf_line_ending() {
        let error = parse(b"GET /a HTTP/1.1\nHost: x\r\n\r\n").unwrap_err();
        assert!(matches!(error, RejectReason::Malformed(_)), "{error:?}");
    }

    #[test]
    fn rejects_an_absolute_form_target() {
        let error = parse(b"GET http://example.test/a HTTP/1.1\r\n\r\n").unwrap_err();
        assert_eq!(error, RejectReason::OriginFormRequired);
    }

    #[test]
    fn more_than_64_headers_is_malformed_not_silently_truncated() {
        let mut head = b"GET /a HTTP/1.1\r\n".to_vec();
        for index in 0..70 {
            head.extend_from_slice(format!("X-T{index}: v\r\n").as_bytes());
        }
        head.extend_from_slice(b"\r\n");
        assert!(matches!(
            parse(&head).unwrap_err(),
            RejectReason::Malformed(_)
        ));
    }

    #[test]
    fn transfer_encoding_is_refused_rather_than_interpreted() {
        let mut headers = HashMap::new();
        headers.insert("transfer-encoding".into(), "chunked".into());
        assert_eq!(
            declared_body_len(&headers).unwrap_err(),
            RejectReason::TransferEncoding
        );
    }

    #[test]
    fn content_length_must_be_a_number() {
        let mut headers = HashMap::new();
        headers.insert("content-length".into(), "abc".into());
        assert_eq!(
            declared_body_len(&headers).unwrap_err(),
            RejectReason::ContentLengthInvalid
        );
        headers.insert("content-length".into(), "12".into());
        assert_eq!(declared_body_len(&headers).unwrap(), 12);
        assert_eq!(declared_body_len(&HashMap::new()).unwrap(), 0);
    }

    #[tokio::test]
    async fn serves_a_real_request_over_a_real_socket() {
        let server = Arc::new(HttpServer::in_memory().unwrap());
        let handle = serve("127.0.0.1:0", server).await.unwrap();

        let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
        stream
            .write_all(b"GET /api/v1/healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8_lossy(&response).into_owned();

        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("Connection: close"), "{text}");
        assert!(text.contains("\"ok\":true"), "{text}");

        handle.shutdown();
    }

    #[tokio::test]
    async fn posts_a_session_over_a_real_socket() {
        let server = Arc::new(HttpServer::in_memory().unwrap());
        let handle = serve("127.0.0.1:0", server).await.unwrap();

        let body = br#"{"title":"over TCP"}"#;
        let mut request = format!(
            "POST /api/v1/sessions HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);

        let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
        stream.write_all(&request).await.unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8_lossy(&response).into_owned();

        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("over TCP"), "{text}");

        let mut second = TcpStream::connect(handle.local_addr).await.unwrap();
        second
            .write_all(b"GET /api/v1/sessions HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut listed = Vec::new();
        second.read_to_end(&mut listed).await.unwrap();
        assert!(
            String::from_utf8_lossy(&listed).contains("over TCP"),
            "session did not survive the request"
        );

        handle.shutdown();
    }

    #[tokio::test]
    async fn refuses_chunked_before_touching_any_route() {
        let server = Arc::new(HttpServer::in_memory().unwrap());
        let handle = serve("127.0.0.1:0", server).await.unwrap();

        let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
        stream
            .write_all(
                b"POST /api/v1/sessions HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{}\r\n0\r\n\r\n",
            )
            .await
            .unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8_lossy(&response).into_owned();

        assert!(text.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{text}");
        assert!(text.contains("Transfer-Encoding"), "{text}");

        handle.shutdown();
    }

    async fn serve_with_token() -> ServerHandle {
        let server = Arc::new(
            HttpServer::in_memory()
                .unwrap()
                .with_auth(ServerAuth::Token(Arc::from("tok3n"))),
        );
        serve("127.0.0.1:0", server).await.unwrap()
    }

    async fn exchange(addr: std::net::SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    fn upgrade_request(protocol: &str) -> String {
        format!(
            "GET /api/v1/ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {protocol}\r\n\r\n"
        )
    }

    #[tokio::test]
    async fn a_state_route_refuses_a_missing_credential_in_the_kap_server_shape() {
        let handle = serve_with_token().await;
        let text = exchange(
            handle.local_addr,
            "GET /api/v1/sessions HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;

        assert!(text.starts_with("HTTP/1.1 401 Unauthorized\r\n"), "{text}");
        assert!(
            text.contains("WWW-Authenticate: Bearer realm=\"kimi-code\""),
            "RFC 6750 says a 401 carries a challenge: {text}"
        );
        assert!(text.contains("\"code\":40101"), "{text}");

        handle.shutdown();
    }

    #[tokio::test]
    async fn the_credential_unlocks_state_routes_while_health_stays_open() {
        let handle = serve_with_token().await;

        let listed = exchange(
            handle.local_addr,
            "GET /api/v1/sessions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer tok3n\r\n\r\n",
        )
        .await;
        assert!(listed.starts_with("HTTP/1.1 200 OK\r\n"), "{listed}");

        let wrong = exchange(
            handle.local_addr,
            "GET /api/v1/sessions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer nope\r\n\r\n",
        )
        .await;
        assert!(
            wrong.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "{wrong}"
        );

        let health = exchange(
            handle.local_addr,
            "GET /api/v1/healthz HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(health.starts_with("HTTP/1.1 200 OK\r\n"), "{health}");

        handle.shutdown();
    }

    #[tokio::test]
    async fn a_websocket_upgrade_needs_the_token_and_echoes_the_protocol_it_accepted() {
        let handle = serve_with_token().await;

        let refused = exchange(
            handle.local_addr,
            &upgrade_request("kimi-code.bearer.wrong"),
        )
        .await;
        assert!(
            refused.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "{refused}"
        );
        assert!(
            !refused.contains("Sec-WebSocket-Protocol"),
            "a refusal must not negotiate a protocol: {refused}"
        );

        let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
        stream
            .write_all(upgrade_request("chrome-v1, kimi-code.bearer.tok3n").as_bytes())
            .await
            .unwrap();
        let mut head = [0_u8; 512];
        let read = timeout(Duration::from_secs(5), stream.read(&mut head))
            .await
            .expect("the handshake should not hang")
            .unwrap();
        let accepted = String::from_utf8_lossy(&head[..read]).into_owned();

        assert!(
            accepted.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
            "{accepted}"
        );
        assert!(
            accepted.contains("Sec-WebSocket-Protocol: kimi-code.bearer.tok3n\r\n"),
            "RFC 6455 requires the selected protocol back or the browser aborts: {accepted}"
        );

        handle.shutdown();
    }
}
