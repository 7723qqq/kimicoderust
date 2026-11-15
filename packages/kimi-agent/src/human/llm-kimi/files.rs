use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::human::llm::media::upload::{ImageUploadInput, VideoUploadInput};
use crate::human::llm::message::{ContentPart, ImageUrl, VideoUrl};
use crate::human::llm::model::LlmModel;
use crate::human::llm_kimi::r#trait::KIMI_DEFAULT_BASE_URL;
use crate::wire::record::WireAsyncError;

pub fn kimi_files_base_url(model: &LlmModel) -> String {
    let base = model
        .connection
        .base_url
        .clone()
        .unwrap_or_else(|| KIMI_DEFAULT_BASE_URL.to_string());
    if model.provider != "anthropic" || base.ends_with("/v1") || base.ends_with("/v1/") {
        return base;
    }
    format!("{}/v1", base.strip_suffix('/').unwrap_or(&base))
}

#[derive(Clone, Default)]
pub struct KimiUploadOptions {
    pub signal: Option<CancellationToken>,
}

pub struct KimiFilesFetchRequest {
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub signal: Option<CancellationToken>,
}

pub struct KimiFilesFetchResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub type KimiFilesFetch = Arc<
    dyn Fn(KimiFilesFetchRequest) -> BoxFuture<'static, Result<KimiFilesFetchResponse, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub struct KimiFilesOptions {
    pub api_key: Option<String>,
    pub base_url: String,
    pub default_headers: Option<HashMap<String, String>>,
    pub fetch: Option<KimiFilesFetch>,
}

#[derive(Debug)]
struct KimiFilesError(String);

impl Display for KimiFilesError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KimiFilesError {}

#[derive(Debug)]
struct KimiFilesApiError {
    status: u16,
    body: String,
}

impl Display for KimiFilesApiError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "KimiFiles upload failed with status {}: {}", self.status, self.body)
    }
}

impl std::error::Error for KimiFilesApiError {}

const READY_FILE_OBJECT_STATUSES: [&str; 3] = ["ready", "ok", "processed"];

fn read_non_empty_string(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn read_upload_failure_detail(record: &serde_json::Map<String, Value>) -> Option<String> {
    if let Some(detail) = read_non_empty_string(record.get("status_details")) {
        return Some(detail);
    }
    if let Some(extract_error) = record.get("extract_error") {
        if let Some(detail) = read_non_empty_string(Some(extract_error)) {
            return Some(detail);
        }
        if let Ok(serialized) = serde_json::to_string(extract_error) {
            return Some(serialized);
        }
    }
    if let Some(raw_error) = record.get("error") {
        if let Some(detail) = read_non_empty_string(Some(raw_error)) {
            return Some(detail);
        }
        if let Some(nested) = raw_error
            .as_object()
            .and_then(|error| read_non_empty_string(error.get("message")))
        {
            return Some(nested);
        }
    }
    read_non_empty_string(record.get("message"))
}

fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                value.to_string()
            } else if let Some(value) = value.as_u64() {
                value.to_string()
            } else {
                value.as_f64().unwrap_or_default().to_string()
            }
        }
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(|value| match value {
                Value::Null => String::new(),
                value => js_string(value),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

fn upload_status_error(
    id: &str,
    field: &str,
    value: &Value,
    detail: Option<String>,
) -> WireAsyncError {
    let rendered = js_string(value);
    let summary = if value.as_str() == Some("error") {
        format!("Kimi file upload failed for file \"{id}\" ({field}={rendered})")
    } else {
        format!("Kimi file upload for file \"{id}\" is not ready ({field}={rendered})")
    };
    let message = match detail {
        Some(detail) => format!("{summary}: {detail}"),
        None => summary,
    };
    KimiFilesError(message).into()
}

fn parse_uploaded_kimi_file(value: &Value) -> Result<String, WireAsyncError> {
    let Some(record) = value.as_object() else {
        return Err(KimiFilesError(
            "Kimi file upload returned an invalid response payload".to_string(),
        )
        .into());
    };
    let Some(id) = record
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)
    else {
        return Err(
            KimiFilesError("Kimi file upload returned an empty file id".to_string()).into(),
        );
    };
    let extract_status = record
        .get("extract_status")
        .filter(|value| !value.is_null());
    if let Some(extract_status) = extract_status
        && extract_status.as_str() != Some("ready")
    {
        return Err(upload_status_error(
            &id,
            "extract_status",
            extract_status,
            read_upload_failure_detail(record),
        ));
    }
    let status = record.get("status").filter(|value| !value.is_null());
    let allow_uploaded_with_ready_extract = extract_status.and_then(Value::as_str) == Some("ready")
        && status.and_then(Value::as_str) == Some("uploaded");
    if let Some(status) = status
        && !allow_uploaded_with_ready_extract
        && status
            .as_str()
            .is_none_or(|status| !READY_FILE_OBJECT_STATUSES.contains(&status))
    {
        return Err(upload_status_error(
            &id,
            "status",
            status,
            read_upload_failure_detail(record),
        ));
    }
    Ok(id)
}

pub struct KimiFiles {
    options: KimiFilesOptions,
    client: Option<reqwest::Client>,
}

impl KimiFiles {
    pub fn new(options: KimiFilesOptions) -> Self {
        let client = options
            .api_key
            .as_ref()
            .filter(|api_key| !api_key.is_empty())
            .map(|_| reqwest::Client::new());
        Self { options, client }
    }

    pub async fn upload_video(
        &self,
        input: VideoUploadInput,
        options: Option<KimiUploadOptions>,
    ) -> Result<ContentPart, WireAsyncError> {
        if !input.mime_type.starts_with("video/") {
            return Err(KimiFilesError(format!(
                "Expected a video mime type, got {}",
                input.mime_type
            ))
            .into());
        }
        let uploaded = self
            .upload(
                input.data,
                &input.mime_type,
                input.filename,
                "video",
                options,
            )
            .await?;
        Ok(ContentPart::VideoUrl {
            video_url: VideoUrl {
                url: format!("ms://{}", uploaded.as_deref().unwrap_or("undefined")),
                id: uploaded,
                name: None,
            },
        })
    }

    pub async fn upload_image(
        &self,
        input: ImageUploadInput,
        options: Option<KimiUploadOptions>,
    ) -> Result<ContentPart, WireAsyncError> {
        if !input.mime_type.starts_with("image/") {
            return Err(KimiFilesError(format!(
                "Expected an image mime type, got {}",
                input.mime_type
            ))
            .into());
        }
        let uploaded = self
            .upload(
                input.data,
                &input.mime_type,
                input.filename,
                "image",
                options,
            )
            .await?;
        Ok(ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: format!("ms://{}", uploaded.as_deref().unwrap_or("undefined")),
                id: uploaded,
                name: None,
                dimensions: None,
                detail: None,
            },
        })
    }

    async fn upload(
        &self,
        data: Vec<u8>,
        mime_type: &str,
        filename: Option<String>,
        purpose: &str,
        options: Option<KimiUploadOptions>,
    ) -> Result<Option<String>, WireAsyncError> {
        let Some(api_key) = self
            .options
            .api_key
            .as_ref()
            .filter(|api_key| !api_key.is_empty())
        else {
            return Err(KimiFilesError("KimiFiles: apiKey is required".to_string()).into());
        };
        let filename = filename.unwrap_or_else(|| guess_filename(mime_type));
        let boundary = format!("----kimi{:016x}", fastrand::u64(..));
        let mut body = Vec::with_capacity(data.len() + 512);
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {mime_type}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&data);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"purpose\"\r\n\r\n");
        body.extend_from_slice(purpose.as_bytes());
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let url = format!("{}/files", self.options.base_url.trim_end_matches('/'));
        let signal = options.and_then(|options| options.signal);
        let response = if let Some(fetch) = &self.options.fetch {
            let mut headers = self.options.default_headers.clone().unwrap_or_default();
            headers.insert("authorization".to_string(), format!("Bearer {api_key}"));
            headers.insert(
                "content-type".to_string(),
                format!("multipart/form-data; boundary={boundary}"),
            );
            fetch(KimiFilesFetchRequest {
                url,
                headers,
                body,
                signal,
            })
            .await?
        } else {
            let mut request = self.client.as_ref().unwrap().post(&url);
            if let Some(headers) = &self.options.default_headers {
                for (name, value) in headers {
                    request = request.header(name, value);
                }
            }
            request = request
                .bearer_auth(api_key)
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(body);
            let response = match signal {
                Some(signal) => tokio::select! {
                    _ = signal.cancelled() => {
                        return Err(KimiFilesError("upload aborted".to_string()).into());
                    }
                    response = request.send() => response?,
                },
                None => request.send().await?,
            };
            KimiFilesFetchResponse {
                status: response.status().as_u16(),
                body: response.bytes().await?.to_vec(),
            }
        };
        if !(200..300).contains(&response.status) {
            return Err(KimiFilesApiError {
                status: response.status,
                body: String::from_utf8_lossy(&response.body).into_owned(),
            }
            .into());
        }
        let value: Value = serde_json::from_slice(&response.body)?;
        Ok(Some(parse_uploaded_kimi_file(&value)?))
    }
}

fn guess_filename(mime_type: &str) -> String {
    let extension = match mime_type.to_lowercase().as_str() {
        "video/mp4" => "mp4",
        "video/mpeg" => "mpeg",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/x-msvideo" => "avi",
        "video/x-flv" => "flv",
        "video/3gpp" => "3gp",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/heic" => "heic",
        "image/heif" => "heif",
        _ => "bin",
    };
    format!("upload.{extension}")
}
