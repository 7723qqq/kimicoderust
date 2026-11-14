pub fn is_animated_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 21
        && has_ascii(bytes, b"RIFF", 0)
        && has_ascii(bytes, b"WEBP", 8)
        && has_ascii(bytes, b"VP8X", 12)
        && bytes[20] & 0x02 != 0
}

fn has_ascii(bytes: &[u8], text: &[u8], at: usize) -> bool {
    bytes.get(at..at + text.len()) == Some(text)
}
