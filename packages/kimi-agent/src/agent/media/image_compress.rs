use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use futures_util::future::BoxFuture;
use serde::Serialize;

use crate::_base::utils::abort::{AbortError, AbortSignal, abort_error};
use crate::app::telemetry::events::{
    ImageCompressEvent, ImageCompressOutcome, ImageCropErrorKind, ImageCropEvent,
};
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::human::llm::message::ContentPart;
use crate::llm::image_policy::DEFAULT_INLINE_IMAGE_BYTE_BUDGET;
use crate::native::image_compress::{
    CompressConfig, CompressResult as NativeCompressResult, CropConfig, CropError, CropResult,
    compress_image, crop_image,
};

use super::file_type::sniff_image_dimensions;
use super::image_format_policy::{
    build_malformed_image_notice, build_unsupported_image_notice, decode_base64_prefix,
    is_data_url, is_model_accepted_image_mime, normalize_image_mime, parse_image_data_url,
    resolve_effective_image_mime, unsupported_image_mime_from_url,
};
use super::webp_animated::is_animated_webp;

pub use crate::human::llm::media::image_compression_caption::{
    ImageCompressionCaptionExtraction, extract_image_compression_captions,
};
pub use crate::native::image_compress::{
    CompressConfig as NativeCompressConfig, CropConfig as NativeCropConfig,
};

pub const MAX_IMAGE_EDGE_PX: u32 = 2000;
pub const IMAGE_BYTE_BUDGET: usize = DEFAULT_INLINE_IMAGE_BYTE_BUDGET;
pub const READ_IMAGE_BYTE_BUDGET: usize = 256 * 1024;
pub const MAX_IMAGE_DECODE_BYTES: usize = 64 * 1024 * 1024;

const MAX_DECODE_PIXELS: u64 = 100_000_000;
const JPEG_QUALITY_STEPS: [u8; 4] = [80, 60, 40, 20];
const FALLBACK_EDGES_PX: [u32; 6] = [2000, 1000, 768, 512, 384, 256];
const RECODABLE_MIME: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

static LENIENT_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

fn decode_lenient_base64(payload: &str) -> Vec<u8> {
    let mut cleaned = Vec::with_capacity(payload.len());
    for unit in payload.encode_utf16() {
        let byte = unit as u8;
        match byte {
            b'=' => break,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => cleaned.push(byte),
            b'-' => cleaned.push(b'+'),
            b'_' => cleaned.push(b'/'),
            _ => {}
        }
    }
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    LENIENT_BASE64.decode(cleaned).unwrap_or_default()
}

static CONFIGURED_MAX_IMAGE_EDGE_PX: AtomicU32 = AtomicU32::new(0);
static CONFIGURED_READ_IMAGE_BYTE_BUDGET: AtomicUsize = AtomicUsize::new(0);

pub fn set_configured_max_image_edge_px(value: Option<u32>) {
    CONFIGURED_MAX_IMAGE_EDGE_PX.store(value.filter(|value| *value > 0).unwrap_or(0), Ordering::Relaxed);
}

pub fn resolve_max_image_edge_px() -> u32 {
    match CONFIGURED_MAX_IMAGE_EDGE_PX.load(Ordering::Relaxed) {
        0 => MAX_IMAGE_EDGE_PX,
        value => value,
    }
}

pub fn set_configured_read_image_byte_budget(value: Option<usize>) {
    CONFIGURED_READ_IMAGE_BYTE_BUDGET.store(
        value.filter(|value| *value > 0).unwrap_or(0),
        Ordering::Relaxed,
    );
}

pub fn resolve_read_image_byte_budget() -> usize {
    match CONFIGURED_READ_IMAGE_BYTE_BUDGET.load(Ordering::Relaxed) {
        0 => READ_IMAGE_BYTE_BUDGET,
        value => value,
    }
}

pub fn is_recodable_image(bytes: &[u8], mime_type: &str) -> bool {
    let normalized = normalize_image_mime(mime_type);
    RECODABLE_MIME.contains(&normalized.as_str())
        && (normalized != "image/webp" || !is_animated_webp(bytes))
}

#[derive(Clone, Copy, Default)]
pub struct CompressImageOptions<'a> {
    pub max_edge: Option<u32>,
    pub byte_budget: Option<usize>,
    pub max_decode_bytes: Option<usize>,
    pub telemetry: Option<&'a dyn ITelemetryService>,
    pub telemetry_source: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressImageResult {
    pub data: Vec<u8>,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    pub changed: bool,
    pub original_byte_length: usize,
    pub final_byte_length: usize,
}

impl CompressImageResult {
    fn passthrough(bytes: &[u8], mime_type: &str) -> Self {
        let dimensions = sniff_image_dimensions(bytes);
        Self {
            data: bytes.to_vec(),
            mime_type: mime_type.to_string(),
            width: dimensions.map_or(0, |dimensions| dimensions.width),
            height: dimensions.map_or(0, |dimensions| dimensions.height),
            original_width: dimensions.map_or(0, |dimensions| dimensions.width),
            original_height: dimensions.map_or(0, |dimensions| dimensions.height),
            changed: false,
            original_byte_length: bytes.len(),
            final_byte_length: bytes.len(),
        }
    }

    fn compressed(bytes: &[u8], result: NativeCompressResult) -> Self {
        Self {
            data: result.data,
            mime_type: result.mime_type,
            width: result.width,
            height: result.height,
            original_width: result.original_width,
            original_height: result.original_height,
            changed: result.changed,
            original_byte_length: bytes.len(),
            final_byte_length: result.final_byte_length,
        }
    }
}

pub async fn compress_image_for_model(
    bytes: &[u8],
    mime_type: &str,
    options: &CompressImageOptions<'_>,
) -> CompressImageResult {
    let started_at = Instant::now();
    let max_edge = options.max_edge.unwrap_or_else(resolve_max_image_edge_px);
    let byte_budget = options.byte_budget.unwrap_or(IMAGE_BYTE_BUDGET);
    let max_decode_bytes = options.max_decode_bytes.unwrap_or(MAX_IMAGE_DECODE_BYTES);
    let normalized_mime = normalize_image_mime(mime_type);
    let dimensions = sniff_image_dimensions(bytes);
    let passthrough = || CompressImageResult {
        data: bytes.to_vec(),
        mime_type: mime_type.to_string(),
        width: dimensions.map_or(0, |dimensions| dimensions.width),
        height: dimensions.map_or(0, |dimensions| dimensions.height),
        original_width: dimensions.map_or(0, |dimensions| dimensions.width),
        original_height: dimensions.map_or(0, |dimensions| dimensions.height),
        changed: false,
        original_byte_length: bytes.len(),
        final_byte_length: bytes.len(),
    };
    if bytes.is_empty() || !is_recodable_image(bytes, &normalized_mime) {
        return finish_compress(
            options,
            started_at,
            ImageCompressOutcome::PassthroughUnsupported,
            &normalized_mime,
            dimensions.and_then(|dimensions| dimensions.transposed).unwrap_or(false),
            passthrough(),
        );
    }
    let longest_edge = dimensions.map_or(0, |dimensions| dimensions.width.max(dimensions.height));
    let within_bytes = bytes.len() <= byte_budget;
    let within_edge = longest_edge > 0 && longest_edge <= max_edge;
    if within_bytes && (within_edge || longest_edge == 0) {
        return finish_compress(
            options,
            started_at,
            ImageCompressOutcome::PassthroughFast,
            &normalized_mime,
            dimensions.and_then(|dimensions| dimensions.transposed).unwrap_or(false),
            passthrough(),
        );
    }
    let too_many_pixels = dimensions.is_some_and(|dimensions| {
        dimensions.width as u64 * dimensions.height as u64 > MAX_DECODE_PIXELS
    });
    if too_many_pixels || bytes.len() > max_decode_bytes {
        return finish_compress(
            options,
            started_at,
            ImageCompressOutcome::PassthroughGuard,
            &normalized_mime,
            dimensions.and_then(|dimensions| dimensions.transposed).unwrap_or(false),
            passthrough(),
        );
    }
    let config = CompressConfig {
        max_edge,
        byte_budget,
        fallback_edges: FALLBACK_EDGES_PX.to_vec(),
        jpeg_quality_steps: JPEG_QUALITY_STEPS.to_vec(),
    };
    match compress_image(bytes, &normalized_mime, &config) {
        Some(result) if result.changed => finish_compress(
            options,
            started_at,
            ImageCompressOutcome::Compressed,
            &normalized_mime,
            dimensions.and_then(|dimensions| dimensions.transposed).unwrap_or(false),
            CompressImageResult::compressed(bytes, result),
        ),
        Some(_) => finish_compress(
            options,
            started_at,
            ImageCompressOutcome::PassthroughUnhelpful,
            &normalized_mime,
            dimensions.and_then(|dimensions| dimensions.transposed).unwrap_or(false),
            passthrough(),
        ),
        None => finish_compress(
            options,
            started_at,
            ImageCompressOutcome::PassthroughError,
            &normalized_mime,
            dimensions.and_then(|dimensions| dimensions.transposed).unwrap_or(false),
            passthrough(),
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressBase64Result {
    pub base64: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    pub changed: bool,
    pub original_byte_length: usize,
    pub final_byte_length: usize,
}

pub async fn compress_base64_for_model(
    base64: &str,
    mime_type: &str,
    options: &CompressImageOptions<'_>,
) -> CompressBase64Result {
    let started_at = Instant::now();
    let max_decode_bytes = options.max_decode_bytes.unwrap_or(MAX_IMAGE_DECODE_BYTES);
    let approximate_bytes = base64.encode_utf16().count().saturating_mul(3) / 4;
    if approximate_bytes > max_decode_bytes {
        let result = CompressBase64Result {
            base64: base64.to_string(),
            mime_type: mime_type.to_string(),
            width: 0,
            height: 0,
            original_width: 0,
            original_height: 0,
            changed: false,
            original_byte_length: approximate_bytes,
            final_byte_length: approximate_bytes,
        };
        report_compress_event(
            options,
            started_at,
            ImageCompressOutcome::PassthroughGuard,
            &normalize_image_mime(mime_type),
            false,
            &result,
        );
        return result;
    }
    let bytes = decode_lenient_base64(base64);
    let result = compress_image_for_model(&bytes, mime_type, options).await;
    if !result.changed {
        return CompressBase64Result {
            base64: base64.to_string(),
            mime_type: mime_type.to_string(),
            width: result.width,
            height: result.height,
            original_width: result.original_width,
            original_height: result.original_height,
            changed: false,
            original_byte_length: result.original_byte_length,
            final_byte_length: result.final_byte_length,
        };
    }
    CompressBase64Result {
        base64: base64::engine::general_purpose::STANDARD.encode(&result.data),
        mime_type: result.mime_type,
        width: result.width,
        height: result.height,
        original_width: result.original_width,
        original_height: result.original_height,
        changed: true,
        original_byte_length: result.original_byte_length,
        final_byte_length: result.final_byte_length,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompressedContentParts {
    pub parts: Vec<ContentPart>,
    pub captions: Vec<String>,
}

pub fn gate_image_format_parts(
    parts: &[ContentPart],
    provider_type: Option<&str>,
) -> Vec<ContentPart> {
    let mut out = Vec::new();
    for part in parts {
        if let ContentPart::ImageUrl { image_url } = part {
            let parsed = parse_image_data_url(&image_url.url);
            if parsed.is_none() {
                if is_data_url(&image_url.url) {
                    out.push(ContentPart::Text {
                        text: build_malformed_image_notice(&image_url.url),
                        meta: None,
                    });
                    continue;
                }
                if let Some(ext_mime) = unsupported_image_mime_from_url(&image_url.url, provider_type) {
                    out.push(ContentPart::Text {
                        text: build_unsupported_image_notice(
                            &ext_mime,
                            Some(&image_url.url),
                            provider_type,
                        ),
                        meta: None,
                    });
                    continue;
                }
                out.push(part.clone());
                continue;
            }
            let (parsed_mime, parsed_base64) = parsed.unwrap();
            let effective_mime = resolve_effective_image_mime(
                &parsed_mime,
                &decode_base64_prefix(&parsed_base64),
            );
            if !is_model_accepted_image_mime(&effective_mime, provider_type) {
                out.push(ContentPart::Text {
                    text: build_unsupported_image_notice(&effective_mime, None, provider_type),
                    meta: None,
                });
                continue;
            }
            let canonical_url = format!(
                "data:{};base64,{parsed_base64}",
                normalize_image_mime(&effective_mime)
            );
            if image_url.url != canonical_url {
                let mut image_url = image_url.clone();
                image_url.url = canonical_url;
                out.push(ContentPart::ImageUrl { image_url });
                continue;
            }
        }
        out.push(part.clone());
    }
    out
}

pub type PersistOriginal = Arc<
    dyn Fn(Vec<u8>, String) -> BoxFuture<'static, Option<String>> + Send + Sync + 'static,
>;

#[derive(Clone, Default)]
pub struct CompressAnnotateOptions {
    pub persist_original: Option<PersistOriginal>,
}

pub struct CompressContentPartsOptions<'a> {
    pub compress: CompressImageOptions<'a>,
    pub annotate: Option<CompressAnnotateOptions>,
    pub provider_type: Option<&'a str>,
    pub signal: Option<&'a AbortSignal>,
}

impl<'a> Default for CompressContentPartsOptions<'a> {
    fn default() -> Self {
        Self {
            compress: CompressImageOptions::default(),
            annotate: None,
            provider_type: None,
            signal: None,
        }
    }
}

pub async fn compress_image_content_parts(
    parts: &[ContentPart],
    options: &CompressContentPartsOptions<'_>,
) -> Result<CompressedContentParts, AbortError> {
    throw_if_aborted(options.signal)?;
    let mut out = Vec::new();
    let mut captions = Vec::new();
    for part in gate_image_format_parts(parts, options.provider_type) {
        throw_if_aborted(options.signal)?;
        if let ContentPart::ImageUrl { image_url } = &part {
            if let Some((parsed_mime, parsed_base64)) = parse_image_data_url(&image_url.url) {
                let result = compress_base64_for_model(
                    &parsed_base64,
                    &parsed_mime,
                    &options.compress,
                )
                .await;
                throw_if_aborted(options.signal)?;
                if result.changed {
                    if let Some(annotate) = &options.annotate {
                        let mut original_path = None;
                        if let Some(persist_original) = &annotate.persist_original {
                            let bytes = decode_lenient_base64(&parsed_base64);
                            original_path = persist_original(bytes, parsed_mime.clone()).await;
                            throw_if_aborted(options.signal)?;
                        }
                        captions.push(build_image_compression_caption(
                            &ImageVariantDescription {
                                width: result.original_width,
                                height: result.original_height,
                                byte_length: result.original_byte_length,
                                mime_type: parsed_mime,
                            },
                            &ImageVariantDescription {
                                width: result.width,
                                height: result.height,
                                byte_length: result.final_byte_length,
                                mime_type: result.mime_type.clone(),
                            },
                            original_path.as_deref(),
                        ));
                    }
                    let mut next_image_url = image_url.clone();
                    next_image_url.url = format!(
                        "data:{};base64,{}",
                        result.mime_type, result.base64
                    );
                    out.push(ContentPart::ImageUrl {
                        image_url: next_image_url,
                    });
                    continue;
                }
            }
        }
        out.push(part);
    }
    Ok(CompressedContentParts {
        parts: out,
        captions,
    })
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), AbortError> {
    if signal.is_some_and(|signal| signal.aborted()) {
        Err(abort_error())
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageCropRegion {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Default)]
pub struct CropImageOptions<'a> {
    pub max_edge: Option<u32>,
    pub byte_budget: Option<usize>,
    pub max_decode_bytes: Option<usize>,
    pub telemetry: Option<&'a dyn ITelemetryService>,
    pub telemetry_source: Option<&'a str>,
    pub skip_resize: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CropImageSuccess {
    pub data: Vec<u8>,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    pub region: ImageCropRegion,
    pub resized: bool,
    pub original_byte_length: usize,
    pub final_byte_length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CropImageFailure {
    pub error: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CropImageOutcome {
    Success(CropImageSuccess),
    Failure(CropImageFailure),
}

fn format_js_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value == f64::INFINITY {
        return "Infinity".to_string();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let magnitude = value.abs();
    let raw = if (1e-6..1e21).contains(&magnitude) {
        value.to_string()
    } else {
        format!("{value:e}")
    };
    let Some((mantissa, exponent)) = raw.split_once('e') else {
        return raw;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return raw;
    };
    format!("{mantissa}e{}{exponent}", if exponent >= 0 { "+" } else { "" })
}

pub async fn crop_image_for_model(
    bytes: &[u8],
    mime_type: &str,
    region: ImageCropRegion,
    options: &CropImageOptions<'_>,
) -> CropImageOutcome {
    let started_at = Instant::now();
    let normalized_mime = normalize_image_mime(mime_type);
    if bytes.is_empty() {
        return crop_failure(
            options,
            started_at,
            ImageCropErrorKind::Empty,
            "The image is empty.".to_string(),
        );
    }
    if !RECODABLE_MIME.contains(&normalized_mime.as_str()) {
        return crop_failure(
            options,
            started_at,
            ImageCropErrorKind::UnsupportedFormat,
            format!(
                "Cropping is only supported for PNG, JPEG, GIF, and WebP images; got {mime_type}."
            ),
        );
    }
    if normalized_mime == "image/webp" && is_animated_webp(bytes) {
        return crop_failure(
            options,
            started_at,
            ImageCropErrorKind::UnsupportedFormat,
            "Cropping is not supported for animated WebP images.".to_string(),
        );
    }
    if ![region.x, region.y, region.width, region.height]
        .into_iter()
        .all(|value| value.is_finite())
    {
        return crop_failure(
            options,
            started_at,
            ImageCropErrorKind::RegionInvalid,
            format!(
                "Region coordinates must be finite numbers; got x={}, y={}, width={}, height={}.",
                format_js_number(region.x),
                format_js_number(region.y),
                format_js_number(region.width),
                format_js_number(region.height)
            ),
        );
    }
    let dimensions = sniff_image_dimensions(bytes);
    if dimensions.is_some_and(|dimensions| {
        dimensions.width as u64 * dimensions.height as u64 > MAX_DECODE_PIXELS
    }) {
        let dimensions = dimensions.unwrap();
        return crop_failure(
            options,
            started_at,
            ImageCropErrorKind::TooLarge,
            format!(
                "The image ({}x{} pixels) is too large to decode for cropping.",
                dimensions.width, dimensions.height
            ),
        );
    }
    if bytes.len() > options.max_decode_bytes.unwrap_or(MAX_IMAGE_DECODE_BYTES) {
        return crop_failure(
            options,
            started_at,
            ImageCropErrorKind::TooLarge,
            "The image is too large to decode for cropping.".to_string(),
        );
    }
    let config = CropConfig {
        max_edge: options.max_edge.unwrap_or_else(resolve_max_image_edge_px),
        byte_budget: options.byte_budget.unwrap_or(IMAGE_BYTE_BUDGET),
        skip_resize: options.skip_resize,
        fallback_edges: FALLBACK_EDGES_PX.to_vec(),
        jpeg_quality_steps: JPEG_QUALITY_STEPS.to_vec(),
    };
    match crop_image(
        bytes,
        &normalized_mime,
        region.x,
        region.y,
        region.width,
        region.height,
        &config,
    ) {
        Ok(result) => crop_success(options, started_at, result),
        Err(CropError::Empty) => crop_failure(
            options,
            started_at,
            ImageCropErrorKind::Empty,
            "The image is empty.".to_string(),
        ),
        Err(CropError::UnsupportedFormat) => crop_failure(
            options,
            started_at,
            ImageCropErrorKind::UnsupportedFormat,
            format!(
                "Cropping is only supported for PNG, JPEG, GIF, and WebP images; got {mime_type}."
            ),
        ),
        Err(CropError::RegionInvalid) => crop_failure(
            options,
            started_at,
            ImageCropErrorKind::RegionInvalid,
            format!(
                "Region coordinates must be finite numbers; got x={}, y={}, width={}, height={}.",
                format_js_number(region.x),
                format_js_number(region.y),
                format_js_number(region.width),
                format_js_number(region.height)
            ),
        ),
        Err(CropError::OutOfBounds {
            original_width,
            original_height,
        }) => crop_failure(
            options,
            started_at,
            ImageCropErrorKind::OutOfBounds,
            format!(
                "Region (x={}, y={}, width={}, height={}) lies outside the {}x{} image.",
                format_js_number(region.x),
                format_js_number(region.y),
                format_js_number(region.width),
                format_js_number(region.height),
                original_width,
                original_height
            ),
        ),
        Err(CropError::Budget {
            encoded_bytes,
            budget,
        }) => crop_failure(
            options,
            started_at,
            ImageCropErrorKind::Budget,
            format!(
                "The cropped region encodes to {encoded_bytes} bytes ({}), over the {budget}-byte ({}) per-image limit. Choose a smaller region, or allow downscaling.",
                format_byte_size(encoded_bytes),
                format_byte_size(budget)
            ),
        ),
        Err(CropError::DecodeFailed(error)) => crop_failure(
            options,
            started_at,
            ImageCropErrorKind::DecodeFailed,
            format!("Failed to decode the image for cropping: {error}"),
        ),
    }
}

fn crop_success(
    options: &CropImageOptions<'_>,
    started_at: Instant,
    result: CropResult,
) -> CropImageOutcome {
    let success = CropImageSuccess {
        data: result.data,
        mime_type: result.mime_type,
        width: result.width,
        height: result.height,
        original_width: result.original_width,
        original_height: result.original_height,
        region: ImageCropRegion {
            x: result.region_x as f64,
            y: result.region_y as f64,
            width: result.region_width as f64,
            height: result.region_height as f64,
        },
        resized: result.resized,
        original_byte_length: result.original_byte_length,
        final_byte_length: result.final_byte_length,
    };
    report_crop_event(
        options.telemetry,
        options.telemetry_source,
        started_at,
        true,
        None,
        Some(&success),
    );
    CropImageOutcome::Success(success)
}

fn crop_failure(
    options: &CropImageOptions<'_>,
    started_at: Instant,
    error_kind: ImageCropErrorKind,
    error: String,
) -> CropImageOutcome {
    report_crop_event(
        options.telemetry,
        options.telemetry_source,
        started_at,
        false,
        Some(error_kind),
        None,
    );
    CropImageOutcome::Failure(CropImageFailure { error })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageVariantDescription {
    pub width: u32,
    pub height: u32,
    pub byte_length: usize,
    pub mime_type: String,
}

pub fn build_image_compression_caption(
    original: &ImageVariantDescription,
    final_variant: &ImageVariantDescription,
    original_path: Option<&str>,
) -> String {
    let mut sentences = vec![
        format!(
            "Image compressed to fit model limits: original {} -> sent {}.",
            describe_image_variant(original),
            describe_image_variant(final_variant)
        ),
        "Fine detail may be lost.".to_string(),
    ];
    if let Some(path) = original_path.filter(|path| !path.is_empty()) {
        sentences.push(format!(
            "The uncompressed original is saved at \"{path}\"; if you need fine detail (e.g. small text), call Read on that path with the region parameter (original-pixel coordinates) to view a crop at full fidelity."
        ));
    } else {
        sentences.push("The uncompressed original was not preserved.".to_string());
    }
    format!("<system>{}</system>", sentences.join(" "))
}

fn describe_image_variant(variant: &ImageVariantDescription) -> String {
    let size = format!(
        "{} ({})",
        variant.mime_type,
        format_byte_size(variant.byte_length)
    );
    if variant.width > 0 && variant.height > 0 {
        format!("{}x{} {size}", variant.width, variant.height)
    } else {
        size
    }
}

pub fn format_byte_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", (bytes as f64 / 1024.0).round())
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

trait CompressEventResult {
    fn mime_type(&self) -> &str;
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn original_width(&self) -> u32;
    fn original_height(&self) -> u32;
    fn original_byte_length(&self) -> usize;
    fn final_byte_length(&self) -> usize;
}

impl CompressEventResult for CompressImageResult {
    fn mime_type(&self) -> &str {
        &self.mime_type
    }
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn original_width(&self) -> u32 {
        self.original_width
    }
    fn original_height(&self) -> u32 {
        self.original_height
    }
    fn original_byte_length(&self) -> usize {
        self.original_byte_length
    }
    fn final_byte_length(&self) -> usize {
        self.final_byte_length
    }
}

impl CompressEventResult for CompressBase64Result {
    fn mime_type(&self) -> &str {
        &self.mime_type
    }
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn original_width(&self) -> u32 {
        self.original_width
    }
    fn original_height(&self) -> u32 {
        self.original_height
    }
    fn original_byte_length(&self) -> usize {
        self.original_byte_length
    }
    fn final_byte_length(&self) -> usize {
        self.final_byte_length
    }
}

fn finish_compress(
    options: &CompressImageOptions<'_>,
    started_at: Instant,
    outcome: ImageCompressOutcome,
    input_mime: &str,
    exif_transposed: bool,
    result: CompressImageResult,
) -> CompressImageResult {
    report_compress_event(
        options,
        started_at,
        outcome,
        input_mime,
        exif_transposed,
        &result,
    );
    result
}

fn report_compress_event(
    options: &CompressImageOptions<'_>,
    started_at: Instant,
    outcome: ImageCompressOutcome,
    input_mime: &str,
    exif_transposed: bool,
    result: &impl CompressEventResult,
) {
    let (Some(telemetry), Some(source)) = (options.telemetry, options.telemetry_source) else {
        return;
    };
    let event = ImageCompressEvent {
        source: source.to_string(),
        outcome,
        input_mime: input_mime.to_string(),
        output_mime: normalize_image_mime(result.mime_type()),
        original_bytes: result.original_byte_length() as u64,
        final_bytes: result.final_byte_length() as u64,
        original_width: result.original_width() as u64,
        original_height: result.original_height() as u64,
        final_width: result.width() as u64,
        final_height: result.height() as u64,
        exif_transposed,
        duration_ms: started_at.elapsed().as_millis() as u64,
    };
    track_event(telemetry, "image_compress", &event);
}

fn report_crop_event(
    telemetry: Option<&dyn ITelemetryService>,
    source: Option<&str>,
    started_at: Instant,
    ok: bool,
    error_kind: Option<ImageCropErrorKind>,
    result: Option<&CropImageSuccess>,
) {
    let (Some(telemetry), Some(source)) = (telemetry, source) else {
        return;
    };
    let original_pixels = result.map_or(0, |result| {
        result.original_width as u64 * result.original_height as u64
    });
    let event = ImageCropEvent {
        source: source.to_string(),
        ok,
        error_kind,
        resized: result.map(|result| result.resized),
        original_width: result.map(|result| result.original_width as u64),
        original_height: result.map(|result| result.original_height as u64),
        region_area_ratio: result.and_then(|result| {
            (original_pixels != 0).then(|| {
                result.region.width * result.region.height / original_pixels as f64
            })
        }),
        final_bytes: result.map(|result| result.final_byte_length as u64),
        duration_ms: started_at.elapsed().as_millis() as u64,
    };
    track_event(telemetry, "image_crop", &event);
}

fn track_event(telemetry: &dyn ITelemetryService, name: &str, event: &impl Serialize) {
    if let Ok(serde_json::Value::Object(properties)) = serde_json::to_value(event) {
        telemetry.track2(name, Some(properties));
    }
}
