use std::io::{Error, ErrorKind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use futures_util::StreamExt;
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;
use crate::persistence::interface::blobStore::{BlobByteStream, IBlobStore};
use crate::wire::record::WireAsyncError;

use super::file_service::{
    FileMeta, FileReadRange, GetResult, IFileService, SaveOptions, file_not_found_error, is_file_id,
};

const BLOB_SCOPE: &str = "files";
const INDEX_SCOPE: &str = "file";
const INDEX_KEY: &str = "index.json";

#[derive(Serialize)]
struct IndexFile<'a> {
    version: u8,
    files: Vec<&'a FileMeta>,
}

#[derive(Default)]
struct FileServiceState {
    index_cache: Option<IndexMap<String, FileMeta>>,
}

pub struct FileServiceImpl {
    blobs: Arc<dyn IBlobStore>,
    state: Mutex<FileServiceState>,
}

impl FileServiceImpl {
    pub fn new(blobs: Arc<dyn IBlobStore>) -> Self {
        Self {
            blobs,
            state: Mutex::new(FileServiceState::default()),
        }
    }

    pub async fn save(
        &self,
        source: BlobByteStream,
        filename: &str,
        options: Option<SaveOptions>,
    ) -> Result<FileMeta, WireAsyncError> {
        let mut state = self.state.lock().await;
        self.ensure_index(&mut state).await?;
        self.prune_expired(&mut state).await;
        let options = options.unwrap_or_default();
        let id = format!("f_{}", uuid::Uuid::new_v4());
        let size = Arc::new(AtomicU64::new(0));
        let counted_size = size.clone();
        let counting = source.map(move |chunk| {
            if let Ok(chunk) = &chunk {
                counted_size.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            chunk
        });
        if let Err(error) = self
            .blobs
            .put_stream(BLOB_SCOPE, &id, Box::pin(counting))
            .await
        {
            let _ = self.blobs.delete(BLOB_SCOPE, &id).await;
            return Err(error);
        }
        let now = Utc::now();
        let expires_at = options
            .expires_in_sec
            .map(|seconds| now + Duration::seconds(seconds))
            .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true));
        let meta = FileMeta {
            id,
            name: options.name.unwrap_or_else(|| filename.to_string()),
            media_type: options
                .mime_type
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            size: size.load(Ordering::Relaxed),
            created_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            expires_at,
        };
        state
            .index_cache
            .as_mut()
            .expect("file index is loaded")
            .insert(meta.id.clone(), meta.clone());
        self.write_index(&state).await?;
        Ok(meta)
    }

    pub async fn get(&self, file_id: &str) -> Result<GetResult, WireAsyncError> {
        if !is_file_id(file_id) {
            return Err(Box::new(file_not_found_error(file_id)));
        }
        let mut state = self.state.lock().await;
        self.ensure_index(&mut state).await?;
        self.prune_expired(&mut state).await;
        let meta = state
            .index_cache
            .as_ref()
            .and_then(|cache| cache.get(file_id))
            .cloned()
            .ok_or_else(|| -> WireAsyncError { Box::new(file_not_found_error(file_id)) })?;
        if !self.blobs.has(BLOB_SCOPE, file_id).await? {
            state
                .index_cache
                .as_mut()
                .expect("file index is loaded")
                .shift_remove(file_id);
            self.write_index(&state).await?;
            return Err(Box::new(file_not_found_error(file_id)));
        }
        let blobs = self.blobs.clone();
        let file_id = file_id.to_string();
        Ok(GetResult::new(
            meta,
            Arc::new(move |range: Option<FileReadRange>| {
                blobs.get_stream(BLOB_SCOPE, &file_id, range.map(Into::into))
            }),
        ))
    }

    pub async fn delete(&self, file_id: &str) -> Result<(), WireAsyncError> {
        if !is_file_id(file_id) {
            return Err(Box::new(file_not_found_error(file_id)));
        }
        let mut state = self.state.lock().await;
        self.ensure_index(&mut state).await?;
        if !state
            .index_cache
            .as_ref()
            .expect("file index is loaded")
            .contains_key(file_id)
        {
            return Err(Box::new(file_not_found_error(file_id)));
        }
        state
            .index_cache
            .as_mut()
            .expect("file index is loaded")
            .shift_remove(file_id);
        self.blobs.delete(BLOB_SCOPE, file_id).await?;
        self.write_index(&state).await
    }

    async fn ensure_index(&self, state: &mut FileServiceState) -> Result<(), WireAsyncError> {
        if state.index_cache.is_some() {
            return Ok(());
        }
        self.load_index(state).await
    }

    async fn load_index(&self, state: &mut FileServiceState) -> Result<(), WireAsyncError> {
        let raw = self.blobs.get(INDEX_SCOPE, INDEX_KEY).await?;
        let Some(raw) = raw else {
            state.index_cache = Some(IndexMap::new());
            return Ok(());
        };
        let parsed = serde_json::from_slice::<Value>(&String::from_utf8_lossy(&raw).as_bytes());
        let mut map = IndexMap::new();
        match parsed {
            Ok(parsed) => {
                if let Some(files) = parsed.get("files").and_then(Value::as_array) {
                    for file in files {
                        if let Some(meta) = is_file_meta(file) {
                            map.insert(meta.id.clone(), meta);
                        }
                    }
                }
                state.index_cache = Some(map);
                self.prune_expired(state).await;
            }
            Err(_) => {
                state.index_cache = Some(IndexMap::new());
            }
        }
        Ok(())
    }

    async fn prune_expired(&self, state: &mut FileServiceState) {
        let now = Utc::now();
        let expired = state
            .index_cache
            .as_ref()
            .map(|cache| {
                cache
                    .values()
                    .filter(|meta| {
                        meta.expires_at
                            .as_deref()
                            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                            .is_some_and(|expires| expires.with_timezone(&Utc) <= now)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if expired.is_empty() {
            return;
        }
        for meta in expired {
            if let Some(cache) = state.index_cache.as_mut() {
                cache.shift_remove(&meta.id);
            }
            let _ = self.blobs.delete(BLOB_SCOPE, &meta.id).await;
        }
        let _ = self.write_index(state).await;
    }

    async fn write_index(&self, state: &FileServiceState) -> Result<(), WireAsyncError> {
        let Some(cache) = state.index_cache.as_ref() else {
            return Ok(());
        };
        let payload = IndexFile {
            version: 1,
            files: cache.values().collect(),
        };
        let bytes = serde_json::to_vec(&payload).map_err(|error| -> WireAsyncError {
            Box::new(Error::new(ErrorKind::InvalidData, error))
        })?;
        self.blobs.put(INDEX_SCOPE, INDEX_KEY, bytes).await
    }
}

#[async_trait]
impl IFileService for FileServiceImpl {
    async fn save(
        &self,
        source: BlobByteStream,
        filename: &str,
        options: Option<SaveOptions>,
    ) -> Result<FileMeta, WireAsyncError> {
        FileServiceImpl::save(self, source, filename, options).await
    }

    async fn get(&self, file_id: &str) -> Result<GetResult, WireAsyncError> {
        FileServiceImpl::get(self, file_id).await
    }

    async fn delete(&self, file_id: &str) -> Result<(), WireAsyncError> {
        FileServiceImpl::delete(self, file_id).await
    }
}

fn is_file_meta(value: &Value) -> Option<FileMeta> {
    let object = value.as_object()?;
    let id = object.get("id")?.as_str()?;
    if !is_file_id(id) {
        return None;
    }
    let name = object.get("name")?.as_str()?.to_string();
    let media_type = object.get("media_type")?.as_str()?.to_string();
    let size = object
        .get("size")?
        .as_u64()
        .filter(|size| *size <= 9_007_199_254_740_991)?;
    let created_at = object.get("created_at")?.as_str()?.to_string();
    let expires_at = match object.get("expires_at") {
        None => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => return None,
    };
    Some(FileMeta {
        id: id.to_string(),
        name,
        media_type,
        size,
        created_at,
        expires_at,
    })
}

pub fn register_file_service() {
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "file",
    );
}
