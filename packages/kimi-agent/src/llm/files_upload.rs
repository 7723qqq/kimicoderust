use crate::rpc::types::BoxFuture;
pub use kimi_core::types::{CredentialFuture, UploadError};

pub fn files_base_url(base_url: &str, protocol: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if protocol != "anthropic" || base.ends_with("/v1") {
        return base.to_string();
    }
    format!("{base}/v1")
}

pub fn multipart_body(
    boundary: &str,
    filename: &str,
    media_type: &str,
    purpose: &str,
    bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 512);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: {media_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"purpose\"\r\n\r\n");
    body.extend_from_slice(purpose.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

pub fn upload_filename(media_type: &str) -> String {
    let extension = match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "video/mp4" => "mp4",
        "video/mpeg" => "mpeg",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/x-msvideo" => "avi",
        _ => "bin",
    };
    format!("upload.{extension}")
}

const READY_FILE_OBJECT_STATUSES: [&str; 3] = ["ready", "ok", "processed"];

fn read_non_empty_string(value: Option<&serde_json::Value>) -> Option<String> {
    let text = value?.as_str()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn read_upload_failure_detail(
    record: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
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

fn js_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                value.to_string()
            } else if let Some(value) = value.as_u64() {
                value.to_string()
            } else {
                value.as_f64().unwrap_or_default().to_string()
            }
        }
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| match value {
                serde_json::Value::Null => String::new(),
                value => js_string(value),
            })
            .collect::<Vec<_>>()
            .join(","),
        serde_json::Value::Object(_) => "[object Object]".to_string(),
    }
}

fn upload_status_error(
    id: &str,
    field: &str,
    value: &serde_json::Value,
    detail: Option<String>,
) -> UploadError {
    let rendered = js_string(value);
    let summary = if value.as_str() == Some("error") {
        format!("Kimi file upload failed for file \"{id}\" ({field}={rendered})")
    } else {
        format!("Kimi file upload for file \"{id}\" is not ready ({field}={rendered})")
    };
    UploadError::Other(match detail {
        Some(detail) => format!("{summary}: {detail}"),
        None => summary,
    })
}

fn parse_uploaded_file(text: &str) -> Result<String, UploadError> {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(_) => {
            return Err(UploadError::Other(format!(
                "media upload returned no file id: {text}"
            )));
        }
    };
    let Some(record) = value.as_object() else {
        return Err(UploadError::Other(
            "Kimi file upload returned an invalid response payload".to_string(),
        ));
    };
    let Some(id) = record
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)
    else {
        return Err(UploadError::Other(
            "Kimi file upload returned an empty file id".to_string(),
        ));
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
    let allow_uploaded_with_ready_extract = extract_status.and_then(serde_json::Value::as_str)
        == Some("ready")
        && status.and_then(serde_json::Value::as_str) == Some("uploaded");
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

fn classify_status(status: u16, body: &str) -> UploadError {
    let message = format!("media upload failed with status {status}: {body}");
    match status {
        401 | 403 => UploadError::Auth(message),
        404 | 405 | 501 => UploadError::Unsupported(message),
        _ => UploadError::Other(message),
    }
}

pub async fn upload_media(
    base_url: &str,
    protocol: &str,
    credential: &str,
    purpose: &str,
    filename: &str,
    media_type: &str,
    bytes: &[u8],
) -> Result<String, UploadError> {
    let boundary = format!("----kimi{:016x}", fastrand::u64(..));
    let body = multipart_body(&boundary, filename, media_type, purpose, bytes);
    let url = format!("{}/files", files_base_url(base_url, protocol));
    let mut request = crate::llm::http::SHARED_HTTP_CLIENT
        .post(&url)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body);

    request = if protocol == "anthropic" {
        request.header("x-api-key", credential)
    } else {
        request.bearer_auth(credential)
    };
    let response = request
        .send()
        .await
        .map_err(|error| UploadError::Other(format!("media upload failed: {error}")))?;
    let status = response.status().as_u16();
    let text = response.text().await;
    if !(200..300).contains(&status) {
        let body = match &text {
            Ok(text) => text.clone(),
            Err(error) => format!("<body could not be read: {error}>"),
        };
        return Err(classify_status(status, &body));
    }
    let text = text.map_err(|error| {
        UploadError::Other(format!(
            "media upload response body could not be read: {error}"
        ))
    })?;
    parse_uploaded_file(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_anthropic_route_gets_its_version_segment_back() {
        assert_eq!(
            files_base_url("https://api.example.test", "anthropic"),
            "https://api.example.test/v1"
        );
        assert_eq!(
            files_base_url("https://api.example.test/v1", "anthropic"),
            "https://api.example.test/v1"
        );
        assert_eq!(
            files_base_url("https://api.example.test/v1/", "anthropic"),
            "https://api.example.test/v1"
        );

        assert_eq!(
            files_base_url("https://api.example.test/v1", "openai"),
            "https://api.example.test/v1"
        );
    }

    #[test]
    fn test_the_multipart_body_carries_the_file_and_the_purpose() {
        let body = multipart_body("BOUND", "a.png", "image/png", "image", b"PNG");
        let text = String::from_utf8_lossy(&body);

        assert!(text.starts_with("--BOUND\r\n"), "{text}");
        assert!(
            text.contains("Content-Disposition: form-data; name=\"file\"; filename=\"a.png\""),
            "{text}"
        );
        assert!(text.contains("Content-Type: image/png"), "{text}");
        assert!(text.contains("PNG"), "{text}");
        assert!(
            text.contains("Content-Disposition: form-data; name=\"purpose\"\r\n\r\nimage\r\n"),
            "{text}"
        );
        assert!(text.ends_with("--BOUND--\r\n"), "{text}");
    }

    #[test]
    fn test_a_binary_payload_survives_the_body() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        let body = multipart_body("B", "a.bin", "application/octet-stream", "image", &bytes);

        let at = body
            .windows(bytes.len())
            .position(|window| window == bytes.as_slice())
            .expect("the payload is embedded verbatim");
        assert!(at > 0);
    }

    #[test]
    fn test_the_filename_follows_the_media_type() {
        assert_eq!(upload_filename("image/png"), "upload.png");
        assert_eq!(upload_filename("video/quicktime"), "upload.mov");
        assert_eq!(upload_filename("application/pdf"), "upload.bin");
    }

    #[test]
    fn test_the_response_file_id_is_read() {
        assert_eq!(
            parse_uploaded_file(r#"{"id":"file-abc","object":"file"}"#).unwrap(),
            "file-abc"
        );
        assert!(parse_uploaded_file(r#"{"id":""}"#).is_err());
        assert!(parse_uploaded_file("not json").is_err());
        assert_eq!(
            parse_uploaded_file(r#"{"id":"file-abc","status":"ready"}"#).unwrap(),
            "file-abc"
        );
        assert!(parse_uploaded_file(r#"{"id":"file-abc","status":"uploading"}"#).is_err());
        assert!(
            parse_uploaded_file(r#"{"id":"file-abc","extract_status":"error"}"#)
                .unwrap_err()
                .message()
                .contains("extract_status=error")
        );
        assert_eq!(
            parse_uploaded_file(
                r#"{"id":"file-abc","extract_status":"ready","status":"uploaded"}"#
            )
            .unwrap(),
            "file-abc"
        );
    }

    #[test]
    fn test_a_rejected_credential_is_not_a_fallback() {
        assert!(matches!(classify_status(401, "nope"), UploadError::Auth(_)));
        assert!(matches!(classify_status(403, "nope"), UploadError::Auth(_)));
        assert!(matches!(
            classify_status(404, "no such route"),
            UploadError::Unsupported(_)
        ));
        assert!(matches!(
            classify_status(500, "boom"),
            UploadError::Other(_)
        ));
    }
}
