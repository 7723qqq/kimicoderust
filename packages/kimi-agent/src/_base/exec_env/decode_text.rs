use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextDecodeErrors {
    Strict,
    Replace,
    Ignore,
}

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, io::Error>> + Send>>;

pub struct ReadUtf8Lines {
    source: ByteStream,
    errors: TextDecodeErrors,
    ready: VecDeque<io::Result<String>>,
    pending: Vec<u8>,
    offset: usize,
    pending_offset: usize,
    source_ended: bool,
}

impl ReadUtf8Lines {
    pub fn new(source: ByteStream, errors: TextDecodeErrors) -> Self {
        Self {
            source,
            errors,
            ready: VecDeque::new(),
            pending: Vec::new(),
            offset: 0,
            pending_offset: 0,
            source_ended: false,
        }
    }

    fn push_chunk(&mut self, chunk: &[u8]) {
        let mut start = 0;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let piece = &chunk[start..=index];
            let had_pending = !self.pending.is_empty();
            let line_offset = if had_pending {
                self.pending_offset
            } else {
                self.offset + start
            };
            let line = if had_pending {
                let mut line = std::mem::take(&mut self.pending);
                line.extend(piece);
                line
            } else {
                piece.to_vec()
            };
            self.ready.push_back(
                decode_text_with_errors(&line, "utf-8", self.errors, line_offset != 0),
            );
            start = index + 1;
        }
        if start < chunk.len() {
            if self.pending.is_empty() {
                self.pending_offset = self.offset + start;
            }
            self.pending.extend(&chunk[start..]);
        }
        self.offset += chunk.len();
    }
}

impl Stream for ReadUtf8Lines {
    type Item = io::Result<String>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(line) = self.ready.pop_front() {
                return Poll::Ready(Some(line));
            }
            if self.source_ended {
                if self.pending.is_empty() {
                    return Poll::Ready(None);
                }
                let pending = std::mem::take(&mut self.pending);
                let ignore_bom = self.pending_offset != 0;
                return Poll::Ready(Some(decode_text_with_errors(
                    &pending,
                    "utf-8",
                    self.errors,
                    ignore_bom,
                )));
            }
            match self.source.as_mut().poll_next(context) {
                Poll::Ready(Some(Ok(chunk))) => self.push_chunk(&chunk),
                Poll::Ready(Some(Err(error))) => {
                    self.source_ended = true;
                    self.pending.clear();
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) => self.source_ended = true,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pub fn read_utf8_lines(source: ByteStream, errors: TextDecodeErrors) -> ReadUtf8Lines {
    ReadUtf8Lines::new(source, errors)
}

fn is_utf8_continuation(byte: u8) -> bool {
    (0x80..=0xbf).contains(&byte)
}

fn decode_utf8_ignore(data: &[u8]) -> String {
    let mut output = String::new();
    let mut index = 0;
    while index < data.len() {
        let b0 = data[index];
        if b0 <= 0x7f {
            output.push(char::from(b0));
            index += 1;
            continue;
        }
        if (0xc2..=0xdf).contains(&b0) {
            if let Some(b1) = data.get(index + 1)
                && is_utf8_continuation(*b1)
            {
                let point = (u32::from(b0 & 0x1f) << 6) | u32::from(b1 & 0x3f);
                output.push(char::from_u32(point).unwrap());
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        if (0xe0..=0xef).contains(&b0) {
            let b1 = data.get(index + 1).copied();
            let b2 = data.get(index + 2).copied();
            let valid_second = b1.is_some_and(|b1| {
                (b0 == 0xe0 && (0xa0..=0xbf).contains(&b1))
                    || ((0xe1..=0xec).contains(&b0) && is_utf8_continuation(b1))
                    || (b0 == 0xed && (0x80..=0x9f).contains(&b1))
                    || ((0xee..=0xef).contains(&b0) && is_utf8_continuation(b1))
            });
            if valid_second && b2.is_some_and(is_utf8_continuation) {
                let b1 = b1.unwrap();
                let b2 = b2.unwrap();
                let point = (u32::from(b0 & 0x0f) << 12)
                    | (u32::from(b1 & 0x3f) << 6)
                    | u32::from(b2 & 0x3f);
                output.push(char::from_u32(point).unwrap());
                index += 3;
                continue;
            }
            index += 1;
            continue;
        }
        if (0xf0..=0xf4).contains(&b0) {
            let b1 = data.get(index + 1).copied();
            let b2 = data.get(index + 2).copied();
            let b3 = data.get(index + 3).copied();
            let valid_second = b1.is_some_and(|b1| {
                (b0 == 0xf0 && (0x90..=0xbf).contains(&b1))
                    || ((0xf1..=0xf3).contains(&b0) && is_utf8_continuation(b1))
                    || (b0 == 0xf4 && (0x80..=0x8f).contains(&b1))
            });
            if valid_second
                && b2.is_some_and(is_utf8_continuation)
                && b3.is_some_and(is_utf8_continuation)
            {
                let b1 = b1.unwrap();
                let b2 = b2.unwrap();
                let b3 = b3.unwrap();
                let point = (u32::from(b0 & 0x07) << 18)
                    | (u32::from(b1 & 0x3f) << 12)
                    | (u32::from(b2 & 0x3f) << 6)
                    | u32::from(b3 & 0x3f);
                output.push(char::from_u32(point).unwrap());
                index += 4;
                continue;
            }
            index += 1;
            continue;
        }
        index += 1;
    }
    output
}

fn decode_utf16le_ignore(data: &[u8]) -> String {
    let mut output = String::new();
    let mut index = 0;
    while index + 1 < data.len() {
        let code_unit = u16::from_le_bytes([data[index], data[index + 1]]);
        if (0xd800..=0xdbff).contains(&code_unit) {
            if index + 3 < data.len() {
                let low = u16::from_le_bytes([data[index + 2], data[index + 3]]);
                if (0xdc00..=0xdfff).contains(&low) {
                    let point = 0x10000
                        + ((u32::from(code_unit) - 0xd800) << 10)
                        + (u32::from(low) - 0xdc00);
                    output.push(char::from_u32(point).unwrap());
                    index += 4;
                    continue;
                }
            }
            index += 2;
            continue;
        }
        if (0xdc00..=0xdfff).contains(&code_unit) {
            index += 2;
            continue;
        }
        output.push(char::from_u32(u32::from(code_unit)).unwrap());
        index += 2;
    }
    output
}

pub fn decode_text_with_errors(
    data: &[u8],
    encoding: &str,
    errors: TextDecodeErrors,
    ignore_bom: bool,
) -> io::Result<String> {
    match encoding {
        "utf-8" | "utf8" => {
            if errors == TextDecodeErrors::Ignore {
                return Ok(decode_utf8_ignore(data));
            }
            let data = if !ignore_bom && data.starts_with(&[0xef, 0xbb, 0xbf]) {
                &data[3..]
            } else {
                data
            };
            match errors {
                TextDecodeErrors::Strict => std::str::from_utf8(data)
                    .map(str::to_string)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
                TextDecodeErrors::Replace => Ok(String::from_utf8_lossy(data).into_owned()),
                TextDecodeErrors::Ignore => unreachable!(),
            }
        }
        "utf16le" | "ucs2" | "ucs-2" => {
            if errors == TextDecodeErrors::Ignore {
                return Ok(decode_utf16le_ignore(data));
            }
            let data = if !ignore_bom && data.starts_with(&[0xff, 0xfe]) {
                &data[2..]
            } else {
                data
            };
            if data.len() % 2 != 0 && errors == TextDecodeErrors::Strict {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete UTF-16 code unit",
                ));
            }
            let units = data
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<_>>();
            match errors {
                TextDecodeErrors::Strict => char::decode_utf16(units)
                    .map(|result| {
                        result.map_err(|error| {
                            io::Error::new(io::ErrorKind::InvalidData, error)
                        })
                    })
                    .collect(),
                TextDecodeErrors::Replace => {
                    let mut text = String::from_utf16_lossy(&units);
                    if data.len() % 2 != 0 {
                        text.push('\u{fffd}');
                    }
                    Ok(text)
                }
                TextDecodeErrors::Ignore => unreachable!(),
            }
        }
        "base64" => Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            data,
        )),
        "base64url" => Ok(base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            data,
        )),
        "hex" => Ok(data.iter().map(|byte| format!("{byte:02x}")).collect()),
        "latin1" | "binary" => Ok(data.iter().map(|byte| char::from(*byte)).collect()),
        "ascii" => Ok(data.iter().map(|byte| char::from(byte & 0x7f)).collect()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown encoding: {encoding}"),
        )),
    }
}
