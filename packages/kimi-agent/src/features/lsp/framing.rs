pub use crate::native::lsp::{LspFraming, MAX_FRAME_BYTES};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct LspFramingError {
    message: String,
}

impl LspFramingError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub fn encode_frame(content: &str) -> Result<Vec<u8>, LspFramingError> {
    let body = content.as_bytes();
    if body.len() > MAX_FRAME_BYTES {
        return Err(LspFramingError::new(format!(
            "message body of {} bytes exceeds the {MAX_FRAME_BYTES}-byte frame limit",
            body.len()
        )));
    }
    let mut output = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    output.extend_from_slice(body);
    Ok(output)
}

#[derive(Debug, Default)]
pub struct MessageDecoder {
    buffer: Vec<u8>,
}

impl MessageDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<String>, LspFramingError> {
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();
        loop {
            let Some(header_end) = self.buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
                if self.buffer.len() > MAX_FRAME_BYTES {
                    return Err(LspFramingError::new(
                        "header exceeds the frame limit without a terminator",
                    ));
                }
                return Ok(messages);
            };
            let header = String::from_utf8_lossy(&self.buffer[..header_end]).into_owned();
            let length = parse_content_length(&header)?;
            let body_start = header_end + 4;
            let body_end = body_start + length;
            if self.buffer.len() < body_end {
                return Ok(messages);
            }
            messages.push(String::from_utf8_lossy(&self.buffer[body_start..body_end]).into_owned());
            self.buffer.drain(..body_end);
        }
    }
}

fn parse_content_length(header: &str) -> Result<usize, LspFramingError> {
    let malformed = || {
        let header = serde_json::to_string(header).unwrap_or_else(|_| format!("{header:?}"));
        LspFramingError::new(format!("malformed LSP frame header: {header}"))
    };
    let prefix = header.get(..15).ok_or_else(malformed)?;
    if !prefix.eq_ignore_ascii_case("Content-Length:") {
        return Err(malformed());
    }
    let value = header.get(15..).ok_or_else(malformed)?.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed());
    }
    let length = value
        .parse::<usize>()
        .map_err(|_| LspFramingError::new(format!("invalid Content-Length: {value}")))?;
    if length > MAX_FRAME_BYTES {
        return Err(LspFramingError::new(format!(
            "invalid Content-Length: {value}"
        )));
    }
    Ok(length)
}
