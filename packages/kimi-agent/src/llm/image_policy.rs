//! Per-provider image policy (v2 `human/llm/media/image-formats.ts`).
//!
//! The baseline policy every OpenAI-compatible vendor speaks, plus the wider
//! Kimi policy (BMP/HEIC/HEIF and a 5 MiB inline budget). The request builders
//! use this for the two things the wire shape depends on: which `data:` mime
//! types are acceptable, and how large an inline image may be.

/// The accepted mime types and inline size budget of one provider.
#[derive(Debug, Clone, Copy)]
pub struct ProviderImagePolicy {
    pub accepted_mimes: &'static [&'static str],
    pub inline_byte_budget: usize,
}

/// v2 `DEFAULT_INLINE_IMAGE_BYTE_BUDGET`.
pub const DEFAULT_INLINE_IMAGE_BYTE_BUDGET: usize = (3.75 * 1024.0 * 1024.0) as usize;

const BASELINE_IMAGE_POLICY: ProviderImagePolicy = ProviderImagePolicy {
    accepted_mimes: &["image/png", "image/jpeg", "image/gif", "image/webp"],
    inline_byte_budget: DEFAULT_INLINE_IMAGE_BYTE_BUDGET,
};

const KIMI_IMAGE_POLICY: ProviderImagePolicy = ProviderImagePolicy {
    accepted_mimes: &[
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
        "image/bmp",
        "image/heic",
        "image/heif",
    ],
    inline_byte_budget: 5 * 1024 * 1024,
};

/// v2 `providerImagePolicy`: Kimi detects the wider policy, every other
/// provider (and an unresolved one) falls back to the baseline.
pub fn provider_image_policy(provider: Option<&str>) -> ProviderImagePolicy {
    match provider {
        Some(provider) if provider.eq_ignore_ascii_case("kimi") => KIMI_IMAGE_POLICY,
        _ => BASELINE_IMAGE_POLICY,
    }
}

impl ProviderImagePolicy {
    /// Whether a `data:` image mime type may reach this provider.
    pub fn accepts(&self, mime: &str) -> bool {
        self.accepted_mimes.contains(&mime)
    }
}

const IMAGE_MIME_BY_SUFFIX: [(&str, &str); 13] = [
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
];

const IMAGE_FORMAT_LABELS: [(&str, &str); 7] = [
    ("image/png", "PNG"),
    ("image/jpeg", "JPEG"),
    ("image/gif", "GIF"),
    ("image/webp", "WebP"),
    ("image/bmp", "BMP"),
    ("image/heic", "HEIC"),
    ("image/heif", "HEIF"),
];

pub fn normalize_image_mime(mime_type: &str) -> String {
    let lower = mime_type.trim().to_ascii_lowercase();
    let base = lower.split(';').next().unwrap_or("").trim();
    if base == "image/jpg" {
        "image/jpeg".to_string()
    } else {
        base.to_string()
    }
}

pub fn is_model_accepted_image_mime(mime_type: &str, provider: Option<&str>) -> bool {
    provider_image_policy(provider).accepts(&normalize_image_mime(mime_type))
}

pub fn accepted_formats_text(provider: Option<&str>) -> String {
    let labels: Vec<String> = provider_image_policy(provider)
        .accepted_mimes
        .iter()
        .map(|mime| {
            IMAGE_FORMAT_LABELS
                .iter()
                .find(|(known, _)| known == mime)
                .map_or_else(|| (*mime).to_string(), |(_, label)| (*label).to_string())
        })
        .collect();
    if labels.len() < 2 {
        return labels.join(", ");
    }
    format!(
        "{}, and {}",
        labels[..labels.len() - 1].join(", "),
        labels[labels.len() - 1]
    )
}

pub fn build_unsupported_image_notice(
    mime_type: &str,
    name: Option<&str>,
    provider: Option<&str>,
) -> String {
    let what = match name {
        Some(name) if !name.is_empty() => {
            format!("\"{name}\" uses unsupported image format {mime_type}")
        }
        _ => format!("unsupported image format {mime_type}"),
    };
    format!(
        "[Image omitted: {what}. The current model provider accepts only {} — convert it to PNG or JPEG and try again.]",
        accepted_formats_text(provider)
    )
}

pub fn build_malformed_image_notice(url: &str) -> String {
    let shown: String = url.chars().take(80).collect();
    let shown = if url.chars().count() > 80 {
        format!("{shown}…")
    } else {
        shown
    };
    format!(
        "[Image omitted: \"{shown}\" is not a valid data URL (its header or payload could not be parsed). Re-encode the image as PNG or JPEG and try again.]"
    )
}

pub fn is_data_url(url: &str) -> bool {
    url.to_ascii_lowercase().starts_with("data:")
}

pub fn parse_image_data_url(url: &str) -> Option<(String, String)> {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("data:") {
        return None;
    }
    let body = &url[5..];
    let marker = lower[5..].find(";base64,")?;
    let mime_type = body[..marker].split(';').next().unwrap_or("").trim();
    if mime_type.is_empty() || mime_type.contains(',') {
        return None;
    }
    Some((
        mime_type.to_string(),
        body[marker + ";base64,".len()..].to_string(),
    ))
}

pub fn decode_base64_prefix(base64: &str) -> Vec<u8> {
    let prefix: String = base64.chars().take(48).collect();
    let trimmed = prefix.trim_end_matches('=');
    let padded = match trimmed.len() % 4 {
        0 => trimmed.to_string(),
        2 => format!("{trimmed}=="),
        3 => format!("{trimmed}="),
        _ => return Vec::new(),
    };
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(padded.as_bytes())
        .unwrap_or_default()
}

pub fn resolve_effective_image_mime(declared_mime: &str, header: &[u8]) -> String {
    match crate::native::file_type::sniff_media_from_magic(header) {
        Some(sniffed) => sniffed.mime_type.to_string(),
        None => declared_mime.to_string(),
    }
}

pub fn unsupported_image_mime_from_url(url: &str, provider: Option<&str>) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let idx = path.rfind('.')?;
    let ext = path[idx..].to_ascii_lowercase();
    let mime = if ext == ".svg" {
        "image/svg+xml".to_string()
    } else {
        IMAGE_MIME_BY_SUFFIX
            .iter()
            .find(|(known, _)| *known == ext)
            .map(|(_, mime)| (*mime).to_string())?
    };
    if is_model_accepted_image_mime(&mime, provider) {
        None
    } else {
        Some(mime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_accepts_the_wider_formats_with_a_larger_budget() {
        let policy = provider_image_policy(Some("kimi"));
        assert!(policy.accepts("image/png"));
        assert!(policy.accepts("image/heic"));
        assert!(policy.accepts("image/bmp"));
        assert_eq!(policy.inline_byte_budget, 5 * 1024 * 1024);
    }

    #[test]
    fn the_gate_normalizes_aliases_and_reports_acceptance() {
        assert_eq!(normalize_image_mime(" Image/JPG ; q=1"), "image/jpeg");
        assert!(is_model_accepted_image_mime("image/jpg", None));
        assert!(!is_model_accepted_image_mime("image/bmp", Some("openai")));
        assert!(is_model_accepted_image_mime("image/bmp", Some("kimi")));
    }

    #[test]
    fn the_notices_match_v2_wording() {
        assert_eq!(
            accepted_formats_text(None),
            "PNG, JPEG, GIF, and WebP"
        );
        assert_eq!(
            accepted_formats_text(Some("kimi")),
            "PNG, JPEG, GIF, WebP, BMP, HEIC, and HEIF"
        );
        assert_eq!(
            build_unsupported_image_notice("image/heic", None, Some("openai")),
            "[Image omitted: unsupported image format image/heic. The current model provider accepts only PNG, JPEG, GIF, and WebP — convert it to PNG or JPEG and try again.]"
        );
        assert_eq!(
            build_unsupported_image_notice("image/heic", Some("a.heic"), Some("openai")),
            "[Image omitted: \"a.heic\" uses unsupported image format image/heic. The current model provider accepts only PNG, JPEG, GIF, and WebP — convert it to PNG or JPEG and try again.]"
        );
        assert!(build_malformed_image_notice("data:image/png;base64").starts_with(
            "[Image omitted: \"data:image/png;base64\" is not a valid data URL"
        ));
    }

    #[test]
    fn a_data_url_is_parsed_into_mime_and_payload() {
        assert_eq!(
            parse_image_data_url("data:image/png;base64,AAAA"),
            Some(("image/png".to_string(), "AAAA".to_string()))
        );
        assert_eq!(
            parse_image_data_url("data:image/jpg;charset=utf-8;base64,BBBB"),
            Some(("image/jpg".to_string(), "BBBB".to_string()))
        );
        assert_eq!(parse_image_data_url("data:image/png,AAAA"), None);
        assert_eq!(parse_image_data_url("https://x/a.png"), None);
        assert!(is_data_url("DATA:image/png;base64,AAAA"));
    }

    #[test]
    fn the_effective_mime_comes_from_the_magic_bytes() {
        assert_eq!(
            resolve_effective_image_mime("image/png", b"\xff\xd8\xffrest"),
            "image/jpeg"
        );
        assert_eq!(
            resolve_effective_image_mime("image/heic", b"plain text"),
            "image/heic"
        );
    }

    #[test]
    fn a_remote_url_is_rejected_by_its_extension() {
        assert_eq!(
            unsupported_image_mime_from_url("https://x/a.HEIC?raw=1", Some("openai")),
            Some("image/heic".to_string())
        );
        assert_eq!(
            unsupported_image_mime_from_url("https://x/a.heic", Some("kimi")),
            None
        );
        assert_eq!(
            unsupported_image_mime_from_url("https://x/a.svg", Some("kimi")),
            Some("image/svg+xml".to_string())
        );
        assert_eq!(
            unsupported_image_mime_from_url("https://x/a.png", Some("openai")),
            None
        );
        assert_eq!(unsupported_image_mime_from_url("https://x/a", Some("openai")), None);
    }
}
