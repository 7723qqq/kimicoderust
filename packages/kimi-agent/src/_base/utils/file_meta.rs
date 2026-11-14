use std::path::Path;

pub use crate::_base::text::encoding::FS_BINARY_NONPRINTABLE_FRACTION;
use crate::_base::text::encoding::classify_text_sample;

pub const FS_BINARY_SAMPLE_BYTES: usize = 4096;

pub struct FileMetaStat {
    pub size: u64,
    pub mtime_ms: Option<f64>,
    pub ino: Option<u64>,
}

pub fn detect_binary(buf: &[u8]) -> bool {
    classify_text_sample(buf).is_binary
}

pub fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut lines = 1 + text.bytes().filter(|byte| *byte == b'\n').count();
    if text.ends_with('\n') {
        lines -= 1;
    }
    lines
}

pub fn build_etag(stat: &FileMetaStat) -> String {
    let mtime = stat.mtime_ms.unwrap_or(0.0).floor() as i128;
    let ino = stat.ino.unwrap_or(0) as i128;
    format!(
        "{}-{}-{}",
        to_base36(mtime),
        to_base36(stat.size as i128),
        to_base36(ino)
    )
}

fn to_base36(value: i128) -> String {
    if value == 0 {
        return "0".to_string();
    }
    let negative = value < 0;
    let mut remaining = value.unsigned_abs();
    let mut digits = Vec::new();
    while remaining > 0 {
        let digit = (remaining % 36) as u8;
        digits.push(if digit < 10 {
            (b'0' + digit) as char
        } else {
            (b'a' + digit - 10) as char
        });
        remaining /= 36;
    }
    if negative {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

const EXT_TO_MIME: &[(&str, &str)] = &[
    (".ts", "text/typescript"),
    (".tsx", "text/typescript"),
    (".js", "text/javascript"),
    (".jsx", "text/javascript"),
    (".mjs", "text/javascript"),
    (".cjs", "text/javascript"),
    (".json", "application/json"),
    (".md", "text/markdown"),
    (".html", "text/html"),
    (".css", "text/css"),
    (".svg", "image/svg+xml"),
    (".png", "image/png"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".gif", "image/gif"),
    (".pdf", "application/pdf"),
    (".yaml", "text/yaml"),
    (".yml", "text/yaml"),
    (".toml", "application/toml"),
    (".sh", "text/x-shellscript"),
    (".py", "text/x-python"),
    (".rs", "text/rust"),
    (".go", "text/x-go"),
];

fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

pub fn guess_mime(path: &str, is_binary: bool) -> String {
    let extension = extension(path);
    if let Some((_, mime)) = EXT_TO_MIME.iter().find(|(ext, _)| *ext == extension) {
        return (*mime).to_string();
    }
    if is_binary {
        "application/octet-stream".to_string()
    } else {
        "text/plain".to_string()
    }
}

pub fn text_extension_for_mime(mime_type: &str) -> Option<String> {
    let mime = mime_type
        .split(';')
        .next()
        .unwrap()
        .trim()
        .to_lowercase();
    if mime == "application/json" || mime.ends_with("+json") {
        return Some(".json".to_string());
    }
    if mime == "application/xml" || mime.ends_with("+xml") {
        return Some(".xml".to_string());
    }
    if mime.ends_with("+yaml") {
        return Some(".yaml".to_string());
    }
    if mime == "application/toml" {
        return Some(".toml".to_string());
    }
    if mime == "text/csv" {
        return Some(".csv".to_string());
    }
    let text_mime = match mime.as_str() {
        "application/javascript" | "application/x-javascript" | "application/ecmascript" => {
            "text/javascript"
        }
        "application/yaml" | "application/x-yaml" => "text/yaml",
        "application/sql" | "application/graphql" | "application/x-www-form-urlencoded" => {
            "text/plain"
        }
        _ => &mime,
    };
    if !text_mime.starts_with("text/") {
        return None;
    }
    Some(
        EXT_TO_MIME
            .iter()
            .find(|(_, value)| *value == text_mime)
            .map(|(extension, _)| (*extension).to_string())
            .unwrap_or_else(|| ".txt".to_string()),
    )
}

pub fn guess_language_id(path: &str) -> Option<&'static str> {
    let extension = extension(path);
    Some(match extension.as_str() {
        ".ts" => "typescript",
        ".tsx" => "typescriptreact",
        ".js" => "javascript",
        ".jsx" => "javascriptreact",
        ".mjs" => "javascript",
        ".cjs" => "javascript",
        ".json" => "json",
        ".md" => "markdown",
        ".html" => "html",
        ".css" => "css",
        ".yaml" => "yaml",
        ".yml" => "yaml",
        ".toml" => "toml",
        ".sh" => "shellscript",
        ".py" => "python",
        ".rs" => "rust",
        ".go" => "go",
        _ => return None,
    })
}
