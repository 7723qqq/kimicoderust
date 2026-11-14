use std::collections::HashMap;
use std::sync::LazyLock;

use crate::human::llm::message::ContentPart;

pub use crate::human::llm::media::path_tag::{
    MediaPathTag, MediaPathTagKind, build_media_path_tag, escape_media_attribute,
    match_media_path_tags, match_single_media_path_tag, unescape_media_attribute,
};

pub type MediaKind = MediaPathTagKind;

pub static IMAGE_MIME_BY_SUFFIX: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        HashMap::from([
            (".png", "image/png"),
            (".jpg", "image/jpeg"),
            (".jpeg", "image/jpeg"),
            (".gif", "image/gif"),
            (".bmp", "image/bmp"),
            (".tif", "image/tiff"),
            (".tiff", "image/tiff"),
            (".webp", "image/webp"),
            (".ico", "image/x-icon"),
            (".heic", "image/heic"),
            (".heif", "image/heif"),
            (".avif", "image/avif"),
            (".svgz", "image/svg+xml"),
        ])
    });

pub static VIDEO_MIME_BY_SUFFIX: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        HashMap::from([
            (".mp4", "video/mp4"),
            (".mpg", "video/mpeg"),
            (".mpeg", "video/mpeg"),
            (".mkv", "video/x-matroska"),
            (".avi", "video/x-msvideo"),
            (".mov", "video/quicktime"),
            (".ogv", "video/ogg"),
            (".wmv", "video/x-ms-wmv"),
            (".webm", "video/webm"),
            (".m4v", "video/x-m4v"),
            (".flv", "video/x-flv"),
            (".3gp", "video/3gpp"),
            (".3g2", "video/3gpp2"),
        ])
    });

pub static DOCUMENT_MIME_BY_SUFFIX: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| HashMap::from([(".pdf", "application/pdf")]));

pub static AUDIO_MIME_BY_SUFFIX: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        HashMap::from([
            (".mp3", "audio/mpeg"),
            (".wav", "audio/wav"),
            (".m4a", "audio/mp4"),
            (".ogg", "audio/ogg"),
            (".oga", "audio/ogg"),
            (".flac", "audio/flac"),
            (".aac", "audio/aac"),
            (".opus", "audio/opus"),
            (".weba", "audio/webm"),
            (".wma", "audio/x-ms-wma"),
        ])
    });

static VIDEO_EXT_BY_MIME: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| invert_mime_by_suffix(&VIDEO_MIME_BY_SUFFIX));
static IMAGE_EXT_BY_MIME: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| invert_mime_by_suffix(&IMAGE_MIME_BY_SUFFIX));
static AUDIO_EXT_BY_MIME: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| invert_mime_by_suffix(&AUDIO_MIME_BY_SUFFIX));
static DOCUMENT_EXT_BY_MIME: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| invert_mime_by_suffix(&DOCUMENT_MIME_BY_SUFFIX));

fn invert_mime_by_suffix(
    table: &'static HashMap<&'static str, &'static str>,
) -> HashMap<&'static str, &'static str> {
    let mut out = HashMap::new();
    for (suffix, mime) in table {
        out.entry(*mime).or_insert(*suffix);
    }
    for (mime, suffix) in [
        ("image/jpeg", ".jpg"),
        ("image/tiff", ".tif"),
        ("video/mpeg", ".mpg"),
        ("audio/ogg", ".ogg"),
    ] {
        if table.contains_key(mime) {
            out.insert(mime, suffix);
        }
    }
    out
}

fn mime_base(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
}

pub fn media_extension_for_mime(mime_type: &str) -> Option<&'static str> {
    let base = mime_base(mime_type);
    VIDEO_EXT_BY_MIME
        .get(base.as_str())
        .or_else(|| IMAGE_EXT_BY_MIME.get(base.as_str()))
        .or_else(|| AUDIO_EXT_BY_MIME.get(base.as_str()))
        .or_else(|| DOCUMENT_EXT_BY_MIME.get(base.as_str()))
        .copied()
}

fn media_suffix(path: &str) -> String {
    let Some(index) = path.rfind('.') else {
        return String::new();
    };
    let last_separator = path.rfind(['/', '\\']).map_or(0, |index| index + 1);
    if index <= last_separator {
        return String::new();
    }
    path[index..].to_lowercase()
}

pub fn media_kind_for_path(path: &str) -> Option<MediaKind> {
    let suffix = media_suffix(path);
    if IMAGE_MIME_BY_SUFFIX.contains_key(suffix.as_str()) {
        return Some(MediaKind::Image);
    }
    if VIDEO_MIME_BY_SUFFIX.contains_key(suffix.as_str()) {
        return Some(MediaKind::Video);
    }
    if AUDIO_MIME_BY_SUFFIX.contains_key(suffix.as_str()) {
        return Some(MediaKind::Audio);
    }
    if DOCUMENT_MIME_BY_SUFFIX.contains_key(suffix.as_str()) {
        return Some(MediaKind::Document);
    }
    None
}

pub fn media_kind_for_mime(mime_type: &str) -> Option<MediaKind> {
    let base = mime_base(mime_type);
    if base.starts_with("image/") {
        return Some(MediaKind::Image);
    }
    if base.starts_with("video/") {
        return Some(MediaKind::Video);
    }
    if base.starts_with("audio/") {
        return Some(MediaKind::Audio);
    }
    if base == "application/pdf" {
        return Some(MediaKind::Document);
    }
    None
}

pub fn media_kind_of_part(part: &ContentPart) -> Option<MediaKind> {
    match part {
        ContentPart::ImageUrl { .. } => Some(MediaKind::Image),
        ContentPart::VideoUrl { .. } => Some(MediaKind::Video),
        ContentPart::AudioUrl { .. } => Some(MediaKind::Audio),
        ContentPart::DocumentUrl { .. } => Some(MediaKind::Document),
        _ => None,
    }
}

const KIMI_FILE_SCHEME: &str = "kimi-file://";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonFileRef {
    pub file_id: String,
}

pub fn is_daemon_file_url(url: &str) -> bool {
    url.starts_with(KIMI_FILE_SCHEME)
}

pub fn build_daemon_file_url(file_id: &str) -> String {
    format!("{KIMI_FILE_SCHEME}{file_id}")
}

pub fn parse_daemon_file_url(url: &str) -> Option<DaemonFileRef> {
    let rest = url.strip_prefix(KIMI_FILE_SCHEME)?;
    let file_id = rest.split('?').next().unwrap_or("");
    (!file_id.is_empty()).then(|| DaemonFileRef {
        file_id: file_id.to_string(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonFileMediaKind {
    Image,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonFilePartRef {
    pub kind: DaemonFileMediaKind,
    pub ref_: DaemonFileRef,
}

pub fn daemon_file_ref_from_part(part: &ContentPart) -> Option<DaemonFilePartRef> {
    match part {
        ContentPart::ImageUrl { image_url } => parse_daemon_file_url(&image_url.url).map(|ref_| {
            DaemonFilePartRef {
                kind: DaemonFileMediaKind::Image,
                ref_,
            }
        }),
        ContentPart::VideoUrl { video_url } => parse_daemon_file_url(&video_url.url).map(|ref_| {
            DaemonFilePartRef {
                kind: DaemonFileMediaKind::Video,
                ref_,
            }
        }),
        _ => None,
    }
}
