use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::file::file_service::is_file_id;
use crate::persistence::interface::atomicDocumentStore::IAtomicDocumentStore;
use crate::persistence::interface::storage::{
    IFileSystemStorageService, StorageReadRange, StorageWriteOptions,
};
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::media_ref::{
    AUDIO_MIME_BY_SUFFIX, IMAGE_MIME_BY_SUFFIX, VIDEO_MIME_BY_SUFFIX, media_extension_for_mime,
};
use super::session_media_store::{
    ISessionMediaStore, SessionMediaFile, SessionMediaMaterializeInput, SessionMediaReadRange,
    SessionMediaReadResult, SessionMediaStreamFactory,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMediaMetadata {
    version: u8,
    key: String,
    name: String,
    #[serde(rename = "mediaType")]
    media_type: String,
}

pub struct SessionMediaStoreService {
    scope: String,
    storage: Arc<dyn IFileSystemStorageService>,
    documents: Arc<dyn IAtomicDocumentStore>,
}

impl SessionMediaStoreService {
    pub fn new(
        session_context: &dyn ISessionContext,
        storage: Arc<dyn IFileSystemStorageService>,
        documents: Arc<dyn IAtomicDocumentStore>,
    ) -> Self {
        Self {
            scope: session_context.scope(Some("media")),
            storage,
            documents,
        }
    }

    fn key_for(file_id: &str, ext: &str) -> String {
        format!("{file_id}{ext}")
    }

    fn metadata_key(file_id: &str) -> String {
        format!("meta/{file_id}.json")
    }

    fn metadata_for(value: Option<Value>, file_id: &str) -> Option<SessionMediaMetadata> {
        let metadata = serde_json::from_value::<SessionMediaMetadata>(value?).ok()?;
        (metadata.version == 1
            && (metadata.key == file_id || metadata.key.starts_with(&format!("{file_id}.")))
            && !metadata.key.contains('/')
            && !metadata.key.contains('\\')
            && !metadata.name.is_empty()
            && !metadata.media_type.is_empty())
        .then_some(metadata)
    }

    fn media_type_for_key(key: &str) -> String {
        let ext = extname(key).to_lowercase();
        IMAGE_MIME_BY_SUFFIX
            .get(ext.as_str())
            .or_else(|| VIDEO_MIME_BY_SUFFIX.get(ext.as_str()))
            .or_else(|| AUDIO_MIME_BY_SUFFIX.get(ext.as_str()))
            .unwrap_or(&"application/octet-stream")
            .to_string()
    }

    async fn find_key(&self, file_id: &str) -> Result<Option<String>, WireAsyncError> {
        let keys = self.storage.list(&self.scope, Some(file_id), None).await?;
        Ok(keys.into_iter().find(|key| {
            key == file_id
                || (key.starts_with(&format!("{file_id}.")) && !key.contains(".tmp."))
        }))
    }
}

#[async_trait]
impl ISessionMediaStore for SessionMediaStoreService {
    fn path_for(&self, file_id: &str, ext: &str) -> Option<String> {
        if !is_file_id(file_id) {
            return None;
        }
        self.storage
            .path_for(&self.scope, &Self::key_for(file_id, ext))
    }

    async fn resolve_display_path(&self, file_id: &str) -> Result<Option<String>, WireAsyncError> {
        if !is_file_id(file_id) {
            return Ok(None);
        }
        let Some(key) = self.find_key(file_id).await? else {
            return Ok(None);
        };
        Ok(self.storage.path_for(&self.scope, &key))
    }

    async fn read(&self, file_id: &str) -> Result<Option<SessionMediaReadResult>, WireAsyncError> {
        if !is_file_id(file_id) {
            return Ok(None);
        }
        let Some(key) = self.find_key(file_id).await? else {
            return Ok(None);
        };
        let data = self.storage.read(&self.scope, &key).await?;
        Ok(data.map(|data| SessionMediaReadResult { data, name: key }))
    }

    async fn open(&self, file_id: &str) -> Result<Option<SessionMediaFile>, WireAsyncError> {
        if !is_file_id(file_id) {
            return Ok(None);
        }
        let stored_metadata = self
            .documents
            .get(&self.scope, &Self::metadata_key(file_id))
            .await?;
        let metadata = Self::metadata_for(stored_metadata, file_id);
        let metadata_key = match &metadata {
            Some(metadata) => {
                if self.storage.size(&self.scope, &metadata.key).await?.is_some() {
                    Some(metadata.key.clone())
                } else {
                    None
                }
            }
            None => None,
        };
        let key = match metadata_key {
            Some(key) => Some(key),
            None => self.find_key(file_id).await?,
        };
        let Some(key) = key else {
            return Ok(None);
        };
        let Some(size) = self.storage.size(&self.scope, &key).await? else {
            return Ok(None);
        };
        let path = self.storage.path_for(&self.scope, &key);
        let name = metadata
            .as_ref()
            .map(|metadata| metadata.name.clone())
            .unwrap_or_else(|| key.clone());
        let media_type = metadata
            .map(|metadata| metadata.media_type)
            .unwrap_or_else(|| Self::media_type_for_key(&key));
        let storage = self.storage.clone();
        let scope = self.scope.clone();
        let stream: SessionMediaStreamFactory = Arc::new(move |range| {
            storage.read_stream(
                &scope,
                &key,
                range.map(|range| StorageReadRange {
                    start: range.start,
                    end: range.end,
                }),
            )
        });
        Ok(Some(SessionMediaFile {
            path,
            name,
            media_type,
            size,
            stream,
        }))
    }

    async fn materialize(
        &self,
        input: SessionMediaMaterializeInput,
    ) -> Result<Option<String>, WireAsyncError> {
        if !is_file_id(&input.file_id) {
            return Ok(None);
        }
        let name_extension = extname(&input.name);
        let ext = if name_extension.is_empty() {
            media_extension_for_mime(&input.mime_type).unwrap_or(".bin")
        } else {
            name_extension
        };
        let key = Self::key_for(&input.file_id, ext);
        let existing_size = self.storage.size(&self.scope, &key).await?;
        if existing_size != Some(input.size) {
            self.storage
                .write_stream(
                    &self.scope,
                    &key,
                    (input.stream)(),
                    Some(StorageWriteOptions {
                        atomic: Some(true),
                        signal: input.signal,
                    }),
                )
                .await?;
        }
        self.documents
            .set(
                &self.scope,
                &Self::metadata_key(&input.file_id),
                serde_json::to_value(SessionMediaMetadata {
                    version: 1,
                    key: key.clone(),
                    name: input.name,
                    media_type: input.mime_type,
                })?,
            )
            .await?;
        Ok(self.storage.path_for(&self.scope, &key))
    }
}

fn extname(path: &str) -> &str {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| &path[path.len() - extension.len() - 1..])
        .unwrap_or("")
}
