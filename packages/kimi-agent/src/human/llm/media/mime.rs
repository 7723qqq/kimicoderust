use std::collections::HashMap;
use std::sync::LazyLock;

pub static IMAGE_MIME_BY_EXT: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from_iter([
        ("png", "image/png"),
        ("jpg", "image/jpeg"),
        ("jpeg", "image/jpeg"),
        ("gif", "image/gif"),
        ("bmp", "image/bmp"),
        ("tif", "image/tiff"),
        ("tiff", "image/tiff"),
        ("webp", "image/webp"),
    ])
});

pub static VIDEO_MIME_BY_EXT: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from_iter([
        ("mp4", "video/mp4"),
        ("mpg", "video/mpeg"),
        ("mpeg", "video/mpeg"),
        ("mov", "video/quicktime"),
        ("webm", "video/webm"),
        ("mkv", "video/x-matroska"),
        ("avi", "video/x-msvideo"),
        ("flv", "video/x-flv"),
        ("3gp", "video/3gpp"),
    ])
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Image,
    Video,
}

pub fn media_kind_for_mime(mime_type: &str) -> Option<MediaKind> {
    if mime_type.starts_with("image/") {
        return Some(MediaKind::Image);
    }
    if mime_type.starts_with("video/") {
        return Some(MediaKind::Video);
    }
    None
}

pub fn media_mime_for_path(path: &str) -> Option<String> {
    let dot = path.rfind('.')?;
    let ext = path[dot + 1..].to_lowercase();
    IMAGE_MIME_BY_EXT
        .get(ext.as_str())
        .or_else(|| VIDEO_MIME_BY_EXT.get(ext.as_str()))
        .map(|mime| (*mime).to_string())
}

pub fn media_kind_for_path(path: &str) -> Option<MediaKind> {
    media_mime_for_path(path).and_then(|mime| media_kind_for_mime(&mime))
}
