use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use parking_lot::Mutex;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::_base::utils::abort::{AbortSignal, abort_error};
use crate::_base::utils::file_meta::text_extension_for_mime;
use crate::agent::media::file_type::{FileKind, sniff_media_from_magic};
use crate::agent::media::image_compress::{
    CompressAnnotateOptions, CompressContentPartsOptions, CompressImageOptions, PersistOriginal,
    compress_image_content_parts, gate_image_format_parts,
};
use crate::agent::media::image_format_policy::{
    build_unsupported_image_notice, decode_base64_prefix, is_model_accepted_image_mime,
    parse_image_data_url, resolve_effective_image_mime,
};
use crate::agent::media::media_ref::{build_daemon_file_url, media_extension_for_mime};
use crate::agent::media::session_media_store::{ISessionMediaStore, SessionMediaMaterializeInput};
use crate::agent::tool_executor::tool_executor::{
    ExecutableToolOutput, ExecutableToolResult, ToolResultSpill,
};
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::environment::environment::EnvironmentPath;
use crate::human::llm::message::{AudioUrl, ContentPart, DocumentUrl, ImageUrl, VideoUrl};
use crate::mcp_core::types::{MCPContentBlock, MCPToolResult};
use crate::os::interface::hostFileSystem::IHostFileSystem;
use crate::persistence::interface::blobStore::BlobByteStream;
use crate::wire::record::WireAsyncError;

pub const MCP_MAX_BINARY_PART_BYTES: usize = 10 * 1024 * 1024;
const MCP_MAX_BINARY_PART_CHARS: usize = (MCP_MAX_BINARY_PART_BYTES * 4).div_ceil(3);
const MCP_MAX_INLINE_NOTICES_CHARS: usize = 4096;
const DEFAULT_MAX_ORIGINALS_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
static LENIENT_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

#[derive(Clone)]
pub struct McpOriginalsTarget {
    pub fs: Arc<dyn IHostFileSystem>,
    pub dir: String,
    pub path: Option<EnvironmentPath>,
}

#[derive(Clone, Default)]
pub struct McpOutputOptions {
    pub signal: Option<AbortSignal>,
    pub attachment_store: Option<Arc<dyn ISessionMediaStore>>,
    pub originals: Option<McpOriginalsTarget>,
    pub telemetry: Option<Arc<dyn ITelemetryService>>,
    pub provider_type: Option<String>,
}

#[derive(Default)]
struct McpOutputState {
    attachment_notices: Vec<String>,
    preserved_urls: HashMap<String, Option<String>>,
}

fn binary_part_too_large_notice(kind: &str, url_length: usize) -> String {
    let approx_mb = url_length as f64 * 3.0 / 4.0 / (1024.0 * 1024.0);
    let cap_mb = MCP_MAX_BINARY_PART_BYTES / (1024 * 1024);
    format!(
        "[{kind}_url dropped: ~{approx_mb:.1} MB exceeds {cap_mb} MB per-part limit. Try a smaller resource.]"
    )
}

fn dropped_block_notice(reason: &str) -> ContentPart {
    ContentPart::Text {
        text: format!("[MCP content dropped: {reason}]"),
        meta: None,
    }
}

pub fn convert_mcp_content_block(
    block: &MCPContentBlock,
    provider_type: Option<&str>,
) -> ContentPart {
    if block.type_ == "text" {
        if let Some(text) = &block.text {
            return ContentPart::Text {
                text: text.clone(),
                meta: None,
            };
        }
    }
    if block.type_ == "image" {
        if let Some(data) = &block.data {
            let mime_type = block.mime_type.as_deref().unwrap_or("image/png");
            return ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: format!("data:{mime_type};base64,{data}"),
                    id: None,
                    name: None,
                    dimensions: None,
                    detail: None,
                },
            };
        }
    }
    if block.type_ == "audio" {
        if let Some(data) = &block.data {
            let mime_type = block.mime_type.as_deref().unwrap_or("audio/mpeg");
            return ContentPart::AudioUrl {
                audio_url: AudioUrl {
                    url: format!("data:{mime_type};base64,{data}"),
                    id: None,
                },
            };
        }
    }
    if block.type_ == "video" {
        if let Some(data) = &block.data {
            let mime_type = block.mime_type.as_deref().unwrap_or("video/mp4");
            return ContentPart::VideoUrl {
                video_url: VideoUrl {
                    url: format!("data:{mime_type};base64,{data}"),
                    id: None,
                    name: None,
                },
            };
        }
    }
    if block.type_ == "resource" {
        if let Some(resource) = &block.resource {
            if let Some(text) = &resource.text {
                return ContentPart::Text {
                    text: text.clone(),
                    meta: None,
                };
            }
            if let Some(blob) = &resource.blob {
                let mime_type = resource
                    .mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream");
                if mime_type.starts_with("image/") {
                    return ContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: format!("data:{mime_type};base64,{blob}"),
                            id: None,
                            name: None,
                            dimensions: None,
                            detail: None,
                        },
                    };
                }
                if mime_type.starts_with("audio/") {
                    return ContentPart::AudioUrl {
                        audio_url: AudioUrl {
                            url: format!("data:{mime_type};base64,{blob}"),
                            id: None,
                        },
                    };
                }
                if mime_type.starts_with("video/") {
                    return ContentPart::VideoUrl {
                        video_url: VideoUrl {
                            url: format!("data:{mime_type};base64,{blob}"),
                            id: None,
                            name: None,
                        },
                    };
                }
                let approx_mb = blob.encode_utf16().count() as f64 * 3.0 / 4.0 / (1024.0 * 1024.0);
                return dropped_block_notice(&format!(
                    "resource blob with unsupported mimeType \"{mime_type}\" (~{approx_mb:.1} MB, uri: {}) was not delivered.",
                    resource.uri
                ));
            }
            return dropped_block_notice(&format!(
                "resource (uri: {}) carried no text or blob payload.",
                resource.uri
            ));
        }
    }
    if block.type_ == "resource_link" {
        if let Some(uri) = &block.uri {
            if uri.is_empty() {
                return dropped_block_notice("resource_link carried an empty uri.");
            }
            let mime_type = block
                .mime_type
                .as_deref()
                .unwrap_or("application/octet-stream");
            if mime_type.starts_with("image/") {
                if !is_model_accepted_image_mime(mime_type, provider_type) {
                    return ContentPart::Text {
                        text: build_unsupported_image_notice(mime_type, Some(uri), provider_type),
                        meta: None,
                    };
                }
                return ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: uri.clone(),
                        id: None,
                        name: None,
                        dimensions: None,
                        detail: None,
                    },
                };
            }
            if mime_type.starts_with("audio/") {
                return ContentPart::AudioUrl {
                    audio_url: AudioUrl {
                        url: uri.clone(),
                        id: None,
                    },
                };
            }
            if mime_type.starts_with("video/") {
                return ContentPart::VideoUrl {
                    video_url: VideoUrl {
                        url: uri.clone(),
                        id: None,
                        name: None,
                    },
                };
            }
            return dropped_block_notice(&format!(
                "resource_link with unsupported mimeType \"{mime_type}\" was not delivered. Fetch it directly if needed: {uri}"
            ));
        }
    }
    dropped_block_notice(&format!(
        "content block of unsupported type \"{}\" was not delivered.",
        block.type_
    ))
}

pub async fn mcp_result_to_executable_output(
    result: &MCPToolResult,
    qualified_tool_name: &str,
    options: &McpOutputOptions,
) -> Result<ExecutableToolResult, WireAsyncError> {
    throw_if_aborted(options.signal.as_ref())?;
    let mut converted = Vec::new();
    let mut state = McpOutputState::default();
    let mut omitted_attachment = false;
    for block in &result.content {
        throw_if_aborted(options.signal.as_ref())?;
        let part = convert_mcp_content_block(block, options.provider_type.as_deref());
        let gated = gate_image_format_parts(
            std::slice::from_ref(&part),
            options.provider_type.as_deref(),
        );
        if matches!(part, ContentPart::ImageUrl { .. }) && options.attachment_store.is_some() {
            let ContentPart::ImageUrl { image_url } = &part else {
                unreachable!()
            };
            let reference = preserve_inline_media(&image_url.url, options, &mut state).await?;
            if let Some(ContentPart::ImageUrl { image_url }) = gated.first() {
                state
                    .preserved_urls
                    .insert(image_url.url.clone(), reference);
            }
        }
        if let ContentPart::AudioUrl { audio_url } = &part {
            preserve_inline_media(&audio_url.url, options, &mut state).await?;
        }
        if let ContentPart::VideoUrl { video_url } = &part {
            preserve_inline_media(&video_url.url, options, &mut state).await?;
        }
        let gated_is_text = matches!(gated.first(), Some(ContentPart::Text { .. }));
        converted.extend(gated);
        if matches!(part, ContentPart::ImageUrl { .. }) && gated_is_text {
            omitted_attachment = true;
            let ContentPart::ImageUrl { image_url } = &part else {
                unreachable!()
            };
            preserve_inline_media(&image_url.url, options, &mut state).await?;
        }
        if matches!(part, ContentPart::Text { .. })
            && block.type_ == "resource"
            && block.resource.as_ref().is_some_and(|resource| {
                resource.blob.is_some() && resource.text.is_none()
            })
        {
            omitted_attachment = true;
            let resource = block.resource.as_ref().unwrap();
            let notice = preserve_attachment(
                resource.blob.as_deref().unwrap(),
                resource
                    .mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
                options,
            )
            .await?;
            state.attachment_notices.push(notice);
        }
    }

    let has_structured_copy = result.structured_content.as_ref().is_some_and(|structured| {
        converted.iter().any(|part| match part {
            ContentPart::Text { text, .. } => parse_comparable_json(text)
                .is_some_and(|value| json_deep_strict_equal(&value, structured)),
            _ => false,
        })
    });
    let wrapped = wrap_media_only(converted, qualified_tool_name);
    let mut structured_extras = Map::new();
    if let Some(structured) = &result.structured_content {
        if !has_structured_copy {
            structured_extras.insert("structuredContent".to_string(), structured.clone());
        }
    }
    if let Some(meta) = &result.meta {
        if let Some(meta) = strip_reserved_meta_keys(meta) {
            structured_extras.insert("_meta".to_string(), Value::Object(meta));
        }
    }
    let mut wrapped = wrapped;
    if !structured_extras.is_empty() {
        if let Ok(serialized) = serialize_structured_extras(&structured_extras) {
            wrapped.push(ContentPart::Text {
                text: format!(
                    "\n<mcp-result-extras>\n{}\n</mcp-result-extras>",
                    serialized.replace('<', "\\u003c")
                ),
                meta: None,
            });
        }
    }

    let compression_notices = Arc::new(Mutex::new(Vec::new()));
    let persist_original: PersistOriginal = Arc::new({
        let options = options.clone();
        let compression_notices = compression_notices.clone();
        move |bytes, mime_type| {
            let options = options.clone();
            let compression_notices = compression_notices.clone();
            Box::pin(async move {
                if let Some(store) = &options.attachment_store {
                    match save_attachment(&bytes, &mime_type, store, options.signal.clone()).await {
                        Ok(saved) => {
                            compression_notices.lock().push(attachment_notice(
                                &saved,
                                &mime_type,
                                bytes.len(),
                            ));
                            Some(saved.reference)
                        }
                        Err(_) => None,
                    }
                } else {
                    persist_original_for_output(&bytes, &mime_type, options.originals.as_ref()).await
                }
            })
        }
    });
    let compressed = compress_image_content_parts(
        &wrapped,
        &CompressContentPartsOptions {
            compress: CompressImageOptions {
                telemetry: options.telemetry.as_deref(),
                telemetry_source: Some("mcp_tool_result"),
                ..Default::default()
            },
            annotate: Some(CompressAnnotateOptions {
                persist_original: Some(persist_original),
            }),
            provider_type: options.provider_type.as_deref(),
            signal: options.signal.as_ref(),
        },
    )
    .await?;
    state
        .attachment_notices
        .extend(compression_notices.lock().drain(..));
    let capped = apply_binary_part_cap(compressed.parts, options, &mut state).await?;
    let mut parts = Vec::new();
    for part in capped.parts {
        parts.push(reference_inline_image(part, options, &state.preserved_urls).await?);
    }
    let mut notices = compressed.captions;
    notices.extend(state.attachment_notices);
    notices.extend(capped.notices);
    let notices = attachment_details(&notices, options).await?;
    if !notices.content.is_empty() {
        parts.push(ContentPart::Text {
            text: notices.content,
            meta: None,
        });
    }
    Ok(ExecutableToolResult {
        output: collapse_single_text(parts),
        truncated: (capped.truncated || omitted_attachment).then_some(true),
        spill: (!notices.suffix.is_empty()).then(|| ToolResultSpill {
            suffix: Some(notices.suffix),
            ..Default::default()
        }),
        is_error: result.is_error.then_some(true),
        ..Default::default()
    })
}

async fn preserve_inline_media(
    url: &str,
    options: &McpOutputOptions,
    state: &mut McpOutputState,
) -> Result<Option<String>, WireAsyncError> {
    throw_if_aborted(options.signal.as_ref())?;
    if let Some(reference) = state.preserved_urls.get(url) {
        return Ok(reference.clone());
    }
    let Some((parsed_mime, parsed_base64)) = parse_image_data_url(url) else {
        return Ok(None);
    };
    let mime = if parsed_mime.starts_with("image/") {
        resolve_effective_image_mime(&parsed_mime, &decode_base64_prefix(&parsed_base64))
    } else {
        parsed_mime
    };
    let preserved = preserve_attachment_result(&parsed_base64, &mime, options).await?;
    state.attachment_notices.push(preserved.notice);
    state
        .preserved_urls
        .insert(url.to_string(), preserved.reference.clone());
    Ok(preserved.reference)
}

async fn reference_inline_image(
    part: ContentPart,
    options: &McpOutputOptions,
    preserved_urls: &HashMap<String, Option<String>>,
) -> Result<ContentPart, WireAsyncError> {
    let ContentPart::ImageUrl { image_url } = &part else {
        return Ok(part);
    };
    if options.attachment_store.is_none() {
        return Ok(part);
    }
    let Some((mime_type, base64)) = parse_image_data_url(&image_url.url) else {
        return Ok(part);
    };
    if sniff_media_from_magic(&decode_base64_prefix(&base64))
        .is_none_or(|sniffed| sniffed.kind != FileKind::Image)
    {
        return Ok(part);
    }
    throw_if_aborted(options.signal.as_ref())?;
    if let Some(reference) = preserved_urls.get(&image_url.url) {
        return Ok(match reference {
            Some(reference) => ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: reference.clone(),
                    ..image_url.clone()
                },
            },
            None => part,
        });
    }
    let bytes = match decode_attachment(&base64) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(part),
    };
    match save_attachment(
        &bytes,
        &mime_type,
        options.attachment_store.as_ref().unwrap(),
        options.signal.clone(),
    )
    .await
    {
        Ok(saved) => Ok(ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: saved.reference,
                ..image_url.clone()
            },
        }),
        Err(error) => {
            throw_if_aborted(options.signal.as_ref())?;
            drop(error);
            Ok(part)
        }
    }
}

async fn attachment_details(
    notices: &[String],
    options: &McpOutputOptions,
) -> Result<AttachmentDetails, WireAsyncError> {
    throw_if_aborted(options.signal.as_ref())?;
    let mut seen = HashSet::new();
    let content = notices
        .iter()
        .filter(|notice| seen.insert((*notice).to_string()))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if content.encode_utf16().count() <= MCP_MAX_INLINE_NOTICES_CHARS {
        return Ok(AttachmentDetails {
            suffix: content.clone(),
            content,
        });
    }
    let Some(store) = &options.attachment_store else {
        return Ok(attachment_details_fallback(content));
    };
    match save_attachment(
        content.as_bytes(),
        "text/plain",
        store,
        options.signal.clone(),
    )
    .await
    {
        Ok(saved) => {
            let mut lines = Vec::new();
            if let Some(path) = &saved.path {
                lines.push(format!(
                    "MCP attachment details saved at: {}",
                    serde_json::to_string(path)?
                ));
            }
            lines.push(format!(
                "Attachment details reference: {}",
                serde_json::to_string(&saved.reference)?
            ));
            lines.push(format!(
                "Session-relative attachment details: {}",
                serde_json::to_string(&saved.relative_path)?
            ));
            lines.push("Pass the attachment details reference to Read to retrieve all original attachment references and compression details in the current session.".to_string());
            let pointer = lines.join("\n");
            Ok(AttachmentDetails {
                content: pointer.clone(),
                suffix: pointer,
            })
        }
        Err(error) => {
            throw_if_aborted(options.signal.as_ref())?;
            drop(error);
            Ok(attachment_details_fallback(content))
        }
    }
}

struct AttachmentDetails {
    content: String,
    suffix: String,
}

fn attachment_details_fallback(content: String) -> AttachmentDetails {
    let suffix = "The complete MCP attachment list could not be saved separately. Attachment details are included in the tool output and may be truncated. Do not repeat the MCP call automatically.".to_string();
    AttachmentDetails {
        content: format!("{content}\n{suffix}"),
        suffix,
    }
}

async fn preserve_attachment(
    base64: &str,
    mime_type: &str,
    options: &McpOutputOptions,
) -> Result<String, WireAsyncError> {
    Ok(preserve_attachment_result(base64, mime_type, options)
        .await?
        .notice)
}

struct PreservedAttachment {
    notice: String,
    reference: Option<String>,
}

async fn preserve_attachment_result(
    base64: &str,
    mime_type: &str,
    options: &McpOutputOptions,
) -> Result<PreservedAttachment, WireAsyncError> {
    throw_if_aborted(options.signal.as_ref())?;
    let result = async {
        let store = options.attachment_store.as_ref().ok_or_else(|| {
            Box::new(std::io::Error::other(
                "Session attachment storage is unavailable",
            )) as WireAsyncError
        })?;
        let bytes = decode_attachment(base64)?;
        let saved = save_attachment(&bytes, mime_type, store, options.signal.clone()).await?;
        Ok::<_, WireAsyncError>(PreservedAttachment {
            notice: attachment_notice(&saved, mime_type, bytes.len()),
            reference: Some(saved.reference),
        })
    }
    .await;
    match result {
        Ok(preserved) => Ok(preserved),
        Err(error) => {
            throw_if_aborted(options.signal.as_ref())?;
            Ok(PreservedAttachment {
                notice: format!(
                    "Original attachment could not be saved ({}): {}. No readable original path is available; original attachment preservation is incomplete. Do not repeat the MCP call automatically.",
                    serde_json::to_string(mime_type)?,
                    error
                ),
                reference: None,
            })
        }
    }
}

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

fn decode_attachment(base64: &str) -> Result<Vec<u8>, WireAsyncError> {
    let compact = base64
        .chars()
        .filter(|character| !is_js_whitespace(*character))
        .collect::<String>();
    let bytes = LENIENT_BASE64.decode(&compact).map_err(|_| {
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid base64 attachment",
        )) as WireAsyncError
    })?;
    let canonical = base64::engine::general_purpose::STANDARD.encode(&bytes);
    if canonical != compact && canonical.trim_end_matches('=') != compact {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid base64 attachment",
        )));
    }
    Ok(bytes)
}

#[derive(Debug, Clone)]
struct SavedAttachment {
    path: Option<String>,
    reference: String,
    relative_path: String,
}

fn attachment_notice(saved: &SavedAttachment, mime_type: &str, size: usize) -> String {
    let mut lines = Vec::new();
    if let Some(path) = &saved.path {
        lines.push(format!(
            "Original attachment saved at: {}",
            serde_json::to_string(path).unwrap()
        ));
    }
    lines.push(format!(
        "Attachment reference: {}",
        serde_json::to_string(&saved.reference).unwrap()
    ));
    lines.push(format!(
        "Session-relative attachment: {}",
        serde_json::to_string(&saved.relative_path).unwrap()
    ));
    lines.push(format!(
        "MIME: {}; size: {size} bytes. Pass the attachment reference to Read in the current session. For other binary formats, Read reports the resolved local path for a converter.",
        serde_json::to_string(mime_type).unwrap()
    ));
    lines.join("\n")
}

async fn save_attachment(
    bytes: &[u8],
    mime_type: &str,
    store: &Arc<dyn ISessionMediaStore>,
    signal: Option<AbortSignal>,
) -> Result<SavedAttachment, WireAsyncError> {
    throw_if_aborted(signal.as_ref())?;
    let mime = js_trim(mime_type.split(';').next().unwrap_or("")).to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(&mime);
    hasher.update("\0");
    hasher.update(bytes);
    let hash = format!("{:x}", hasher.finalize());
    let ext = if mime == "application/pdf" {
        ".pdf".to_string()
    } else if mime == "image/svg+xml" {
        if bytes.starts_with(&[0x1f, 0x8b]) {
            ".svgz".to_string()
        } else {
            ".svg".to_string()
        }
    } else {
        text_extension_for_mime(&mime)
            .or_else(|| media_extension_for_mime(&mime).map(str::to_string))
            .unwrap_or_else(|| ".bin".to_string())
    };
    let file_id = format!("f_mcp_{hash}");
    let name = format!("attachment{ext}");
    let size = bytes.len() as u64;
    let bytes = bytes.to_vec();
    let stream = Arc::new(move || {
        let bytes = bytes.clone();
        Box::pin(futures_util::stream::once(async move {
            Ok::<Vec<u8>, WireAsyncError>(bytes)
        })) as BlobByteStream
    });
    let path = store
        .materialize(SessionMediaMaterializeInput {
            file_id: file_id.clone(),
            size,
            name,
            mime_type: mime,
            stream,
            signal: signal.clone(),
        })
        .await?;
    throw_if_aborted(signal.as_ref())?;
    Ok(SavedAttachment {
        path,
        reference: build_daemon_file_url(&file_id),
        relative_path: format!("media/{file_id}{ext}"),
    })
}

fn serialize_structured_extras(extras: &Map<String, Value>) -> Result<String, serde_json::Error> {
    let mut fields = Vec::new();
    for key in ["structuredContent", "_meta"] {
        if let Some(value) = extras.get(key) {
            fields.push(format!(
                "{}:{}",
                serde_json::to_string(key)?,
                serde_json::to_string(value)?
            ));
        }
    }
    Ok(format!("{{{}}}", fields.join(",")))
}

fn json_deep_strict_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => {
            let Some(left) = left.as_f64() else {
                return false;
            };
            let Some(right) = right.as_f64() else {
                return false;
            };
            if left == 0.0 && right == 0.0 {
                left.is_sign_negative() == right.is_sign_negative()
            } else {
                left == right
            }
        }
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_deep_strict_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left_value)| {
                    right
                        .get(key)
                        .is_some_and(|right_value| json_deep_strict_equal(left_value, right_value))
                })
        }
        _ => false,
    }
}

fn parse_comparable_json(text: &str) -> Option<Value> {
    if !json_numbers_are_canonical(text) {
        return None;
    }
    serde_json::from_str(text).ok()
}

fn json_numbers_are_canonical(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;
    while index < bytes.len() {
        let character = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if character == b'\\' {
                escaped = true;
            } else if character == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if character == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if character == b'-' || character.is_ascii_digit() {
            let start = index;
            index += 1;
            while index < bytes.len()
                && matches!(bytes[index], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
            {
                index += 1;
            }
            let Ok(token) = std::str::from_utf8(&bytes[start..index]) else {
                return false;
            };
            let Ok(value) = token.parse::<f64>() else {
                return false;
            };
            if js_number_to_string(value) != token {
                return false;
            }
            continue;
        }
        index += 1;
    }
    true
}

fn js_number_to_string(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let magnitude = value.abs();
    let raw = if (1e-6..1e21).contains(&magnitude) {
        format!("{value}")
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

fn strip_reserved_meta_keys(meta: &Map<String, Value>) -> Option<Map<String, Value>> {
    let out = meta
        .iter()
        .filter(|(key, _)| !is_reserved_meta_key(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<_, _>>();
    (!out.is_empty()).then_some(out)
}

fn is_reserved_meta_key(key: &str) -> bool {
    let Some(slash) = key.find('/') else {
        return false;
    };
    if slash == 0 {
        return false;
    }
    let labels = key[..slash].split('.').collect::<Vec<_>>();
    labels.iter().enumerate().any(|(index, label)| {
        matches!(*label, "modelcontextprotocol" | "mcp") && index + 1 < labels.len()
    })
}

fn wrap_media_only(parts: Vec<ContentPart>, qualified_tool_name: &str) -> Vec<ContentPart> {
    let has_media = parts.iter().any(|part| {
        matches!(
            part,
            ContentPart::ImageUrl { .. }
                | ContentPart::AudioUrl { .. }
                | ContentPart::VideoUrl { .. }
        )
    });
    let has_non_empty_text = parts.iter().any(|part| {
        matches!(part, ContentPart::Text { text, .. } if !text.is_empty())
    });
    if !has_media || has_non_empty_text {
        return parts;
    }
    let mut out = Vec::with_capacity(parts.len() + 2);
    out.push(ContentPart::Text {
        text: format!("<mcp_tool_result name=\"{qualified_tool_name}\">"),
        meta: None,
    });
    out.extend(parts);
    out.push(ContentPart::Text {
        text: "</mcp_tool_result>".to_string(),
        meta: None,
    });
    out
}

struct BinaryPartCap {
    parts: Vec<ContentPart>,
    truncated: bool,
    notices: Vec<String>,
}

async fn apply_binary_part_cap(
    parts: Vec<ContentPart>,
    options: &McpOutputOptions,
    state: &mut McpOutputState,
) -> Result<BinaryPartCap, WireAsyncError> {
    let mut truncated = false;
    let mut out = Vec::new();
    let mut notices = Vec::new();
    for part in parts {
        if matches!(part, ContentPart::Text { .. } | ContentPart::Think { .. }) {
            out.push(part);
            continue;
        }
        let (kind, url) = match &part {
            ContentPart::ImageUrl { image_url } => ("image", &image_url.url),
            ContentPart::AudioUrl { audio_url } => ("audio", &audio_url.url),
            ContentPart::DocumentUrl { document_url } => ("document", &document_url.url),
            ContentPart::VideoUrl { video_url } => ("video", &video_url.url),
            _ => {
                out.push(part);
                continue;
            }
        };
        if url.encode_utf16().count() > MCP_MAX_BINARY_PART_CHARS {
            preserve_inline_media(url, options, state).await?;
            let notice = binary_part_too_large_notice(kind, url.encode_utf16().count());
            out.push(ContentPart::Text {
                text: notice.clone(),
                meta: None,
            });
            notices.push(notice);
            truncated = true;
        } else {
            out.push(part);
        }
    }
    Ok(BinaryPartCap {
        parts: out,
        truncated,
        notices,
    })
}

fn collapse_single_text(parts: Vec<ContentPart>) -> ExecutableToolOutput {
    if parts.len() == 1 {
        if let ContentPart::Text { text, .. } = &parts[0] {
            return ExecutableToolOutput::Text(text.clone());
        }
    }
    ExecutableToolOutput::Parts(parts)
}

async fn persist_original_for_output(
    bytes: &[u8],
    mime_type: &str,
    originals: Option<&McpOriginalsTarget>,
) -> Option<String> {
    let Some(originals) = originals else {
        return crate::agent::media::image_originals::persist_original_image(
            bytes,
            mime_type,
            &crate::agent::media::image_originals::PersistOriginalImageOptions::default(),
        )
        .map(|path| path.to_string_lossy().into_owned());
    };
    persist_original_with_target(bytes, mime_type, originals)
        .await
        .ok()
        .flatten()
}

async fn persist_original_with_target(
    bytes: &[u8],
    mime_type: &str,
    target: &McpOriginalsTarget,
) -> Result<Option<String>, WireAsyncError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let path_class = originals_path_class(target.path.as_ref());
    let hash = format!("{:x}", Sha256::digest(bytes));
    let extension = match js_trim(mime_type).to_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/tiff" => "tif",
        _ => "img",
    };
    let file_name = format!("{}.{extension}", &hash[..32]);
    let file_path = path_class.join(&target.dir, &[&file_name]);
    target.fs.mkdir(&target.dir, true).await?;
    let replace = target
        .fs
        .stat(&file_path)
        .await
        .map(|stat| stat.size != bytes.len() as u64)
        .unwrap_or(true);
    if replace {
        target.fs.write_bytes(&file_path, bytes).await?;
    }
    sweep_originals(target, &path_class).await?;
    match target.fs.stat(&file_path).await {
        Ok(_) => Ok(Some(file_path)),
        Err(_) => Ok(None),
    }
}

fn originals_path_class(path: Option<&EnvironmentPath>) -> EnvironmentPath {
    path.cloned().unwrap_or_else(|| {
        EnvironmentPath::new(if cfg!(windows) { "win32" } else { "posix" })
    })
}

async fn sweep_originals(
    target: &McpOriginalsTarget,
    path_class: &EnvironmentPath,
) -> Result<(), WireAsyncError> {
    let names = target.fs.readdir(&target.dir).await?;
    let mut entries = Vec::new();
    for entry in names {
        if !entry.is_file {
            continue;
        }
        let path = path_class.join(&target.dir, &[&entry.name]);
        let Ok(info) = target.fs.stat(&path).await else {
            continue;
        };
        if !info.is_file {
            continue;
        }
        entries.push((path, info.size, info.mtime_ms.unwrap_or(0.0)));
    }
    let mut total = entries.iter().map(|(_, size, _)| *size).sum::<u64>();
    if total <= DEFAULT_MAX_ORIGINALS_TOTAL_BYTES {
        return Ok(());
    }
    entries.sort_by(|left, right| left.2.total_cmp(&right.2));
    for (path, size, _) in entries {
        if total <= DEFAULT_MAX_ORIGINALS_TOTAL_BYTES {
            break;
        }
        let _ = target.fs.remove(&path).await;
        total = total.saturating_sub(size);
    }
    Ok(())
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), WireAsyncError> {
    if let Some(signal) = signal.filter(|signal| signal.aborted()) {
        let reason = signal
            .reason()
            .unwrap_or_else(|| std::sync::Arc::new(abort_error()));
        Err(Box::new(reason) as WireAsyncError)
    } else {
        Ok(())
    }
}
