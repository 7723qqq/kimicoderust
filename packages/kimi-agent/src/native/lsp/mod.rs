use std::path::Path;
use std::process::Stdio;
use tokio::process::{Child, Command};
use which::which;

pub struct LanguageServerManager;

impl LanguageServerManager {
    pub fn locate_server(language: &str) -> Option<String> {
        let binary_name = match language {
            "typescript" | "javascript" => "typescript-language-server",
            "rust" => "rust-analyzer",
            "python" => "pyright",
            _ => return None,
        };
        which(binary_name)
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    }

    pub fn start_server(binary_path: &str) -> Result<Child, std::io::Error> {
        Command::new(binary_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    }

    pub async fn stop_server(child: &mut Child) -> Result<(), std::io::Error> {
        let _ = child.kill().await;
        let _ = child.wait().await?;
        Ok(())
    }
}

pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub struct LspFraming;

impl LspFraming {
    pub fn encode(payload: &serde_json::Value) -> Result<Vec<u8>, String> {
        let body = serde_json::to_vec(payload).map_err(|error| error.to_string())?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(format!(
                "message body of {} bytes exceeds the {MAX_FRAME_BYTES}-byte frame limit",
                body.len()
            ));
        }
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut msg = Vec::with_capacity(header.len() + body.len());
        msg.extend_from_slice(header.as_bytes());
        msg.extend_from_slice(&body);
        Ok(msg)
    }

    pub fn decode(buffer: &mut Vec<u8>) -> Result<Option<serde_json::Value>, String> {
        let header_end_marker = b"\r\n\r\n";
        let marker_pos = match buffer.windows(4).position(|w| w == header_end_marker) {
            Some(pos) => pos,
            None => {
                if buffer.len() > MAX_FRAME_BYTES {
                    return Err("header exceeds the frame limit without a terminator".into());
                }
                return Ok(None);
            }
        };

        let header_str = std::str::from_utf8(&buffer[..marker_pos])
            .map_err(|e| format!("Invalid UTF-8 in LSP headers: {e}"))?;
        let length = parse_content_length(header_str)?;

        let body_start = marker_pos + 4;
        let body_end = body_start + length;

        if buffer.len() < body_end {
            return Ok(None);
        }

        let body_bytes = &buffer[body_start..body_end];
        let json_val: serde_json::Value = serde_json::from_slice(body_bytes)
            .map_err(|e| format!("Failed to parse LSP JSON payload: {e}"))?;

        buffer.drain(..body_end);
        Ok(Some(json_val))
    }
}

fn parse_content_length(header: &str) -> Result<usize, String> {
    let malformed = || format!("malformed LSP frame header: {header:?}");
    let prefix = header.get(..15).ok_or_else(malformed)?;
    if !prefix.eq_ignore_ascii_case("Content-Length:") {
        return Err(malformed());
    }
    let value = header.get(15..).ok_or_else(malformed)?.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed());
    }
    let length: usize = value
        .parse()
        .map_err(|_| format!("invalid Content-Length: {value}"))?;
    if length > MAX_FRAME_BYTES {
        return Err(format!("invalid Content-Length: {value}"));
    }
    Ok(length)
}

use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct LspClientSession {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    buffer: Vec<u8>,
    next_id: i64,
    pending_notifications: Vec<serde_json::Value>,
}

impl LspClientSession {
    pub fn from_child(mut child: Child) -> Result<Self, std::io::Error> {
        let stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Failed to capture child stdin",
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Failed to capture child stdout",
            )
        })?;

        Ok(Self {
            child,
            stdin,
            stdout,
            buffer: Vec::new(),
            next_id: 1,
            pending_notifications: Vec::new(),
        })
    }

    pub async fn send_notification(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), std::io::Error> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let frame = LspFraming::encode(&payload)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.stdin.write_all(&frame).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    pub async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<i64, std::io::Error> {
        let id = self.next_id;
        self.next_id += 1;

        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let frame = LspFraming::encode(&payload)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.stdin.write_all(&frame).await?;
        self.stdin.flush().await?;
        Ok(id)
    }

    pub async fn read_message(&mut self) -> Result<Option<serde_json::Value>, String> {
        if let Some(msg) = LspFraming::decode(&mut self.buffer)? {
            return Ok(Some(msg));
        }

        let mut temp_buf = [0u8; 4096];
        let bytes_read = self
            .stdout
            .read(&mut temp_buf)
            .await
            .map_err(|e| format!("Failed to read from child stdout: {e}"))?;

        if bytes_read == 0 {
            return Ok(None);
        }

        self.buffer.extend_from_slice(&temp_buf[..bytes_read]);
        LspFraming::decode(&mut self.buffer)
    }

    pub async fn wait_response(
        &mut self,
        target_id: i64,
        timeout_dur: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        let deadline = tokio::time::Instant::now() + timeout_dur;

        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());

            match tokio::time::timeout(remaining, self.read_message()).await {
                Ok(Ok(Some(msg))) => {
                    if msg.get("id").and_then(|id| id.as_i64()) == Some(target_id) {
                        return Ok(msg);
                    } else {
                        self.pending_notifications.push(msg);
                    }
                }
                Ok(Ok(None)) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(format!("Timeout waiting for LSP response ID {target_id}")),
            }
        }

        Err(format!("Timeout waiting for LSP response ID {target_id}"))
    }

    pub fn take_pending_notifications(&mut self) -> Vec<serde_json::Value> {
        std::mem::take(&mut self.pending_notifications)
    }

    pub async fn initialize(
        &mut self,
        root_path: &Path,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        let root_uri = path_to_uri(root_path);
        let params = serde_json::json!({
            "processId": null,
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "definition": { "dynamicRegistration": true },
                    "references": { "dynamicRegistration": true },
                    "hover": { "dynamicRegistration": true },
                    "documentSymbol": { "dynamicRegistration": true },
                    "synchronization": { "dynamicRegistration": true }
                }
            }
        });

        let req_id = self
            .send_request("initialize", params)
            .await
            .map_err(|e| format!("Failed to send initialize request: {e}"))?;

        let response = self.wait_response(req_id, timeout).await?;

        self.send_notification("initialized", serde_json::json!({}))
            .await
            .map_err(|e| format!("Failed to send initialized notification: {e}"))?;

        Ok(response)
    }

    pub async fn notify_did_open(
        &mut self,
        file_path: &Path,
        language_id: &str,
        text: &str,
    ) -> Result<(), std::io::Error> {
        let uri = path_to_uri(file_path);
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri,
                "languageId": language_id,
                "version": 1,
                "text": text,
            }
        });
        self.send_notification("textDocument/didOpen", params).await
    }

    pub async fn goto_definition(
        &mut self,
        file_path: &Path,
        line: u32,
        character: u32,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        let uri = path_to_uri(file_path);
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri,
            },
            "position": {
                "line": line,
                "character": character,
            }
        });

        let req_id = self
            .send_request("textDocument/definition", params)
            .await
            .map_err(|e| format!("Failed to send definition request: {e}"))?;

        self.wait_response(req_id, timeout).await
    }

    pub async fn find_references(
        &mut self,
        file_path: &Path,
        line: u32,
        character: u32,
        include_declaration: bool,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        let uri = path_to_uri(file_path);
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri,
            },
            "position": {
                "line": line,
                "character": character,
            },
            "context": {
                "includeDeclaration": include_declaration,
            }
        });

        let req_id = self
            .send_request("textDocument/references", params)
            .await
            .map_err(|e| format!("Failed to send references request: {e}"))?;

        self.wait_response(req_id, timeout).await
    }

    pub async fn get_hover(
        &mut self,
        file_path: &Path,
        line: u32,
        character: u32,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        let uri = path_to_uri(file_path);
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri,
            },
            "position": {
                "line": line,
                "character": character,
            }
        });

        let req_id = self
            .send_request("textDocument/hover", params)
            .await
            .map_err(|e| format!("Failed to send hover request: {e}"))?;

        self.wait_response(req_id, timeout).await
    }

    pub async fn get_document_symbols(
        &mut self,
        file_path: &Path,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        let uri = path_to_uri(file_path);
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri,
            }
        });

        let req_id = self
            .send_request("textDocument/documentSymbol", params)
            .await
            .map_err(|e| format!("Failed to send documentSymbol request: {e}"))?;

        self.wait_response(req_id, timeout).await
    }

    pub async fn shutdown(mut self) -> Result<(), std::io::Error> {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await?;
        Ok(())
    }
}

pub fn path_to_uri(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    if raw.starts_with('/') {
        format!("file://{raw}")
    } else {
        format!("file:///{raw}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_locate_server_unsupported() {
        assert!(LanguageServerManager::locate_server("brainfuck").is_none());
    }

    #[test]
    fn test_lsp_framing_encode_and_decode_roundtrip() {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "rootUri": "file:///workspace"
            }
        });

        let encoded = LspFraming::encode(&payload).unwrap();
        assert!(encoded.starts_with(b"Content-Length: "));

        let mut buffer = encoded.clone();
        let decoded = LspFraming::decode(&mut buffer).unwrap();
        assert!(decoded.is_some());
        assert_eq!(decoded.unwrap(), payload);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_lsp_framing_packet_fragmentation() {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "text": "fn main() {}" }
        });
        let encoded = LspFraming::encode(&payload).unwrap();

        let mut buffer = Vec::new();

        buffer.extend_from_slice(&encoded[..10]);
        let res1 = LspFraming::decode(&mut buffer).unwrap();
        assert!(res1.is_none());

        buffer.extend_from_slice(&encoded[10..]);
        let res2 = LspFraming::decode(&mut buffer).unwrap();
        assert_eq!(res2, Some(payload));
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_lsp_framing_sticky_packets() {
        let msg1 = json!({ "id": 1, "result": "ok" });
        let msg2 = json!({ "id": 2, "result": "pong" });

        let mut combined = LspFraming::encode(&msg1).unwrap();
        combined.extend_from_slice(&LspFraming::encode(&msg2).unwrap());

        let mut buffer = combined;

        let res1 = LspFraming::decode(&mut buffer).unwrap();
        assert_eq!(res1, Some(msg1));

        let res2 = LspFraming::decode(&mut buffer).unwrap();
        assert_eq!(res2, Some(msg2));

        assert!(buffer.is_empty());
    }

    #[test]
    fn test_lsp_framing_rejects_oversized_body() {
        let payload = json!({ "pad": "x".repeat(MAX_FRAME_BYTES + 1) });
        let error = LspFraming::encode(&payload).unwrap_err();
        assert!(
            error.contains("exceeds the"),
            "the limit is reported, got {error:?}"
        );
        assert!(LspFraming::encode(&json!({ "pad": "x".repeat(1024) })).is_ok());
    }

    #[test]
    fn test_lsp_framing_rejects_oversized_header() {
        let mut buffer = vec![b'C'; MAX_FRAME_BYTES + 1];
        let error = LspFraming::decode(&mut buffer).unwrap_err();
        assert_eq!(error, "header exceeds the frame limit without a terminator");
    }

    #[test]
    fn test_lsp_framing_rejects_malformed_headers() {
        for header in [
            "Content-Type: application/json",
            "Content-Length: abc",
            "Content-Length: -1",
            "Content-Length: 2\r\nContent-Type: application/json",
        ] {
            let mut buffer = format!("{header}\r\n\r\n{{}}").into_bytes();
            let error = LspFraming::decode(&mut buffer).unwrap_err();
            assert!(
                error.starts_with("malformed LSP frame header:"),
                "{header:?} -> {error:?}"
            );
        }
    }

    #[test]
    fn test_lsp_framing_rejects_oversized_content_length() {
        let mut buffer = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1).into_bytes();
        let error = LspFraming::decode(&mut buffer).unwrap_err();
        assert_eq!(
            error,
            format!("invalid Content-Length: {}", MAX_FRAME_BYTES + 1)
        );
    }

    #[test]
    fn test_lsp_framing_accepts_case_insensitive_header_name() {
        let mut buffer = b"content-length: 2\r\n\r\n{}".to_vec();
        assert_eq!(
            LspFraming::decode(&mut buffer).unwrap(),
            Some(json!({}))
        );
    }

    #[test]
    fn test_path_to_uri_conversion() {
        let p1 = Path::new("/workspace/src/lib.rs");
        assert_eq!(path_to_uri(p1), "file:///workspace/src/lib.rs");

        let p2 = Path::new("C:\\Users\\dev\\project\\main.rs");
        let uri2 = path_to_uri(p2);
        assert!(uri2.starts_with("file:///C:/Users/dev/project/main.rs"));
    }

    #[test]
    fn test_lsp_request_framing_extended() {
        let uri = path_to_uri(Path::new("src/main.rs"));
        let ref_params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": 10, "character": 4 },
            "context": { "includeDeclaration": true }
        });
        let req = json!({
            "jsonrpc": "2.0",
            "id": 101,
            "method": "textDocument/references",
            "params": ref_params
        });
        let encoded = LspFraming::encode(&req).expect("should encode references request");
        let mut buf = encoded;
        let decoded = LspFraming::decode(&mut buf)
            .expect("should decode")
            .expect("some");
        assert_eq!(decoded["method"], "textDocument/references");
        assert_eq!(decoded["id"], 101);
        assert_eq!(decoded["params"]["context"]["includeDeclaration"], true);

        let hover_params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": 5, "character": 2 }
        });
        let hover_req = json!({
            "jsonrpc": "2.0",
            "id": 102,
            "method": "textDocument/hover",
            "params": hover_params
        });
        let encoded_hover = LspFraming::encode(&hover_req).expect("should encode hover request");
        let mut buf_hover = encoded_hover;
        let decoded_hover = LspFraming::decode(&mut buf_hover)
            .expect("should decode")
            .expect("some");
        assert_eq!(decoded_hover["method"], "textDocument/hover");
        assert_eq!(decoded_hover["id"], 102);
    }
}
