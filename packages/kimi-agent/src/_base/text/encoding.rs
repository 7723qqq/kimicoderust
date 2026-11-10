use encoding_rs::{UTF_16BE, UTF_16LE};

pub const FS_BINARY_NONPRINTABLE_FRACTION: f64 = 0.3;
pub const ENCODING_DETECTION_SAMPLE_BYTES: usize = 512;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextClassification {
    pub is_binary: bool,
    pub encoding: UtfTextEncoding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextEncodingDetection {
    pub encoding: UtfTextEncoding,
    pub seems_binary: bool,
}

fn sniff_text_encoding(sample: &[u8]) -> TextEncodingDetection {
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
        if sample.len() >= 3 && b0 == 0xef && b1 == 0xbb && sample[2] == 0xbf {
            return TextEncodingDetection {
                encoding: UtfTextEncoding::Utf8,
                seems_binary: false,
            };
        }
    }
    let mut zeros_at_odd = 0usize;
    let mut zeros_at_even = 0usize;
    for (index, byte) in sample
        .iter()
        .take(ENCODING_DETECTION_SAMPLE_BYTES)
        .enumerate()
    {
        if *byte != 0 {
            continue;
        }
        if index % 2 == 1 {
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

pub fn classify_text_sample(sample: &[u8]) -> TextClassification {
    let sniffed = sniff_text_encoding(sample);
    if sniffed.seems_binary || sniffed.encoding != UtfTextEncoding::Utf8 {
        return TextClassification {
            is_binary: sniffed.seems_binary,
            encoding: sniffed.encoding,
        };
    }
    if sample.contains(&0) {
        return TextClassification {
            is_binary: true,
            encoding: UtfTextEncoding::Utf8,
        };
    }
    let mut end = sample.len();
    for index in sample.len().saturating_sub(3)..sample.len() {
        let byte = sample[index];
        let expected = match byte {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 0,
        };
        if expected == 0 || index + expected <= sample.len() {
            continue;
        }
        if sample[index + 1..]
            .iter()
            .all(|continuation| (0x80..=0xbf).contains(continuation))
        {
            end = index;
            break;
        }
    }
    let Ok(text) = std::str::from_utf8(&sample[..end]) else {
        return TextClassification {
            is_binary: true,
            encoding: UtfTextEncoding::Utf8,
        };
    };
    let mut non_printable = 0usize;
    let mut total = 0usize;
    for ch in text.chars() {
        total += 1;
        let code = ch as u32;
        if code == 9 || code == 10 || code == 13 {
            continue;
        }
        if code < 32 || (0x7f..=0x9f).contains(&code) {
            non_printable += 1;
        }
    }
    let is_binary =
        total > 0 && non_printable as f64 / total as f64 > FS_BINARY_NONPRINTABLE_FRACTION;
    TextClassification {
        is_binary,
        encoding: UtfTextEncoding::Utf8,
    }
}

pub fn detect_text_encoding(sample: &[u8]) -> TextEncodingDetection {
    let classification = classify_text_sample(sample);
    TextEncodingDetection {
        encoding: classification.encoding,
        seems_binary: classification.is_binary,
    }
}

pub fn decode_utf_text(bytes: &[u8], encoding: UtfTextEncoding) -> String {
    match encoding {
        UtfTextEncoding::Utf8 => {
            String::from_utf8_lossy(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
                .into_owned()
        }
        UtfTextEncoding::Utf16Le => {
            let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
            UTF_16LE.decode(bytes).0.into_owned()
        }
        UtfTextEncoding::Utf16Be => {
            let bytes = bytes.strip_prefix(&[0xfe, 0xff]).unwrap_or(bytes);
            UTF_16BE.decode(bytes).0.into_owned()
        }
    }
}
