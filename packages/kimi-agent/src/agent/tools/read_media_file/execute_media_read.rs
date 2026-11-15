use std::sync::Arc;

use base64::Engine;
use serde_json::Value;

use super::read_media_file::{
    MAX_MEDIA_BYTES, MAX_MEDIA_MEGABYTES, VideoUploadOptions, VideoUploader,
};
use crate::_base::utils::abort::AbortSignal;
use crate::agent::media::file_type::{
    DetectFileTypeMode, FileKind, detect_file_type, sniff_image_dimensions,
};
use crate::agent::media::image_compress::{
    CompressImageOptions, CropImageOptions, CropImageOutcome, ImageCropRegion,
    compress_image_for_model, crop_image_for_model, is_recodable_image,
    resolve_max_image_edge_px, resolve_read_image_byte_budget,
};
use crate::agent::media::image_format_policy::{
    is_model_accepted_image_mime, normalize_image_mime, provider_image_policy,
};
use crate::agent::tools::file_read_source::FileReadSource;
use crate::human::llm::media::upload::VideoUploadInput;
use crate::human::llm::message::{ContentPart, DocumentUrl, ImageDimensions, ImageUrl, VideoUrl};
use crate::llm_adapter::contract::capability::{ModelCapability, is_unknown_capability};
use crate::llm_adapter::contract::errors::VideoUploadUnsupportedError;
use crate::native::file_type::MediaSniffKind;
use crate::os::interface::host_environment::HostEnvironmentInfo;
use crate::wire::record::WireAsyncError;
use kimi_core::types::UploadError;

pub const MAX_IMAGE_DECODE_BYTES: u64 = 64 * 1024 * 1024;
pub const READ_IMAGE_BYTE_BUDGET: u64 = 256 * 1024;
pub const MAX_IMAGE_EDGE_PX: u32 = 2000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaReadRegion {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaReadArgs {
    pub path: String,
    pub region: Option<MediaReadRegion>,
    pub full_resolution: Option<bool>,
}

pub type MediaTelemetry = Arc<dyn Fn(&str, Value) + Send + Sync>;

#[derive(Clone)]
pub struct MediaReadContext {
    pub capabilities: ModelCapability,
    pub video_uploader: Option<VideoUploader>,
    pub inline_video_supported: bool,
    pub provider_type: Option<String>,
    pub telemetry: Option<MediaTelemetry>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaReadOutput {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaReadResult {
    pub output: MediaReadOutput,
    pub note: Option<String>,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
struct DetectedMedia {
    kind: MediaSniffKind,
    mime_type: String,
}

#[derive(Debug, Clone)]
enum MediaFileType {
    Text,
    Unknown,
    Media(DetectedMedia),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageDeliveryKind {
    Untouched,
    Downsampled,
    Crop,
    Full,
}

#[derive(Debug, Clone)]
struct ImageDelivery {
    kind: ImageDeliveryKind,
    width: u32,
    height: u32,
    byte_length: usize,
    mime_type: String,
    region: Option<ImageCropRegion>,
    resized: bool,
}

fn failure(output: impl Into<String>) -> MediaReadResult {
    MediaReadResult {
        output: MediaReadOutput::Text(output.into()),
        note: None,
        is_error: true,
    }
}

fn text(text: impl Into<String>) -> ContentPart {
    ContentPart::Text {
        text: text.into(),
        meta: None,
    }
}

pub async fn execute_media_read(
    ctx: &MediaReadContext,
    args: &MediaReadArgs,
    source: &dyn FileReadSource,
    env: Option<&HostEnvironmentInfo>,
    header: &[u8],
    signal: Option<AbortSignal>,
) -> Result<MediaReadResult, WireAsyncError> {
    match execute_inner(ctx, args, source, env, header, signal.clone()).await {
        Ok(result) => Ok(result),
        Err(error) if signal.as_ref().is_some_and(AbortSignal::aborted) => Err(error),
        Err(error) => Ok(failure(format!("Failed to read {}: {error}", args.path))),
    }
}

async fn execute_inner(
    ctx: &MediaReadContext,
    args: &MediaReadArgs,
    source: &dyn FileReadSource,
    env: Option<&HostEnvironmentInfo>,
    header: &[u8],
    signal: Option<AbortSignal>,
) -> Result<MediaReadResult, WireAsyncError> {
    if let Some(signal) = signal.as_ref() {
        signal.throw_if_aborted()?;
    }
    let safe_path = source.name().to_string();
    let file_type = detect_media_type(&safe_path, header);
    let detected = match file_type {
        MediaFileType::Text => {
            return Ok(failure(format!(
                "\"{}\" is a text file. Use Read without region or full_resolution to read text files.",
                args.path
            )));
        }
        MediaFileType::Unknown => {
            return Ok(failure(format!(
                "\"{}\" is not a supported image or video file. Use Read for text files, or Bash or an MCP tool for other binary formats.",
                args.path
            )));
        }
        MediaFileType::Media(detected) => detected,
    };
    let capabilities_unknown = is_unknown_capability(&ctx.capabilities);
    if detected.kind == MediaSniffKind::Image
        && !ctx.capabilities.image_in
        && !capabilities_unknown
    {
        return Ok(failure(
            "The current model does not support image input. Tell the user to use a model with image input capability.",
        ));
    }
    if detected.kind == MediaSniffKind::Image
        && !is_model_accepted_image_mime(&detected.mime_type, ctx.provider_type.as_deref())
    {
        return Ok(failure(build_image_conversion_guidance(
            &args.path,
            &detected.mime_type,
            env.map(|env| env.os_kind.as_str()),
        )));
    }
    if detected.kind == MediaSniffKind::Video
        && !ctx.capabilities.video_in
        && !capabilities_unknown
    {
        return Ok(failure(
            "The current model does not support video input. Tell the user to use a model with video input capability.",
        ));
    }
    if detected.kind == MediaSniffKind::Document
        && ctx.capabilities.pdf_in != Some(true)
        && !capabilities_unknown
    {
        return Ok(failure(
            "The current model does not support PDF input. Tell the user to use a model with PDF input capability, or convert the file to text first.",
        ));
    }
    let stat = source.stat().await?;
    if stat.size == 0 {
        return Ok(failure(format!("\"{}\" is empty.", args.path)));
    }
    if stat.size > MAX_MEDIA_BYTES {
        return Ok(failure(format!(
            "\"{}\" is {} bytes, which exceeds the maximum {MAX_MEDIA_MEGABYTES}MB for media files.",
            args.path, stat.size
        )));
    }
    if matches!(detected.kind, MediaSniffKind::Video | MediaSniffKind::Document)
        && (args.region.is_some() || args.full_resolution == Some(true))
    {
        return Ok(failure("region and full_resolution apply only to image files."));
    }
    if detected.kind == MediaSniffKind::Image
        && stat.size > MAX_IMAGE_DECODE_BYTES
        && (args.region.is_some() || args.full_resolution == Some(true))
    {
        return Ok(failure(build_image_decode_limit_error(stat.size)));
    }
    let inline_image_byte_budget = provider_image_policy(ctx.provider_type.as_deref())
        .inline_byte_budget as u64;
    if detected.kind == MediaSniffKind::Image
        && args.region.is_none()
        && args.full_resolution == Some(true)
        && stat.size > inline_image_byte_budget
    {
        return Ok(failure(build_full_resolution_limit_error(
            &args.path,
            stat.size,
            inline_image_byte_budget,
        )));
    }
    let read_byte_budget = resolve_read_image_byte_budget() as u64;
    let max_edge = resolve_max_image_edge_px();
    if detected.kind == MediaSniffKind::Image
        && args.region.is_none()
        && args.full_resolution != Some(true)
        && stat.size > MAX_IMAGE_DECODE_BYTES
        && stat.size > read_byte_budget
    {
        return Ok(failure(build_image_delivery_limit_error(
            stat.size,
            read_byte_budget,
            max_edge,
        )));
    }
    let data = source.read_bytes(None).await?;
    let mut dimensions = (detected.kind == MediaSniffKind::Image)
        .then(|| sniff_image_dimensions(&data))
        .flatten()
        .map(|dimensions| ImageDimensions {
            width: dimensions.width,
            height: dimensions.height,
        });
    let mut delivery = None;
    let media_part = match detected.kind {
        MediaSniffKind::Image => {
            if let Some(region) = args.region {
                let outcome = crop_image_for_model(
                    &data,
                    &detected.mime_type,
                    ImageCropRegion {
                        x: region.x,
                        y: region.y,
                        width: region.width,
                        height: region.height,
                    },
                    &CropImageOptions {
                        max_edge: Some(max_edge),
                        byte_budget: Some(read_byte_budget as usize),
                        skip_resize: args.full_resolution == Some(true),
                        ..CropImageOptions::default()
                    },
                )
                .await;
                let outcome = match outcome {
                    CropImageOutcome::Success(outcome) => outcome,
                    CropImageOutcome::Failure(outcome) => {
                        return Ok(failure(format!(
                            "Cannot read region from \"{}\": {}",
                            args.path, outcome.error
                        )));
                    }
                };
                let dimensions_value = (outcome.width > 0 && outcome.height > 0)
                    .then_some(ImageDimensions {
                        width: outcome.width,
                        height: outcome.height,
                    });
                dimensions = Some(ImageDimensions {
                    width: outcome.original_width,
                    height: outcome.original_height,
                });
                delivery = Some(ImageDelivery {
                    kind: ImageDeliveryKind::Crop,
                    width: outcome.width,
                    height: outcome.height,
                    byte_length: outcome.final_byte_length,
                    mime_type: outcome.mime_type.clone(),
                    region: Some(outcome.region),
                    resized: outcome.resized,
                });
                image_part(
                    &outcome.mime_type,
                    &outcome.data,
                    dimensions_value,
                )
            } else if args.full_resolution == Some(true) {
                if data.len() as u64 > inline_image_byte_budget {
                    return Ok(failure(build_full_resolution_limit_error(
                        &args.path,
                        data.len() as u64,
                        inline_image_byte_budget,
                    )));
                }
                delivery = Some(ImageDelivery {
                    kind: ImageDeliveryKind::Full,
                    width: dimensions.as_ref().map_or(0, |value| value.width),
                    height: dimensions.as_ref().map_or(0, |value| value.height),
                    byte_length: data.len(),
                    mime_type: detected.mime_type.clone(),
                    region: None,
                    resized: false,
                });
                image_part(
                    &detected.mime_type,
                    &data,
                    dimensions
                        .clone()
                        .filter(|dimensions| dimensions.width > 0 && dimensions.height > 0),
                )
            } else {
                let inline_only = !is_recodable_image(&data, &detected.mime_type);
                let compressed = compress_image_for_model(
                    &data,
                    &detected.mime_type,
                    &CompressImageOptions {
                        max_edge: Some(max_edge),
                        byte_budget: Some(read_byte_budget as usize),
                        ..CompressImageOptions::default()
                    },
                )
                .await;
                if inline_only {
                    let inline_limit = read_byte_budget.max(inline_image_byte_budget);
                    if compressed.final_byte_length as u64 > inline_limit {
                        return Ok(failure(build_oversized_image_conversion_guidance(
                            &args.path,
                            &detected.mime_type,
                            env.map(|env| env.os_kind.as_str()).unwrap_or(""),
                            compressed.final_byte_length as u64,
                            inline_limit,
                        )));
                    }
                } else if compressed.final_byte_length as u64 > read_byte_budget
                    || compressed.width.max(compressed.height) > max_edge
                {
                    return Ok(failure(build_image_delivery_limit_error(
                        compressed.final_byte_length as u64,
                        read_byte_budget,
                        max_edge,
                    )));
                }
                let part = image_part(
                    &compressed.mime_type,
                    &compressed.data,
                    (compressed.width > 0 && compressed.height > 0).then_some(
                        ImageDimensions {
                            width: compressed.width,
                            height: compressed.height,
                        },
                    ),
                );
                delivery = Some(ImageDelivery {
                    kind: if compressed.changed {
                        ImageDeliveryKind::Downsampled
                    } else {
                        ImageDeliveryKind::Untouched
                    },
                    width: compressed.width,
                    height: compressed.height,
                    byte_length: compressed.final_byte_length,
                    mime_type: compressed.mime_type.clone(),
                    region: None,
                    resized: false,
                });
                if compressed.changed {
                    dimensions = Some(ImageDimensions {
                        width: compressed.original_width,
                        height: compressed.original_height,
                    });
                }
                part
            }
        }
        MediaSniffKind::Document => ContentPart::DocumentUrl {
            document_url: DocumentUrl {
                url: format!(
                    "data:{};base64,{}",
                    detected.mime_type,
                    base64::engine::general_purpose::STANDARD.encode(&data)
                ),
                mime_type: detected.mime_type.clone(),
                id: None,
                name: Some(safe_path.clone()),
            },
        },
        MediaSniffKind::Video => {
            video_content_part(ctx, data, &detected.mime_type, &safe_path, signal).await?
        }
    };
    let tag = match detected.kind {
        MediaSniffKind::Image => "image",
        MediaSniffKind::Video => "video",
        MediaSniffKind::Document => "document",
    };
    let note = build_media_note(
        tag,
        &detected.mime_type,
        stat.size,
        dimensions.as_ref(),
        delivery.as_ref(),
    );
    Ok(MediaReadResult {
        output: MediaReadOutput::Parts(vec![
            text(format!("<{tag} path=\"{safe_path}\">")),
            media_part,
            text(format!("</{tag}>")),
        ]),
        note: Some(note),
        is_error: false,
    })
}

fn detect_media_type(path: &str, header: &[u8]) -> MediaFileType {
    let file_type = detect_file_type(path, Some(header), DetectFileTypeMode::Media);
    let kind = match file_type.kind {
        FileKind::Text => return MediaFileType::Text,
        FileKind::Unknown => return MediaFileType::Unknown,
        FileKind::Image => MediaSniffKind::Image,
        FileKind::Video => MediaSniffKind::Video,
        FileKind::Document => MediaSniffKind::Document,
    };
    MediaFileType::Media(DetectedMedia {
        kind,
        mime_type: file_type.mime_type,
    })
}

fn image_part(mime_type: &str, data: &[u8], dimensions: Option<ImageDimensions>) -> ContentPart {
    ContentPart::ImageUrl {
        image_url: ImageUrl {
            url: format!(
                "data:{mime_type};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(data)
            ),
            id: None,
            name: None,
            dimensions,
            detail: None,
        },
    }
}

async fn video_content_part(
    ctx: &MediaReadContext,
    data: Vec<u8>,
    mime_type: &str,
    safe_path: &str,
    signal: Option<AbortSignal>,
) -> Result<ContentPart, WireAsyncError> {
    if let Some(signal) = signal.as_ref() {
        signal.throw_if_aborted()?;
    }
    if let Some(uploader) = ctx.video_uploader.as_ref() {
        let filename = safe_path
            .rsplit(['/', '\\'])
            .next()
            .map(str::to_string);
        let result = uploader(
            VideoUploadInput {
                data: data.clone(),
                mime_type: mime_type.to_string(),
                filename,
            },
            VideoUploadOptions {
                signal: signal.clone(),
            },
        )
        .await;
        match result {
            Ok(part) => return Ok(part),
            Err(error) => {
                if signal.as_ref().is_some_and(AbortSignal::aborted)
                    || should_surface_video_upload_error(&error, ctx.inline_video_supported)
                {
                    return Err(error);
                }
            }
        }
    }
    Ok(ContentPart::VideoUrl {
        video_url: VideoUrl {
            url: format!(
                "data:{mime_type};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(data)
            ),
            id: None,
            name: None,
        },
    })
}

fn should_surface_video_upload_error(error: &WireAsyncError, inline_video_supported: bool) -> bool {
    if error.downcast_ref::<VideoUploadUnsupportedError>().is_some() {
        return !inline_video_supported;
    }
    if matches!(error.downcast_ref::<UploadError>(), Some(UploadError::Auth(_))) {
        return true;
    }
    if error
        .downcast_ref::<reqwest::Error>()
        .and_then(|error| error.status())
        .is_some_and(|status| status.as_u16() == 401 || status.as_u16() == 403)
    {
        return true;
    }
    let debug = format!("{error:?}");
    ["statusCode: 401", "statusCode: 403", "status: 401", "status: 403"]
        .iter()
        .any(|pattern| debug.contains(pattern))
}

fn build_media_note(
    kind: &str,
    mime_type: &str,
    byte_size: u64,
    dimensions: Option<&ImageDimensions>,
    delivery: Option<&ImageDelivery>,
) -> String {
    let mut parts = vec![
        format!("Read {kind} file."),
        format!("Mime type: {mime_type}."),
        format!("Size: {byte_size} bytes."),
    ];
    if kind == "image"
        && let Some(dimensions) = dimensions
    {
        parts.push(format!(
            "Original dimensions: {}x{} pixels.",
            dimensions.width, dimensions.height
        ));
    }
    if let Some(delivery) = delivery {
        match delivery.kind {
            ImageDeliveryKind::Downsampled => {
                parts.push(format!(
                    "The attached image was downsampled to {}x{} pixels ({}, {}) to fit model limits; fine detail may be lost.",
                    delivery.width,
                    delivery.height,
                    delivery.mime_type,
                    format_byte_size(delivery.byte_length as u64)
                ));
                parts.push(
                    "To inspect fine detail, call Read again with the region parameter (original-image pixel coordinates) to view a crop at full fidelity."
                        .to_string(),
                );
            }
            ImageDeliveryKind::Crop => {
                if let Some(region) = delivery.region {
                    parts.push(format!(
                        "Showing region (x={}, y={}, width={}, height={}) of the original image{}.",
                        region.x,
                        region.y,
                        region.width,
                        region.height,
                        if delivery.resized {
                            format!(", downsampled to {}x{} pixels", delivery.width, delivery.height)
                        } else {
                            " at native resolution".to_string()
                        }
                    ));
                    parts.push(format!(
                        "To output coordinates in original-image pixels, locate them within this crop and add the region offset (x={}, y={}).",
                        region.x, region.y
                    ));
                }
            }
            ImageDeliveryKind::Full => {
                parts.push("Shown at native resolution; no downscaling applied.".to_string());
            }
            ImageDeliveryKind::Untouched => {}
        }
    }
    if kind == "image"
        && dimensions.is_some()
        && delivery.is_none_or(|delivery| delivery.kind != ImageDeliveryKind::Crop)
    {
        parts.push(
            "If you need to output coordinates, output relative coordinates first and compute absolute coordinates using the original image size."
                .to_string(),
        );
    }
    parts.push(
        "If you generate or edit images or videos via commands or scripts, read the result back immediately before continuing."
            .to_string(),
    );
    format!("<system>{}</system>", parts.join(" "))
}

fn build_image_delivery_limit_error(
    final_bytes: u64,
    read_byte_budget: u64,
    max_edge: u32,
) -> String {
    format!(
        "Image is too large to send safely after compression ({final_bytes} bytes; limit {read_byte_budget} bytes and {max_edge}px on the longest edge). The original image was not sent to the model. Do not retry the same file unchanged. Use Bash or an available image-processing tool to create a smaller copy within both limits, then call Read on the smaller copy."
    )
}

fn build_image_decode_limit_error(final_bytes: u64) -> String {
    format!(
        "Image is too large to process safely for region or full_resolution ({final_bytes} bytes; safe decode limit {MAX_IMAGE_DECODE_BYTES} bytes). The original image was not sent to the model. Do not retry the same file unchanged. Use Bash or an available image-processing tool to create a smaller copy or crop the needed region into a separate image, then call Read on the resulting file."
    )
}

fn build_full_resolution_limit_error(path: &str, final_bytes: u64, inline_budget: u64) -> String {
    format!(
        "\"{path}\" is {final_bytes} bytes ({}), over the {inline_budget}-byte ({}) per-image limit, so full_resolution cannot be honored. Use region to view a crop at full fidelity instead.",
        format_byte_size(final_bytes),
        format_byte_size(inline_budget)
    )
}

fn format_byte_size(bytes: u64) -> String {
    crate::agent::media::image_compress::format_byte_size(bytes as usize)
}

fn build_image_conversion_guidance(path: &str, mime_type: &str, os_kind: Option<&str>) -> String {
    let converted = converted_path(path);
    format!(
        "\"{path}\" is an {mime_type} image, which the provider does not accept. Convert it to JPEG first, then read the converted file. {}",
        image_conversion_command(path, &converted, mime_type, os_kind)
    )
}

fn build_oversized_image_conversion_guidance(
    path: &str,
    mime_type: &str,
    os_kind: &str,
    byte_length: u64,
    inline_budget: u64,
) -> String {
    let converted = converted_path(path);
    format!(
        "\"{path}\" is a {byte_length}-byte {mime_type} image, over the {inline_budget}-byte per-image limit, and this format cannot be downsampled locally. Convert it to JPEG first, then read the converted file. {}",
        image_conversion_command(path, &converted, mime_type, Some(os_kind))
    )
}

fn converted_path(path: &str) -> String {
    let last_separator = path.rfind(['/', '\\']).map_or(0, |index| index + 1);
    match path
        .rfind('.')
        .filter(|index| *index >= last_separator && *index + 1 < path.len())
    {
        Some(index) => format!("{}.jpg", &path[..index]),
        None => format!("{path}.jpg"),
    }
}

fn image_conversion_command(path: &str, converted: &str, mime_type: &str, os_kind: Option<&str>) -> String {
    let magick = format!("magick \"{path}\" \"{converted}\"");
    let decoder = matches!(normalize_image_mime(mime_type).as_str(), "image/heic" | "image/heif")
        .then_some(("heif-convert", "libheif-examples"));
    match os_kind {
        Some("macOS") => format!("On macOS: sips -s format jpeg \"{path}\" --out \"{converted}\""),
        Some("Linux") => match decoder {
            Some((command, package)) => format!(
                "On Linux: {command} \"{path}\" \"{converted}\" (package {package}), or with ImageMagick: {magick}"
            ),
            None => format!("On Linux, with ImageMagick: {magick}"),
        },
        Some("Windows") => format!(
            "On Windows, with ImageMagick: {magick} (install it first if missing: winget install ImageMagick.ImageMagick)"
        ),
        _ => {
            let decoder = decoder
                .map(|(command, package)| {
                    format!(", {command} \"{path}\" \"{converted}\" (Linux, package {package})")
                })
                .unwrap_or_default();
            format!(
                "Options: sips -s format jpeg \"{path}\" --out \"{converted}\" (macOS){decoder}, or {magick} (ImageMagick)"
            )
        }
    }
}
