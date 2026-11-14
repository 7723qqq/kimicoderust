use base64::Engine;
use serde_json::Value;

use super::messages::ProtocolViolationError;

pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

pub fn encode_frame(value: &Value) -> Result<Vec<u8>, ProtocolViolationError> {
    let mut encoded = serde_json::to_vec(value)
        .map_err(|_| ProtocolViolationError::new("message is not valid JSON"))?;
    encoded.push(b'\n');
    if encoded.len() > MAX_MESSAGE_BYTES {
        return Err(ProtocolViolationError::new(format!(
            "message exceeds the {MAX_MESSAGE_BYTES}-byte frame cap"
        )));
    }
    Ok(encoded)
}

pub fn encode_base64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn decode_base64(data: &str) -> Vec<u8> {
    let mut normalized = data
        .bytes()
        .filter(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'-' | b'_')
        })
        .map(|byte| match byte {
            b'-' => b'+',
            b'_' => b'/',
            byte => byte,
        })
        .collect::<Vec<_>>();
    while normalized.last() == Some(&b'=') {
        normalized.pop();
    }
    while normalized.len() % 4 != 0 {
        normalized.push(b'=');
    }
    base64::engine::general_purpose::STANDARD
        .decode(normalized)
        .unwrap_or_default()
}

#[derive(Default)]
pub struct LineFrameDecoder {
    pending: Vec<u8>,
    pending_bytes: usize,
}

impl LineFrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>, ProtocolViolationError> {
        let mut frames = Vec::new();
        let mut start = 0;
        while let Some(relative) = chunk[start..].iter().position(|byte| *byte == 0x0a) {
            let newline = start + relative;
            let line = self.take_line(&chunk[start..newline]);
            if let Some(frame) = self.decode_line(&line)? {
                frames.push(frame);
            }
            start = newline + 1;
        }
        if start < chunk.len() {
            self.pending.extend_from_slice(&chunk[start..]);
            self.pending_bytes += chunk.len() - start;
            if self.pending_bytes > MAX_MESSAGE_BYTES {
                return Err(ProtocolViolationError::new(format!(
                    "message exceeds the {MAX_MESSAGE_BYTES}-byte frame cap"
                )));
            }
        }
        Ok(frames)
    }

    fn take_line(&mut self, tail: &[u8]) -> Vec<u8> {
        if self.pending_bytes == 0 {
            return tail.to_vec();
        }
        self.pending.extend_from_slice(tail);
        self.pending_bytes = 0;
        std::mem::take(&mut self.pending)
    }

    fn decode_line(&self, line: &[u8]) -> Result<Option<Value>, ProtocolViolationError> {
        let end = line.len().saturating_sub(usize::from(line.last() == Some(&0x0d)));
        if end == 0 {
            return Ok(None);
        }
        if end > MAX_MESSAGE_BYTES {
            return Err(ProtocolViolationError::new(format!(
                "message exceeds the {MAX_MESSAGE_BYTES}-byte frame cap"
            )));
        }
        let line = line[..end].strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&line[..end]);
        let text = std::str::from_utf8(line)
            .map_err(|_| ProtocolViolationError::new("message is not valid UTF-8"))?;
        serde_json::from_str(text)
            .map(Some)
            .map_err(|_| ProtocolViolationError::new("message is not valid JSON"))
    }
}
