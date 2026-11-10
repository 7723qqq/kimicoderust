use encoding_rs::GBK;

pub const TRANSCODE_MAX_BYTES: u64 = 10 * 1024 * 1024;

const MIN_GBK_UTF8_REPLACEMENT_RATIO: f64 = 0.15;
const MAX_LENIENT_REPLACEMENT_RATIO: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyTextEncoding {
    Gbk,
    Utf8Lenient,
}

fn contains_cjk_ideograph(text: &str) -> bool {
    text.chars().any(|ch| {
        matches!(
            ch,
            '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'
        )
    })
}

pub fn decode_utf8_lenient(bytes: &[u8]) -> (String, usize) {
    let text = String::from_utf8_lossy(bytes);
    let replaced = text.chars().filter(|&ch| ch == '\u{FFFD}').count();
    (text.into_owned(), replaced)
}

pub fn detect_legacy_text_encoding(bytes: &[u8]) -> Option<LegacyTextEncoding> {
    if bytes.is_empty() {
        return None;
    }
    let (gbk, _, _) = GBK.decode(bytes);
    if !gbk.contains('\u{FFFD}') && contains_cjk_ideograph(&gbk) {
        let (_, replaced) = decode_utf8_lenient(bytes);
        if replaced as f64 / bytes.len() as f64 >= MIN_GBK_UTF8_REPLACEMENT_RATIO {
            return Some(LegacyTextEncoding::Gbk);
        }
    }
    let (_, replaced) = decode_utf8_lenient(bytes);
    if replaced as f64 / bytes.len() as f64 <= MAX_LENIENT_REPLACEMENT_RATIO {
        return Some(LegacyTextEncoding::Utf8Lenient);
    }
    None
}

pub fn decode_gbk(bytes: &[u8]) -> String {
    GBK.decode(bytes).0.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_gbk() {
        let gbk = [0xd6, 0xd0, 0xce, 0xc4, 0xc4, 0xda, 0xc8, 0xdd, 0x0a];
        assert_eq!(
            detect_legacy_text_encoding(&gbk),
            Some(LegacyTextEncoding::Gbk)
        );
        assert_eq!(decode_gbk(&gbk), "中文内容\n");
    }

    #[test]
    fn detects_almost_utf8() {
        let mut bytes = b"\xe6\xad\xa3\xe5\xb8\xb8\n".to_vec();
        bytes.push(0x88);
        assert_eq!(
            detect_legacy_text_encoding(&bytes),
            Some(LegacyTextEncoding::Utf8Lenient)
        );
    }

    #[test]
    fn rejects_garbage_and_empty_input() {
        assert_eq!(detect_legacy_text_encoding(&[0xff; 8]), None);
        assert_eq!(detect_legacy_text_encoding(&[]), None);
    }
}
