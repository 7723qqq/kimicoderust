pub const ENCODING_DETECTION_SAMPLE_BYTES: usize = 512;

pub const TRANSCODE_MAX_BYTES: u64 = 10 * 1024 * 1024;

const MIN_ZERO_BYTES_FOR_UTF16: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtfTextEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl UtfTextEncoding {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TextEncodingDetection {
    pub encoding: UtfTextEncoding,

    pub seems_binary: bool,
}

pub fn detect_text_encoding(sample: &[u8]) -> TextEncodingDetection {
    if sample.len() >= 2 {
        let (b0, b1) = (sample[0], sample[1]);
        if b0 == 0xfe && b1 == 0xff {
            return TextEncodingDetection {
                encoding: UtfTextEncoding::Utf16Be,
                seems_binary: false,
            };
        }
        if b0 == 0xff && b1 == 0xfe {
            return TextEncodingDetection {
                encoding: UtfTextEncoding::Utf16Le,
                seems_binary: false,
            };
        }
    }

    let mut zeros_at_odd = 0usize;
    let mut zeros_at_even = 0usize;
    let limit = sample.len().min(ENCODING_DETECTION_SAMPLE_BYTES);
    for (i, &b) in sample.iter().take(limit).enumerate() {
        if b != 0 {
            continue;
        }
        if i % 2 == 1 {
            zeros_at_odd += 1;
        } else {
            zeros_at_even += 1;
        }
    }

    if zeros_at_odd == 0 && zeros_at_even == 0 {
        return TextEncodingDetection {
            encoding: UtfTextEncoding::Utf8,
            seems_binary: false,
        };
    }
    if zeros_at_even == 0 && zeros_at_odd >= MIN_ZERO_BYTES_FOR_UTF16 {
        return TextEncodingDetection {
            encoding: UtfTextEncoding::Utf16Le,
            seems_binary: false,
        };
    }
    if zeros_at_odd == 0 && zeros_at_even >= MIN_ZERO_BYTES_FOR_UTF16 {
        return TextEncodingDetection {
            encoding: UtfTextEncoding::Utf16Be,
            seems_binary: false,
        };
    }
    TextEncodingDetection {
        encoding: UtfTextEncoding::Utf8,
        seems_binary: true,
    }
}

pub fn decode_utf_text(bytes: &[u8], encoding: UtfTextEncoding) -> String {
    match encoding {
        UtfTextEncoding::Utf16Le => {
            let offset = if bytes.starts_with(&[0xff, 0xfe]) {
                2
            } else {
                0
            };
            decode_utf16(&bytes[offset..], true)
        }
        UtfTextEncoding::Utf16Be => {
            let offset = if bytes.starts_with(&[0xfe, 0xff]) {
                2
            } else {
                0
            };
            decode_utf16(&bytes[offset..], false)
        }
        UtfTextEncoding::Utf8 => {
            let offset = if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
                3
            } else {
                0
            };
            String::from_utf8_lossy(&bytes[offset..]).into_owned()
        }
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    let mut out = String::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let unit = units[i];
        if (0xD800..=0xDBFF).contains(&unit) {
            if let Some(&low) = units.get(i + 1)
                && (0xDC00..=0xDFFF).contains(&low)
            {
                let c = 0x10000 + (((unit as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                out.push(char::from_u32(c).unwrap_or('\u{FFFD}'));
                i += 2;
                continue;
            }
            out.push('\u{FFFD}');
        } else if (0xDC00..=0xDFFF).contains(&unit) {
            out.push('\u{FFFD}');
        } else {
            out.push(char::from_u32(unit as u32).unwrap_or('\u{FFFD}'));
        }
        i += 1;
    }
    if bytes.len() % 2 == 1 {
        out.push('\u{FFFD}');
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEndingStyle {
    Lf,
    CrLf,
    Mixed,
}

#[derive(Debug, Default, Clone)]
pub struct LineEndingFlags {
    has_crlf: bool,
    has_lf: bool,
    has_lone_cr: bool,

    has_bare_lf: bool,
}

impl LineEndingFlags {
    pub fn feed(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.has_lf = true;
                self.has_bare_lf = true;
            }
            b'\r' => {
                self.has_lone_cr = true;
            }
            _ => {}
        }
    }

    pub fn feed_crlf(&mut self) {
        self.has_crlf = true;
        self.has_lf = true;
    }

    pub fn style(&self) -> LineEndingStyle {
        if self.has_lone_cr {
            return LineEndingStyle::Mixed;
        }
        if self.has_crlf && self.has_bare_lf {
            return LineEndingStyle::Mixed;
        }
        if self.has_crlf {
            return LineEndingStyle::CrLf;
        }
        LineEndingStyle::Lf
    }
}

pub fn detect_line_ending_style(data: &[u8]) -> LineEndingStyle {
    let mut flags = LineEndingFlags::default();
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'\r' && i + 1 < data.len() && data[i + 1] == b'\n' {
            flags.feed_crlf();
            i += 2;
        } else {
            flags.feed(data[i]);
            i += 1;
        }
    }
    flags.style()
}

pub fn make_carriage_returns_visible(text: &str) -> String {
    text.replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le_bytes(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in text.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    fn utf16be_bytes(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in text.encode_utf16() {
            out.extend_from_slice(&unit.to_be_bytes());
        }
        out
    }

    #[test]
    fn test_bom_utf16le() {
        let d = detect_text_encoding(&[0xff, 0xfe, b'a', 0x00]);
        assert_eq!(d.encoding, UtfTextEncoding::Utf16Le);
        assert!(!d.seems_binary);
    }

    #[test]
    fn test_bom_utf16be() {
        let d = detect_text_encoding(&[0xfe, 0xff, 0x00, b'a']);
        assert_eq!(d.encoding, UtfTextEncoding::Utf16Be);
        assert!(!d.seems_binary);
    }

    #[test]
    fn test_bomless_utf16le_parity() {
        let d = detect_text_encoding(&[0x61, 0x00, 0x62, 0x00, 0x63, 0x00]);
        assert_eq!(d.encoding, UtfTextEncoding::Utf16Le);
        assert!(!d.seems_binary);
    }

    #[test]
    fn test_bomless_utf16be_parity() {
        let d = detect_text_encoding(&[0x00, 0x61, 0x00, 0x62, 0x00, 0x63]);
        assert_eq!(d.encoding, UtfTextEncoding::Utf16Be);
        assert!(!d.seems_binary);
    }

    #[test]
    fn test_isolated_zero_is_binary() {
        let d = detect_text_encoding(b"plain prefix\x00\x01");
        assert!(d.seems_binary);
        assert_eq!(d.encoding, UtfTextEncoding::Utf8);
    }

    #[test]
    fn test_zeros_at_both_parities_is_binary() {
        let d = detect_text_encoding(&[0x00, 0x00, 0x61, 0x00, 0x00, 0x62]);
        assert!(d.seems_binary);
    }

    #[test]
    fn test_plain_ascii_is_utf8() {
        let d = detect_text_encoding(b"hello world");
        assert_eq!(d.encoding, UtfTextEncoding::Utf8);
        assert!(!d.seems_binary);
    }

    #[test]
    fn test_decode_utf16le_strips_bom_and_handles_cjk() {
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend_from_slice(&utf16le_bytes("中\n"));
        assert_eq!(decode_utf_text(&bytes, UtfTextEncoding::Utf16Le), "中\n");
    }

    #[test]
    fn test_decode_utf16be_strips_bom() {
        let mut bytes = vec![0xfe, 0xff];
        bytes.extend_from_slice(&utf16be_bytes("中\n"));
        assert_eq!(decode_utf_text(&bytes, UtfTextEncoding::Utf16Be), "中\n");
    }

    #[test]
    fn test_decode_utf16le_bomless() {
        let bytes = utf16le_bytes("hello\n");
        assert_eq!(decode_utf_text(&bytes, UtfTextEncoding::Utf16Le), "hello\n");
    }

    #[test]
    fn test_decode_utf16_surrogate_pair() {
        let bytes = utf16le_bytes("a😀b");
        assert_eq!(decode_utf_text(&bytes, UtfTextEncoding::Utf16Le), "a😀b");
    }

    #[test]
    fn test_decode_utf16_unpaired_surrogates_replaced() {
        let mut bytes = vec![0x00, 0xd8, 0x41, 0x00];
        bytes.extend_from_slice(&utf16le_bytes("x"));
        bytes.extend_from_slice(&[0x00, 0xdc]);
        let text = decode_utf_text(&bytes, UtfTextEncoding::Utf16Le);
        assert_eq!(text, "\u{FFFD}Ax\u{FFFD}");
    }

    #[test]
    fn test_decode_utf16_odd_trailing_byte_replaced() {
        let mut bytes = utf16le_bytes("ab");
        bytes.push(0x00);
        assert_eq!(
            decode_utf_text(&bytes, UtfTextEncoding::Utf16Le),
            "ab\u{FFFD}"
        );
    }

    #[test]
    fn test_decode_utf8_strips_bom() {
        let bytes = [0xef, 0xbb, 0xbf, b'h', b'i'];
        assert_eq!(decode_utf_text(&bytes, UtfTextEncoding::Utf8), "hi");
    }

    #[test]
    fn test_detect_lf() {
        assert_eq!(
            detect_line_ending_style(b"hello\nworld\n"),
            LineEndingStyle::Lf
        );
    }

    #[test]
    fn test_detect_crlf() {
        assert_eq!(
            detect_line_ending_style(b"hello\r\nworld\r\n"),
            LineEndingStyle::CrLf
        );
    }

    #[test]
    fn test_detect_mixed() {
        assert_eq!(
            detect_line_ending_style(b"hello\r\nworld\n"),
            LineEndingStyle::Mixed
        );
    }

    #[test]
    fn test_detect_lone_cr() {
        assert_eq!(
            detect_line_ending_style(b"hello\rworld"),
            LineEndingStyle::Mixed
        );
    }

    #[test]
    fn test_detect_lone_cr_then_crlf_is_mixed() {
        assert_eq!(
            detect_line_ending_style(b"a\rb\r\n"),
            LineEndingStyle::Mixed
        );
    }

    #[test]
    fn test_detect_crlf_then_lone_cr_is_mixed() {
        assert_eq!(
            detect_line_ending_style(b"a\r\nb\rc\n"),
            LineEndingStyle::Mixed
        );
    }

    #[test]
    fn test_make_carriage_returns_visible() {
        assert_eq!(make_carriage_returns_visible("a\rb\r\n"), "a\\rb\\r\n");
    }
}
