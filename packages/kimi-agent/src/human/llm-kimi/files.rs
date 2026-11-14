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
        Ok(value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string))
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
