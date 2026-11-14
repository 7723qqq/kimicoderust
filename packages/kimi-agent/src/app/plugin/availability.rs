use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::wire::record::WireAsyncError;

use super::marketplace::{
    MarketplaceHostEntry, PluginClient, PluginMarketplace, PluginMarketplaceEntry,
    PluginMarketplacePlatform, marketplace_entry_installable_on_host,
};
use super::types::{PluginOrigin, PluginRecord};

const LOCK_RETRY_MS: u64 = 50;
const LOCK_TIMEOUT_MS: u64 = 10_000;
const LOCK_STALE_MS: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginAvailabilityEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients: Option<Vec<PluginClient>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<PluginMarketplacePlatform>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_source_key: Option<String>,
    pub checked_at: String,
}

impl MarketplaceHostEntry for PluginAvailabilityEntry {
    fn host_clients(&self) -> Option<&[PluginClient]> {
        self.clients.as_deref()
    }

    fn host_platforms(&self) -> Option<&[PluginMarketplacePlatform]> {
        self.platforms.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginAvailabilityFile {
    pub version: u8,
    pub sources: IndexMap<String, IndexMap<String, PluginAvailabilityEntry>>,
}

impl Default for PluginAvailabilityFile {
    fn default() -> Self {
        Self {
            version: 1,
            sources: IndexMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginAvailabilityMatch {
    pub marketplace_source: String,
    pub catalog_id: String,
}

pub fn plugin_availability_source_key(source: &str) -> String {
    let normalized = source.trim();
    if normalized.len() == 71
        && normalized.starts_with("sha256:")
        && normalized[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return normalized.to_string();
    }
    format!("sha256:{}", hex_digest(&Sha256::digest(normalized.as_bytes())))
}

fn availability_value(entry: &PluginAvailabilityEntry) -> String {
    let mut value = Map::new();
    if let Some(clients) = &entry.clients {
        let mut clients = clients.iter().map(|client| client.as_str()).collect::<Vec<_>>();
        clients.sort_unstable();
        value.insert(
            "clients".to_string(),
            Value::Array(clients.into_iter().map(Value::from).collect()),
        );
    }
    if let Some(platforms) = &entry.platforms {
        let mut platforms = platforms
            .iter()
            .map(|platform| platform.as_str())
            .collect::<Vec<_>>();
        platforms.sort_unstable();
        value.insert(
            "platforms".to_string(),
            Value::Array(platforms.into_iter().map(Value::from).collect()),
        );
    }
    Value::Object(value).to_string()
}

fn availability_file_value(file: &PluginAvailabilityFile) -> String {
    let mut sources = file.sources.iter().collect::<Vec<_>>();
    sources.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut output = Map::new();
    for (source, entries) in sources {
        let mut serialized_entries = Map::new();
        for (id, entry) in entries {
            let mut value = Map::new();
            value.insert("value".to_string(), Value::String(availability_value(entry)));
            if let Some(artifact_source_key) = &entry.artifact_source_key {
                value.insert(
                    "artifactSourceKey".to_string(),
                    Value::String(artifact_source_key.clone()),
                );
            }
            serialized_entries.insert(id.clone(), Value::Object(value));
        }
        output.insert(source.clone(), Value::Object(serialized_entries));
    }
    Value::Object(output).to_string()
}

fn catalog_entries(
    marketplace: &PluginMarketplace,
    checked_at: &str,
) -> IndexMap<String, PluginAvailabilityEntry> {
    marketplace
        .plugins
        .iter()
        .map(|entry| {
            (
                entry.id.clone(),
                PluginAvailabilityEntry {
                    clients: entry.clients.clone(),
                    platforms: entry.platforms.clone(),
                    artifact_source_key: Some(plugin_availability_source_key(&entry.source)),
                    checked_at: checked_at.to_string(),
                },
            )
        })
        .collect()
}

fn availability_object(value: &Value) -> Result<&Map<String, Value>, WireAsyncError> {
    value
        .as_object()
        .ok_or_else(|| "Invalid plugin availability cache".into())
}

fn availability_strings(value: Option<&Value>) -> Result<Option<Vec<String>>, WireAsyncError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(values) = value.as_array() else {
        return Err("Invalid plugin availability cache".into());
    };
    values
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "Invalid plugin availability cache".into())
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn availability_string(value: Option<&Value>) -> Result<Option<String>, WireAsyncError> {
    match value {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(|value| Some(value.to_string()))
            .ok_or_else(|| "Invalid plugin availability cache".into()),
    }
}

fn migrate_availability_entry(value: &Value) -> Result<PluginAvailabilityEntry, WireAsyncError> {
    let entry = availability_object(value)?;
    let checked_at = availability_string(entry.get("checkedAt"))?
        .ok_or_else(|| "Invalid plugin availability cache")?;
    let artifact_source = availability_string(entry.get("artifactSource"))?;
    let clients = availability_strings(entry.get("clients"))?
        .map(|clients| {
            clients
                .into_iter()
                .map(|client| match client.as_str() {
                    "desktop" => Ok(PluginClient::Desktop),
                    "cli" => Ok(PluginClient::Cli),
                    _ => Err("Invalid plugin availability cache".into()),
                })
                .collect::<Result<Vec<_>, WireAsyncError>>()
        })
        .transpose()?;
    let platforms = availability_strings(entry.get("platforms"))?
        .map(|platforms| {
            platforms
                .into_iter()
                .map(|platform| match platform.as_str() {
                    "darwin" | "macos" => Ok(PluginMarketplacePlatform::Darwin),
                    "win32" | "windows" => Ok(PluginMarketplacePlatform::Win32),
                    "linux" => Ok(PluginMarketplacePlatform::Linux),
                    "cloud" => Ok(PluginMarketplacePlatform::Cloud),
                    _ => Err("Invalid plugin availability cache".into()),
                })
                .collect::<Result<Vec<_>, WireAsyncError>>()
        })
        .transpose()?;
    Ok(PluginAvailabilityEntry {
        clients,
        platforms,
        artifact_source_key: availability_string(entry.get("artifactSourceKey"))?
            .or_else(|| artifact_source.map(|source| plugin_availability_source_key(&source))),
        checked_at,
    })
}

async fn read_availability(file_path: &Path) -> Result<PluginAvailabilityFile, WireAsyncError> {
    let text = match tokio::fs::read_to_string(file_path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PluginAvailabilityFile::default());
        }
        Err(error) => return Err(Box::new(error)),
    };
    let parsed: Value = serde_json::from_str(&text)?;
    let parsed = availability_object(&parsed)?;
    if parsed.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("Invalid plugin availability cache".into());
    }
    let sources = availability_object(
        parsed
            .get("sources")
            .ok_or_else(|| "Invalid plugin availability cache")?,
    )?;
    let mut output = IndexMap::new();
    for (source, value) in sources {
        let mut entries = IndexMap::new();
        for (id, entry) in availability_object(value)? {
            entries.insert(id.clone(), migrate_availability_entry(entry)?);
        }
        output.insert(plugin_availability_source_key(source), entries);
    }
    Ok(PluginAvailabilityFile {
        version: 1,
        sources: output,
    })
}

async fn with_file_lock<T, F, Fut>(lock_path: &Path, run: F) -> Result<T, WireAsyncError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, WireAsyncError>>,
{
    let deadline = Instant::now() + Duration::from_millis(LOCK_TIMEOUT_MS);
    loop {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
            .await
        {
            Ok(handle) => {
                drop(handle);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let age = match tokio::fs::metadata(lock_path).await {
                    Ok(metadata) => metadata
                        .modified()
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .map(|elapsed| elapsed.as_millis())
                        .unwrap_or(0),
                    Err(_) => 0,
                };
                if age > LOCK_STALE_MS as u128 {
                    let _ = tokio::fs::remove_file(lock_path).await;
                    continue;
                }
                if Instant::now() > deadline {
                    return Err(format!(
                        "Timed out waiting for the plugin availability lock: {}",
                        lock_path.to_string_lossy()
                    )
                    .into());
                }
                tokio::time::sleep(Duration::from_millis(LOCK_RETRY_MS)).await;
            }
            Err(error) => return Err(Box::new(error)),
        }
    }
    let result = run().await;
    let _ = tokio::fs::remove_file(lock_path).await;
    result
}

pub struct PluginAvailabilityStore {
    file_path: PathBuf,
    current: RwLock<PluginAvailabilityFile>,
    generation_value: AtomicU64,
}

impl PluginAvailabilityStore {
    pub fn new(kimi_home_dir: &str) -> Self {
        Self {
            file_path: Path::new(kimi_home_dir)
                .join("plugins")
                .join("availability.json"),
            current: RwLock::new(PluginAvailabilityFile::default()),
            generation_value: AtomicU64::new(0),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation_value.load(Ordering::Acquire)
    }

    pub async fn refresh(&self) -> Result<(), WireAsyncError> {
        let next = read_availability(&self.file_path).await?;
        self.replace_current(next);
        Ok(())
    }

    pub async fn update(&self, marketplace: &PluginMarketplace) -> Result<bool, WireAsyncError> {
        let source = plugin_availability_source_key(&marketplace.source);
        let checked_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let incoming = catalog_entries(marketplace, &checked_at);
        if let Some(parent) = self.file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let lock_path = PathBuf::from(format!("{}.lock", self.file_path.to_string_lossy()));
        with_file_lock(&lock_path, || async {
            let latest = read_availability(&self.file_path).await?;
            let existing = latest.sources.get(&source).cloned().unwrap_or_default();
            let changed = incoming.iter().any(|(id, entry)| {
                existing
                    .get(id)
                    .is_none_or(|current| availability_value(current) != availability_value(entry))
            });
            if !changed {
                self.replace_current(latest);
                return Ok(false);
            }
            let mut next = latest.clone();
            let mut merged = latest.sources.get(&source).cloned().unwrap_or_default();
            for (id, entry) in incoming {
                merged.insert(id, entry);
            }
            next.sources.insert(source.clone(), merged);
            let temporary = PathBuf::from(format!(
                "{}.{}.tmp",
                self.file_path.to_string_lossy(),
                Uuid::new_v4()
            ));
            let write_result = async {
                let text = serde_json::to_string_pretty(&next)?;
                tokio::fs::write(&temporary, text).await?;
                tokio::fs::rename(&temporary, &self.file_path).await?;
                Ok::<(), WireAsyncError>(())
            }
            .await;
            let _ = tokio::fs::remove_file(&temporary).await;
            write_result?;
            self.replace_current(next);
            Ok(true)
        })
        .await
    }

    pub fn entry_for(&self, record: &PluginRecord) -> Option<PluginAvailabilityEntry> {
        if record.origin == PluginOrigin::Custom {
            return None;
        }
        let catalog_id = record.catalog_id.clone().or_else(|| {
            (record.origin == PluginOrigin::Unknown).then(|| record.id.clone())
        })?;
        let original_source_key = record
            .original_source
            .as_deref()
            .map(plugin_availability_source_key);
        let current = self.current.read();
        if let Some(marketplace_source) = &record.marketplace_source {
            let entry = current
                .sources
                .get(&plugin_availability_source_key(marketplace_source))
                .and_then(|entries| entries.get(&catalog_id))
                .cloned();
            if record.origin == PluginOrigin::Unknown
                && entry.as_ref().is_none_or(|entry| {
                    entry.artifact_source_key.is_none()
                        || entry.artifact_source_key != original_source_key
                })
            {
                return None;
            }
            return entry;
        }
        let matches = current
            .sources
            .values()
            .filter_map(|entries| entries.get(&catalog_id))
            .cloned()
            .collect::<Vec<_>>();
        if record.origin == PluginOrigin::Unknown {
            let provenance_matches = matches
                .iter()
                .filter(|entry| {
                    entry.artifact_source_key.is_some()
                        && entry.artifact_source_key == original_source_key
                })
                .cloned()
                .collect::<Vec<_>>();
            return match provenance_matches.len() {
                0 => None,
                1 => provenance_matches.into_iter().next(),
                _ => Some(unavailable_entry()),
            };
        }
        match matches.len() {
            0 => None,
            1 => matches.into_iter().next(),
            _ => {
                let provenance_matches = matches
                    .iter()
                    .filter(|entry| {
                        entry.artifact_source_key.is_some()
                            && entry.artifact_source_key == original_source_key
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if provenance_matches.len() == 1 {
                    provenance_matches.into_iter().next()
                } else {
                    Some(unavailable_entry())
                }
            }
        }
    }

    pub fn match_by_artifact(&self, artifact_source: &str) -> Option<PluginAvailabilityMatch> {
        let digest = plugin_availability_source_key(artifact_source);
        let current = self.current.read();
        for (marketplace_source, entries) in &current.sources {
            if let Some((catalog_id, _)) = entries
                .iter()
                .find(|(_, entry)| entry.artifact_source_key.as_deref() == Some(digest.as_str()))
            {
                return Some(PluginAvailabilityMatch {
                    marketplace_source: marketplace_source.clone(),
                    catalog_id: catalog_id.clone(),
                });
            }
        }
        None
    }

    pub fn supports(&self, record: &PluginRecord, client: PluginClient, platform: &str) -> bool {
        self.entry_for(record).is_none_or(|entry| {
            marketplace_entry_installable_on_host(&entry, client, platform)
        })
    }

    pub fn entry_supports_host(
        &self,
        entry: &impl MarketplaceHostEntry,
        client: PluginClient,
        platform: &str,
    ) -> bool {
        marketplace_entry_installable_on_host(entry, client, platform)
    }

    fn replace_current(&self, next: PluginAvailabilityFile) {
        let mut current = self.current.write();
        if availability_file_value(&current) != availability_file_value(&next) {
            self.generation_value.fetch_add(1, Ordering::AcqRel);
        }
        *current = next;
    }
}

fn unavailable_entry() -> PluginAvailabilityEntry {
    PluginAvailabilityEntry {
        clients: Some(Vec::new()),
        platforms: Some(Vec::new()),
        artifact_source_key: None,
        checked_at: String::new(),
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn availability_entry_from_marketplace(
    entry: &PluginMarketplaceEntry,
    checked_at: &str,
) -> PluginAvailabilityEntry {
    PluginAvailabilityEntry {
        clients: entry.clients.clone(),
        platforms: entry.platforms.clone(),
        artifact_source_key: Some(plugin_availability_source_key(&entry.source)),
        checked_at: checked_at.to_string(),
    }
}

pub fn shared_availability_store(store: PluginAvailabilityStore) -> Arc<PluginAvailabilityStore> {
    Arc::new(store)
}
