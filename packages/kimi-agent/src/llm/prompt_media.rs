//! Prompt-intake media preparation (v2 `agent/media/image-compress.ts`,
//! `image-format-policy.ts`): the format gate, inline-image compression, and
//! the compression caption the model reads.

use std::path::PathBuf;

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::native::image_compress::{CompressConfig, compress_image};
use crate::rpc::types::ContentBlock;
use crate::server::files::FileStore;

/// v2 `MAX_IMAGE_EDGE_PX` — the longest-edge budget for a prompt image.
pub const MAX_IMAGE_EDGE_PX: u32 = 2000;

/// v2 `DEFAULT_INLINE_IMAGE_BYTE_BUDGET` (3.75 MiB) — the per-image byte
/// budget for a prompt image.
pub const INLINE_IMAGE_BYTE_BUDGET: usize = 3_932_160;

/// v2 `MAX_IMAGE_DECODE_BYTES` — over this, the image is passed through
/// untouched rather than decoded (the decode-bomb guard the TS wrapper
/// owns; `compress_image` itself only computes).
pub const MAX_IMAGE_DECODE_BYTES: usize = 64 * 1024 * 1024;

const FALLBACK_EDGES_PX: [u32; 6] = [2000, 1000, 768, 512, 384, 256];
const JPEG_QUALITY_STEPS: [u8; 4] = [80, 60, 40, 20];

/// The engine-side compression budget for a prompt's inline image — v2's
/// `compressImageForModel` defaults.
pub fn prompt_compress_config() -> CompressConfig {
    prompt_compress_config_for(None)
}

/// [`prompt_compress_config`] resolved for the provider: v2
/// `providerImagePolicy` gives kimi a 5 MiB inline budget (baseline 3.75 MiB).
/// The compression entry compresses to the model's budget, so a kimi image
/// between the two limits must not be re-encoded down to the baseline.
pub fn prompt_compress_config_for(provider: Option<&str>) -> CompressConfig {
    CompressConfig {
        max_edge: MAX_IMAGE_EDGE_PX,
        byte_budget: crate::llm::image_policy::provider_image_policy(provider).inline_byte_budget,
        fallback_edges: FALLBACK_EDGES_PX.to_vec(),
        jpeg_quality_steps: JPEG_QUALITY_STEPS.to_vec(),
    }
}

/// One side of a caption's comparison (v2 `ImageVariantDescription`).
pub struct ImageVariant {
    pub width: u32,
    pub height: u32,
    pub byte_length: usize,
    pub mime_type: String,
}

/// v2 `formatByteSize`, as the fork's SDK port words it.
pub fn format_byte_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", (bytes as f64 / 1024.0).round())
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn describe_image_variant(variant: &ImageVariant) -> String {
    let size = format!(
        "{} ({})",
        variant.mime_type,
        format_byte_size(variant.byte_length)
    );
    if variant.width > 0 && variant.height > 0 {
        format!("{}x{} {}", variant.width, variant.height, size)
    } else {
        size
    }
}

/// The compression caption, v2 `buildImageCompressionCaption`.
pub fn build_image_compression_caption(
    original: &ImageVariant,
    final_variant: &ImageVariant,
    original_path: Option<&str>,
) -> String {
    let mut sentences = vec![format!(
        "Image compressed to fit model limits: original {} -> sent {}.",
        describe_image_variant(original),
        describe_image_variant(final_variant),
    )];
    sentences.push("Fine detail may be lost.".to_string());
    match original_path {
        Some(path) if !path.is_empty() => sentences.push(format!(
            "The uncompressed original is saved at \"{path}\"; if you need fine detail (e.g. small text), call Read on that path with the region parameter (original-pixel coordinates) to view a crop at full fidelity."
        )),
        _ => sentences.push("The uncompressed original was not preserved.".to_string()),
    }
    format!("<system>{}</system>", sentences.join(" "))
}

/// Persist an uncompressed original under the file store, content-addressed
/// so the same original submitted twice occupies one blob (v2's
/// `persistOriginalImage` wrote a sha256-named cache file; the store's
/// `save_with_id` is the fork's dedupe seam, and its blob path is what the
/// caption names — `Read` serves paths outside the workspace). `None` when
/// the store cannot take it, which the caption reports as "not preserved".
pub fn persist_original_image(
    store: &FileStore,
    bytes: &[u8],
    mime_type: &str,
    name: &str,
) -> Option<PathBuf> {
    if bytes.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let id = format!("f_orig_{:x}", hasher.finalize());
    store
        .save_with_id(&id, name, mime_type, None, bytes)
        .ok()
        .and_then(|_| store.blob_path(&id))
}

/// Prepare one inline base64 image part for the model (v2's
/// `compressBase64ForModel` + caption half of `resolvePromptMediaFiles`).
///
/// Returns the caption to insert before the image (when compression
/// changed the bytes) and the block to send. A decode failure, an
/// over-`MAX_IMAGE_DECODE_BYTES` payload, or an image already within both
/// budgets passes through unchanged with no caption — v2's `passthrough`.
pub fn prepare_inline_image(
    store: &FileStore,
    provider: Option<&str>,
    media_type: &str,
    data_base64: &str,
    name: Option<&str>,
) -> (Option<String>, ContentBlock) {
    let passthrough = || ContentBlock::Image {
        media_type: media_type.to_string(),
        data: data_base64.to_string(),
        name: name.map(str::to_string),
    };
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_base64) else {
        return (None, passthrough());
    };
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_DECODE_BYTES {
        return (None, passthrough());
    }
    let Some(result) = compress_image(&bytes, media_type, &prompt_compress_config_for(provider))
    else {
        return (None, passthrough());
    };
    if !result.changed {
        return (None, passthrough());
    }
    let original_path = persist_original_image(store, &bytes, media_type, name.unwrap_or("image"))
        .map(|path| path.display().to_string());
    let caption = build_image_compression_caption(
        &ImageVariant {
            width: result.original_width,
            height: result.original_height,
            byte_length: result.original_byte_length,
            mime_type: media_type.to_string(),
        },
        &ImageVariant {
            width: result.width,
            height: result.height,
            byte_length: result.final_byte_length,
            mime_type: result.mime_type.clone(),
        },
        original_path.as_deref(),
    );
    let block = ContentBlock::Image {
        media_type: result.mime_type,
        data: base64::engine::general_purpose::STANDARD.encode(&result.data),
        name: name.map(str::to_string),
    };
    (Some(caption), block)
}

fn inline_image_blocks(
    store: &FileStore,
    provider: Option<&str>,
    media_type: &str,
    data: &str,
    name: Option<&str>,
) -> Vec<ContentBlock> {
    let url = format!("data:{media_type};base64,{data}");
    let Some((parsed_mime, parsed_data)) = crate::llm::image_policy::parse_image_data_url(&url)
    else {
        return vec![ContentBlock::Text {
            text: crate::llm::image_policy::build_malformed_image_notice(&url),
            meta: None,
        }];
    };
    let effective = crate::llm::image_policy::resolve_effective_image_mime(
        &parsed_mime,
        &crate::llm::image_policy::decode_base64_prefix(&parsed_data),
    );
    let normalized = crate::llm::image_policy::normalize_image_mime(&effective);
    if !crate::llm::image_policy::is_model_accepted_image_mime(&normalized, provider) {
        return vec![ContentBlock::Text {
            text: crate::llm::image_policy::build_unsupported_image_notice(
                &normalized,
                None,
                provider,
            ),
            meta: None,
        }];
    }
    let (caption, block) = prepare_inline_image(store, provider, &normalized, &parsed_data, name);
    let mut blocks = Vec::new();
    if let Some(caption) = caption {
        blocks.push(ContentBlock::Text {
            text: caption,
            meta: None,
        });
    }
    blocks.push(block);
    blocks
}

pub fn image_blocks_from_base64(
    store: &FileStore,
    provider: Option<&str>,
    media_type: &str,
    data: &str,
    name: Option<&str>,
) -> Vec<ContentBlock> {
    inline_image_blocks(store, provider, media_type, data, name)
}

pub fn image_blocks_from_url(
    store: &FileStore,
    provider: Option<&str>,
    url: &str,
    name: Option<&str>,
) -> Vec<ContentBlock> {
    if let Some((mime_type, data)) = crate::llm::image_policy::parse_image_data_url(url) {
        return inline_image_blocks(store, provider, &mime_type, &data, name);
    }
    if crate::llm::image_policy::is_data_url(url) {
        return vec![ContentBlock::Text {
            text: crate::llm::image_policy::build_malformed_image_notice(url),
            meta: None,
        }];
    }
    if let Some(mime_type) =
        crate::llm::image_policy::unsupported_image_mime_from_url(url, provider)
    {
        return vec![ContentBlock::Text {
            text: crate::llm::image_policy::build_unsupported_image_notice(
                &mime_type,
                Some(url),
                provider,
            ),
            meta: None,
        }];
    }
    vec![ContentBlock::ImageUrl {
        url: url.to_string(),
        id: None,
        name: name.map(str::to_string),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};

    fn rgb_image(width: u32, height: u32) -> DynamicImage {
        let mut img = RgbImage::new(width, height);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8]);
        }
        DynamicImage::ImageRgb8(img)
    }

    fn png_base64(width: u32, height: u32) -> String {
        let mut bytes = Vec::new();
        rgb_image(width, height)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    }

    fn store_in(dir: &tempfile::TempDir) -> FileStore {
        FileStore::with_root(dir.path().to_path_buf())
    }

    #[test]
    fn the_inline_budget_follows_the_provider_policy() {
        assert_eq!(
            prompt_compress_config().byte_budget,
            INLINE_IMAGE_BYTE_BUDGET
        );
        assert_eq!(
            prompt_compress_config_for(Some("kimi")).byte_budget,
            5 * 1024 * 1024,
            "v2 `KIMI_IMAGE_POLICY` raises the inline budget to 5 MiB"
        );
        for provider in ["openai", "anthropic", "google"] {
            assert_eq!(
                prompt_compress_config_for(Some(provider)).byte_budget,
                INLINE_IMAGE_BYTE_BUDGET,
                "{provider} keeps the baseline 3.75 MiB budget"
            );
        }
    }

    #[test]
    fn format_byte_size_matches_the_sdk_wording() {
        assert_eq!(format_byte_size(512), "512 B");
        assert_eq!(format_byte_size(2048), "2 KB");
        assert_eq!(format_byte_size(5_242_880), "5.0 MB");
    }

    #[test]
    fn the_caption_matches_v2_verbatim() {
        let caption = build_image_compression_caption(
            &ImageVariant {
                width: 4000,
                height: 3000,
                byte_length: 5_242_880,
                mime_type: "image/png".into(),
            },
            &ImageVariant {
                width: 2000,
                height: 1500,
                byte_length: 1_048_576,
                mime_type: "image/jpeg".into(),
            },
            Some("/cache/kimi-code-original-images/abc.png"),
        );
        assert_eq!(
            caption,
            "<system>Image compressed to fit model limits: original 4000x3000 image/png (5.0 MB) -> sent 2000x1500 image/jpeg (1.0 MB). Fine detail may be lost. The uncompressed original is saved at \"/cache/kimi-code-original-images/abc.png\"; if you need fine detail (e.g. small text), call Read on that path with the region parameter (original-pixel coordinates) to view a crop at full fidelity.</system>"
        );
    }

    #[test]
    fn a_caption_without_a_preserved_original_says_so() {
        let caption = build_image_compression_caption(
            &ImageVariant {
                width: 10,
                height: 10,
                byte_length: 100,
                mime_type: "image/png".into(),
            },
            &ImageVariant {
                width: 10,
                height: 10,
                byte_length: 90,
                mime_type: "image/jpeg".into(),
            },
            None,
        );
        assert!(caption.ends_with(
            "Fine detail may be lost. The uncompressed original was not preserved.</system>"
        ));
    }

    #[test]
    fn an_over_budget_inline_image_is_compressed_captioned_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let data = png_base64(4000, 3000);

        let (caption, block) =
            prepare_inline_image(&store, None, "image/png", &data, Some("shot.png"));

        let caption = caption.expect("an over-budget image is captioned");
        assert!(
            caption.starts_with(
                "<system>Image compressed to fit model limits: original 4000x3000 image/"
            )
        );
        assert!(caption.contains("Fine detail may be lost."));
        assert!(caption.contains("call Read on that path"));
        // The caption names a real persisted original.
        let path = caption
            .split("saved at \"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("the caption names the original's path");
        assert!(std::path::Path::new(path).exists(), "{path}");

        let ContentBlock::Image {
            media_type,
            data: sent,
            name,
        } = block
        else {
            panic!("expected an image block");
        };
        assert_eq!(name.as_deref(), Some("shot.png"));
        assert!(media_type.starts_with("image/"));
        let sent_bytes = base64::engine::general_purpose::STANDARD
            .decode(&sent)
            .expect("the sent block is valid base64");
        assert!(!sent_bytes.is_empty());
    }

    #[test]
    fn a_small_inline_image_passes_through_uncaptioned() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let data = png_base64(10, 10);

        let (caption, block) = prepare_inline_image(&store, None, "image/png", &data, None);

        assert!(caption.is_none(), "an in-budget image is not captioned");
        let ContentBlock::Image {
            media_type,
            data: sent,
            ..
        } = block
        else {
            panic!("expected an image block");
        };
        assert_eq!(media_type, "image/png");
        assert_eq!(sent, data, "the original bytes pass through untouched");
    }

    #[test]
    fn an_unsupported_inline_format_becomes_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let bmp = base64::engine::general_purpose::STANDARD.encode(b"BMfakebmpdata");

        let blocks =
            image_blocks_from_base64(&store, Some("openai"), "image/bmp", &bmp, Some("a.bmp"));
        assert_eq!(blocks.len(), 1);
        let ContentBlock::Text { text, .. } = &blocks[0] else {
            panic!("expected a notice, got {:?}", blocks[0]);
        };
        assert!(text.contains("unsupported image format image/bmp"), "{text}");
        assert!(text.contains("PNG, JPEG, GIF, and WebP"), "{text}");

        let blocks =
            image_blocks_from_base64(&store, Some("kimi"), "image/bmp", &bmp, Some("a.bmp"));
        assert_eq!(blocks.len(), 1);
        let ContentBlock::Image { media_type, .. } = &blocks[0] else {
            panic!("expected an image block, got {:?}", blocks[0]);
        };
        assert_eq!(media_type, "image/bmp");
    }

    #[test]
    fn a_malformed_data_url_becomes_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let blocks = image_blocks_from_url(&store, None, "data:image/png,AAAA", Some("a.png"));
        assert_eq!(blocks.len(), 1);
        let ContentBlock::Text { text, .. } = &blocks[0] else {
            panic!("expected a notice, got {:?}", blocks[0]);
        };
        assert!(text.contains("is not a valid data URL"), "{text}");
    }

    #[test]
    fn a_remote_url_is_gated_by_its_extension() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);

        let blocks =
            image_blocks_from_url(&store, Some("openai"), "https://x/a.heic", Some("a.heic"));
        let ContentBlock::Text { text, .. } = &blocks[0] else {
            panic!("expected a notice, got {:?}", blocks[0]);
        };
        assert!(
            text.contains("\"https://x/a.heic\" uses unsupported image format image/heic"),
            "{text}"
        );

        let blocks = image_blocks_from_url(&store, Some("openai"), "https://x/a.png", None);
        assert_eq!(
            blocks[0],
            ContentBlock::ImageUrl {
                url: "https://x/a.png".into(),
                id: None,
                name: None,
            }
        );
    }

    #[test]
    fn an_inline_data_url_image_goes_through_compression() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let data = png_base64(10, 10);
        let url = format!("data:image/jpg;base64,{data}");

        let blocks = image_blocks_from_url(&store, None, &url, None);

        assert_eq!(blocks.len(), 1);
        let ContentBlock::Image {
            media_type,
            data: sent,
            ..
        } = &blocks[0]
        else {
            panic!("expected an image block, got {:?}", blocks[0]);
        };
        assert_eq!(media_type, "image/png");
        assert_eq!(sent, &data);
    }

    #[test]
    fn an_unsupported_format_never_reaches_the_image_block() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let data = base64::engine::general_purpose::STANDARD.encode(b"\x00\x00\x00\x18ftypheic");
        let blocks =
            image_blocks_from_base64(&store, None, "image/heic", &data, Some("a.heic"));
        assert!(matches!(&blocks[0], ContentBlock::Text { text, .. } if text.contains("image/heic")));
    }
}
