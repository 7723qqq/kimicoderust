use std::path::{Path, PathBuf};

use kimi_core::primitives::errors::Error2;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::wire::record::WireAsyncError;

use super::errors::PLUGIN_LOAD_FAILED;
use super::types::{PluginCapabilityState, PluginGithubMetadata, PluginOrigin, PluginSource};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledRecord {
    pub id: String,
    pub root: String,
    pub source: PluginSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PluginOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marketplace_source: Option<String>,
    pub enabled: bool,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<PluginCapabilityState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<PluginGithubMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledFile {
    pub version: u8,
    pub plugins: Vec<InstalledRecord>,
}

impl Default for InstalledFile {
    fn default() -> Self {
        Self {
            version: 1,
            plugins: Vec::new(),
        }
    }
}

pub async fn read_installed(kimi_home_dir: &str) -> Result<InstalledFile, WireAsyncError> {
    let file_path = installed_path(kimi_home_dir);
    let text = match tokio::fs::read_to_string(&file_path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(InstalledFile::default()),
        Err(error) => return Err(Box::new(error)),
    };
    let parsed: Value = serde_json::from_str(&text).map_err(|error| {
        plugin_store_parse_error(&file_path, error.to_string(), Some(Box::new(error)))
    })?;
    if !parsed.is_object()
        || !parsed
            .get("plugins")
            .is_some_and(|plugins| plugins.is_array())
    {
        return Err(plugin_store_parse_error(
            &file_path,
            "installed.json is not a valid InstalledFile object".to_string(),
            None,
        ));
    }
    serde_json::from_value(parsed).map_err(|error| {
        plugin_store_parse_error(&file_path, error.to_string(), Some(Box::new(error)))
    })
}

pub async fn write_installed(kimi_home_dir: &str, data: &InstalledFile) -> Result<(), WireAsyncError> {
    let dir = Path::new(kimi_home_dir).join("plugins");
    tokio::fs::create_dir_all(&dir).await?;
    let final_path = dir.join("installed.json");
    let temporary = dir.join("installed.json.tmp");
    let text = serde_json::to_string_pretty(data)?;
    tokio::fs::write(&temporary, text).await?;
    tokio::fs::rename(&temporary, &final_path).await?;
    Ok(())
}

fn installed_path(kimi_home_dir: &str) -> PathBuf {
    Path::new(kimi_home_dir).join("plugins").join("installed.json")
}

fn plugin_store_parse_error(
    file_path: &Path,
    message: String,
    cause: Option<WireAsyncError>,
) -> WireAsyncError {
    let rendered_path = file_path.to_string_lossy().into_owned();
    let mut details = Map::new();
    details.insert("path".to_string(), Value::String(rendered_path.clone()));
    let error = Error2::new(
        PLUGIN_LOAD_FAILED,
        format!("Failed to parse {rendered_path}: {message}"),
    )
    .with_details(details);
    let error = match cause {
        Some(cause) => error.with_cause(cause),
        None => error,
    };
    Box::new(error)
}
