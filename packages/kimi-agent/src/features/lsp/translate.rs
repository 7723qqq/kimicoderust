use serde_json::Value;

use super::protocol::{LspHover, LspLocation};

pub fn path_to_uri(path: &str) -> String {
    let path = std::path::Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap().join(path)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    url::Url::from_file_path(&normalized)
        .unwrap_or_else(|_| panic!("cannot convert path to file URL: {}", normalized.display()))
        .to_string()
}

pub fn normalize_locations_response(response: &Value) -> Result<Vec<LspLocation>, String> {
    if response.is_null() {
        return Ok(Vec::new());
    }
    if let Some(entries) = response.as_array() {
        return entries.iter().map(normalize_location_entry).collect();
    }
    Ok(vec![normalize_location_entry(response)?])
}

fn normalize_location_entry(entry: &Value) -> Result<LspLocation, String> {
    if entry.get("targetUri").is_some_and(Value::is_string)
        && entry.get("targetRange").is_some_and(Value::is_object)
    {
        return Ok(LspLocation {
            uri: entry["targetUri"].as_str().unwrap().to_string(),
            range: serde_json::from_value(entry["targetRange"].clone())
                .map_err(|error| error.to_string())?,
        });
    }
    if entry.get("uri").is_some_and(Value::is_string)
        && entry.get("range").is_some_and(Value::is_object)
    {
        return serde_json::from_value(entry.clone()).map_err(|error| error.to_string());
    }
    Err(format!("malformed LSP location response: {entry}"))
}

pub fn normalize_hover_response(response: &Value) -> Result<Option<LspHover>, String> {
    if response.is_null() {
        return Ok(None);
    }
    if !response.is_object() {
        return Err(format!("malformed LSP hover response: {response}"));
    }
    serde_json::from_value(response.clone())
        .map(Some)
        .map_err(|error| error.to_string())
}

pub fn uri_to_path(uri: &str) -> String {
    if !uri.starts_with("file://") {
        return uri.to_string();
    }
    url::Url::parse(uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| uri.to_string())
}
