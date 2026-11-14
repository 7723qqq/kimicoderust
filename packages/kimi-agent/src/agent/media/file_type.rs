use std::collections::HashSet;
use std::sync::LazyLock;

pub use crate::native::file_type::{
    MEDIA_SNIFF_BYTES, detect_media_file_type, is_sensitive_file, resolve_mime,
};
use crate::native::file_type::{MediaSniffKind, sniff_media_from_magic as sniff_native};

pub use super::media_ref::{
    AUDIO_MIME_BY_SUFFIX, DOCUMENT_MIME_BY_SUFFIX, IMAGE_MIME_BY_SUFFIX, VIDEO_MIME_BY_SUFFIX,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Text,
    Image,
    Video,
    Document,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileType {
    pub kind: FileKind,
    pub mime_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectFileTypeMode {
    Text,
    Media,
}

pub static NON_TEXT_SUFFIXES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        ".icns", ".psd", ".ai", ".eps", ".doc", ".docx", ".dot", ".dotx", ".rtf",
        ".odt", ".xls", ".xlsx", ".xlsm", ".xlt", ".xltx", ".xltm", ".ods", ".ppt",
        ".pptx", ".pptm", ".pps", ".ppsx", ".odp", ".pages", ".numbers", ".key", ".zip",
        ".rar", ".7z", ".tar", ".gz", ".tgz", ".bz2", ".xz", ".zst", ".lz", ".lz4",
        ".br", ".cab", ".ar", ".deb", ".rpm", ".mp3", ".wav", ".flac", ".ogg", ".oga",
        ".opus", ".aac", ".m4a", ".wma", ".ttf", ".otf", ".woff", ".woff2", ".exe",
        ".dll", ".so", ".dylib", ".bin", ".apk", ".ipa", ".jar", ".class", ".pyc",
        ".pyo", ".wasm", ".dmg", ".iso", ".img", ".sqlite", ".sqlite3", ".db", ".db3",
    ])
});

pub fn sniff_media_from_magic(data: &[u8]) -> Option<FileType> {
    sniff_native(data).map(|sniffed| FileType {
        kind: match sniffed.kind {
            MediaSniffKind::Image => FileKind::Image,
            MediaSniffKind::Video => FileKind::Video,
            MediaSniffKind::Document => FileKind::Document,
        },
        mime_type: sniffed.mime_type.to_string(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
    pub transposed: Option<bool>,
}

pub fn sniff_image_dimensions(data: &[u8]) -> Option<ImageDimensions> {
    if data.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) && data.len() >= 24 {
        return Some(ImageDimensions {
            width: u32::from_be_bytes([data[16], data[17], data[18], data[19]]),
            height: u32::from_be_bytes([data[20], data[21], data[22], data[23]]),
            transposed: None,
        });
    }
    if (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) && data.len() >= 10 {
        return Some(ImageDimensions {
            width: u16::from_le_bytes([data[6], data[7]]) as u32,
            height: u16::from_le_bytes([data[8], data[9]]) as u32,
            transposed: None,
        });
    }
    if data.starts_with(b"BM") && data.len() >= 26 {
        return Some(ImageDimensions {
            width: i32::from_le_bytes([data[18], data[19], data[20], data[21]]) as u32,
            height: i32::from_le_bytes([data[22], data[23], data[24], data[25]]).unsigned_abs(),
            transposed: None,
        });
    }
    if data.starts_with(b"RIFF") && data.len() >= 30 {
        match &data[12..16] {
            b"VP8 " => {
                return Some(ImageDimensions {
                    width: (u16::from_le_bytes([data[26], data[27]]) & 0x3fff) as u32,
                    height: (u16::from_le_bytes([data[28], data[29]]) & 0x3fff) as u32,
                    transposed: None,
                });
            }
            b"VP8L" if data.len() >= 25 => {
                let bits = u32::from_le_bytes([data[21], data[22], data[23], data[24]]);
                return Some(ImageDimensions {
                    width: (bits & 0x3fff) + 1,
                    height: ((bits >> 14) & 0x3fff) + 1,
                    transposed: None,
                });
            }
            b"VP8X" => {
                return Some(ImageDimensions {
                    width: 1
                        + data[24] as u32
                        + ((data[25] as u32) << 8)
                        + ((data[26] as u32) << 16),
                    height: 1
                        + data[27] as u32
                        + ((data[28] as u32) << 8)
                        + ((data[29] as u32) << 16),
                    transposed: None,
                });
            }
            _ => {}
        }
    }
    if data.starts_with(&[0xff, 0xd8]) {
        let mut orientation = None;
        let mut offset = 2usize;
        while offset + 9 < data.len() {
            if data[offset] != 0xff {
                offset += 1;
                continue;
            }
            let marker = data[offset + 1];
            if (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc
            {
                let height = u16::from_be_bytes([data[offset + 5], data[offset + 6]]) as u32;
                let width = u16::from_be_bytes([data[offset + 7], data[offset + 8]]) as u32;
                return if orientation.is_some_and(|value| value >= 5) {
                    Some(ImageDimensions {
                        width: height,
                        height: width,
                        transposed: Some(true),
                    })
                } else {
                    Some(ImageDimensions {
                        width,
                        height,
                        transposed: None,
                    })
                };
            }
            if marker == 0xd8 || marker == 0xd9 || (0xd0..=0xd7).contains(&marker) {
                offset += 2;
                continue;
            }
            let segment_length = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            if segment_length < 2 {
                break;
            }
            if marker == 0xe1 && orientation.is_none() {
                orientation = read_exif_orientation(data, offset + 4, offset + 2 + segment_length);
            }
            offset += 2 + segment_length;
        }
    }
    None
}

fn read_exif_orientation(data: &[u8], start: usize, end: usize) -> Option<u16> {
    let bounded_end = end.min(data.len());
    if start + 6 > bounded_end || data.get(start..start + 6)? != b"Exif\0\0" {
        return None;
    }
    let tiff = start + 6;
    if tiff + 8 > bounded_end {
        return None;
    }
    let little_endian = match data.get(tiff..tiff + 2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let read_u16 = |offset: usize| -> Option<u16> {
        let bytes = data.get(offset..offset + 2)?;
        Some(if little_endian {
            u16::from_le_bytes([bytes[0], bytes[1]])
        } else {
            u16::from_be_bytes([bytes[0], bytes[1]])
        })
    };
    let read_u32 = |offset: usize| -> Option<u32> {
        let bytes = data.get(offset..offset + 4)?;
        Some(if little_endian {
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else {
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        })
    };
    if read_u16(tiff + 2)? != 42 {
        return None;
    }
    let ifd = tiff.checked_add(read_u32(tiff + 4)? as usize)?;
    if ifd + 2 > bounded_end {
        return None;
    }
    let entry_count = read_u16(ifd)? as usize;
    for index in 0..entry_count {
        let entry = ifd + 2 + index * 12;
        if entry + 12 > bounded_end {
            return None;
        }
        if read_u16(entry)? == 0x0112 {
            let value = read_u16(entry + 8)?;
            return (1..=8).contains(&value).then_some(value);
        }
    }
    None
}

fn get_suffix(path: &str) -> String {
    let Some(index) = path.rfind('.') else {
        return String::new();
    };
    let last_separator = path.rfind(['/', '\\']).map_or(0, |index| index + 1);
    if index <= last_separator {
        return String::new();
    }
    path[index..].to_lowercase()
}

fn media_hint(suffix: &str) -> Option<FileType> {
    if suffix == ".svg" {
        return Some(FileType {
            kind: FileKind::Text,
            mime_type: "image/svg+xml".to_string(),
        });
    }
    if let Some(mime_type) = IMAGE_MIME_BY_SUFFIX.get(suffix) {
        return Some(FileType {
            kind: FileKind::Image,
            mime_type: (*mime_type).to_string(),
        });
    }
    if let Some(mime_type) = VIDEO_MIME_BY_SUFFIX.get(suffix) {
        return Some(FileType {
            kind: FileKind::Video,
            mime_type: (*mime_type).to_string(),
        });
    }
    DOCUMENT_MIME_BY_SUFFIX.get(suffix).map(|mime_type| FileType {
        kind: FileKind::Document,
        mime_type: (*mime_type).to_string(),
    })
}

fn unknown_file_type() -> FileType {
    FileType {
        kind: FileKind::Unknown,
        mime_type: String::new(),
    }
}

pub fn detect_file_type(
    path: &str,
    header: Option<&[u8]>,
    mode: DetectFileTypeMode,
) -> FileType {
    let suffix = get_suffix(path);
    let hint = media_hint(&suffix);
    if let Some(data) = header {
        if let Some(sniffed) = sniff_media_from_magic(data) {
            if mode == DetectFileTypeMode::Media {
                return sniffed;
            }
            if let Some(hint) = hint {
                if sniffed.kind != hint.kind {
                    return unknown_file_type();
                }
                return hint;
            }
            return sniffed;
        }
        if hint.as_ref().is_some_and(|hint| hint.kind == FileKind::Image) {
            return unknown_file_type();
        }
        if mode == DetectFileTypeMode::Media
            && hint.as_ref().is_some_and(|hint| hint.kind == FileKind::Video)
        {
            return hint.unwrap();
        }
        if hint.as_ref().is_some_and(|hint| hint.kind == FileKind::Document)
            && data.starts_with(b"%PDF-")
        {
            return hint.unwrap();
        }
        if data.contains(&0) {
            return unknown_file_type();
        }
    }
    if let Some(hint) = hint {
        return hint;
    }
    if NON_TEXT_SUFFIXES.contains(suffix.as_str()) {
        return unknown_file_type();
    }
    FileType {
        kind: FileKind::Text,
        mime_type: "text/plain".to_string(),
    }
}
