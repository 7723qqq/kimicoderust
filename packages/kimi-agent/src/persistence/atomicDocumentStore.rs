use std::error::Error;

use kimi_core::primitives::errors::{
    Error2, Error2Options, ErrorDetails, domains::storage as storage_errors,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::file_storage_service::{FileStorageService, StorageResult, StorageWriteOptions};

type BoxError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Copy)]
enum DocumentCodec {
    Json,
    Toml,
}

impl DocumentCodec {
    fn format(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Toml => "toml",
        }
    }

    fn encode<T: Serialize>(self, value: &T) -> Result<Vec<u8>, BoxError> {
        match self {
            Self::Json => serde_json::to_vec(value).map_err(|error| Box::new(error) as BoxError),
            Self::Toml => toml::to_string(value)
                .map(|text| format!("{}\n", text.trim_end_matches(['\r', '\n'])).into_bytes())
                .map_err(|error| Box::new(error) as BoxError),
        }
    }

    fn decode<T: DeserializeOwned>(self, bytes: &[u8]) -> Result<T, BoxError> {
        match self {
            Self::Json => {
                serde_json::from_slice(bytes).map_err(|error| Box::new(error) as BoxError)
            }
            Self::Toml => {
                let text =
                    std::str::from_utf8(bytes).map_err(|error| Box::new(error) as BoxError)?;
                let text = if text.trim().is_empty() { "" } else { text };
                toml::from_str(text).map_err(|error| Box::new(error) as BoxError)
            }
        }
    }
}

#[derive(Clone)]
struct AtomicDocumentStoreBase {
    storage: FileStorageService,
    codec: DocumentCodec,
}

impl AtomicDocumentStoreBase {
    fn new(storage: FileStorageService, codec: DocumentCodec) -> Self {
        Self { storage, codec }
    }

    fn get<T: DeserializeOwned>(&self, scope: &str, key: &str) -> StorageResult<Option<T>> {
        let Some(bytes) = self.storage.read(scope, key)? else {
            return Ok(None);
        };
        self.codec
            .decode(&bytes)
            .map(Some)
            .map_err(|cause| decode_error(scope, key, self.codec.format(), cause))
    }

    fn set<T: Serialize>(&self, scope: &str, key: &str, value: &T) -> StorageResult<()> {
        let bytes = self
            .codec
            .encode(value)
            .map_err(|cause| encode_error(scope, key, self.codec.format(), cause))?;
        self.set_bytes(scope, key, &bytes)
    }

    fn set_bytes(&self, scope: &str, key: &str, bytes: &[u8]) -> StorageResult<()> {
        self.storage.write_with_options(
            scope,
            key,
            bytes,
            StorageWriteOptions {
                atomic: Some(true),
                signal: None,
            },
        )
    }

    fn delete(&self, scope: &str, key: &str) -> StorageResult<()> {
        self.storage.delete(scope, key)
    }

    fn list(&self, scope: &str, prefix: Option<&str>) -> StorageResult<Vec<String>> {
        self.storage.list(scope, prefix)
    }

    fn acquire(&self, _scope: &str, _key: &str) -> AtomicDocumentGuard {
        AtomicDocumentGuard
    }
}

#[derive(Clone)]
pub struct JsonAtomicDocumentStore {
    base: AtomicDocumentStoreBase,
}

impl JsonAtomicDocumentStore {
    pub fn new(storage: FileStorageService) -> Self {
        Self {
            base: AtomicDocumentStoreBase::new(storage, DocumentCodec::Json),
        }
    }

    pub fn get<T: DeserializeOwned>(&self, scope: &str, key: &str) -> StorageResult<Option<T>> {
        self.base.get(scope, key)
    }

    pub fn set<T: Serialize>(&self, scope: &str, key: &str, value: &T) -> StorageResult<()> {
        self.base.set(scope, key, value)
    }

    pub fn delete(&self, scope: &str, key: &str) -> StorageResult<()> {
        self.base.delete(scope, key)
    }

    pub fn list(&self, scope: &str, prefix: Option<&str>) -> StorageResult<Vec<String>> {
        self.base.list(scope, prefix)
    }

    pub fn acquire(&self, scope: &str, key: &str) -> AtomicDocumentGuard {
        self.base.acquire(scope, key)
    }
}

#[derive(Clone)]
pub struct TomlAtomicDocumentStore {
    base: AtomicDocumentStoreBase,
}

impl TomlAtomicDocumentStore {
    pub fn new(storage: FileStorageService) -> Self {
        Self {
            base: AtomicDocumentStoreBase::new(storage, DocumentCodec::Toml),
        }
    }

    pub fn get<T: DeserializeOwned>(&self, scope: &str, key: &str) -> StorageResult<Option<T>> {
        self.base.get(scope, key)
    }

    pub fn set<T: Serialize>(&self, scope: &str, key: &str, value: &T) -> StorageResult<()> {
        self.base.set(scope, key, value)
    }

    pub fn delete(&self, scope: &str, key: &str) -> StorageResult<()> {
        self.base.delete(scope, key)
    }

    pub fn list(&self, scope: &str, prefix: Option<&str>) -> StorageResult<Vec<String>> {
        self.base.list(scope, prefix)
    }

    pub fn acquire(&self, scope: &str, key: &str) -> AtomicDocumentGuard {
        self.base.acquire(scope, key)
    }

    pub fn get_text(&self, scope: &str, key: &str) -> StorageResult<Option<String>> {
        Ok(self
            .base
            .storage
            .read(scope, key)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    pub fn set_text(&self, scope: &str, key: &str, text: &str) -> StorageResult<()> {
        self.base.set_bytes(scope, key, text.as_bytes())
    }
}

pub struct AtomicDocumentGuard;

impl AtomicDocumentGuard {
    pub fn dispose(self) {}
}

pub(crate) fn decode_error(scope: &str, key: &str, format: &str, cause: BoxError) -> Error2 {
    Error2::with_options(
        storage_errors::STORAGE_DECODE_FAILED,
        format!("failed to decode {scope}/{key} as {format}"),
        Error2Options {
            details: Some(document_details(scope, key, format)),
            cause: Some(cause),
            name: Some("StorageError".to_string()),
        },
    )
    .with_retryable(false)
}

fn encode_error(scope: &str, key: &str, format: &str, cause: BoxError) -> Error2 {
    Error2::with_options(
        storage_errors::STORAGE_CORRUPTED,
        format!("failed to encode {scope}/{key} as {format}"),
        Error2Options {
            details: Some(document_details(scope, key, format)),
            cause: Some(cause),
            name: Some("StorageError".to_string()),
        },
    )
    .with_retryable(false)
}

fn document_details(scope: &str, key: &str, format: &str) -> ErrorDetails {
    let mut details = ErrorDetails::new();
    details.insert("scope".to_string(), Value::String(scope.to_string()));
    details.insert("key".to_string(), Value::String(key.to_string()));
    details.insert("format".to_string(), Value::String(format.to_string()));
    details
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{JsonAtomicDocumentStore, TomlAtomicDocumentStore};
    use crate::persistence::file_storage_service::FileStorageService;

    #[test]
    fn json_documents_round_trip_list_delete_and_acquire_without_locking() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorageService::new(dir.path(), None, None);
        let store = JsonAtomicDocumentStore::new(storage);

        assert_eq!(store.get::<Value>("session", "missing").unwrap(), None);
        store
            .set("session", "state.json", &json!({ "title": "old" }))
            .unwrap();
        let guard = store.acquire("session", "state.json");
        store
            .set(
                "session",
                "state.json",
                &json!({ "title": "new", "count": 2 }),
            )
            .unwrap();
        store.set("session", "job-1", &json!({})).unwrap();
        store.set("session", "job-2", &json!({})).unwrap();

        assert_eq!(
            store.get::<Value>("session", "state.json").unwrap(),
            Some(json!({ "title": "new", "count": 2 }))
        );
        let mut jobs = store.list("session", Some("job-")).unwrap();
        jobs.sort();
        assert_eq!(jobs, vec!["job-1", "job-2"]);
        guard.dispose();

        store.delete("session", "state.json").unwrap();
        store.delete("session", "state.json").unwrap();
        assert_eq!(store.get::<Value>("session", "state.json").unwrap(), None);
    }

    #[test]
    fn malformed_json_and_invalid_utf8_are_decode_errors_with_context() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorageService::new(dir.path(), None, None);
        let store = JsonAtomicDocumentStore::new(storage.clone());

        for (key, bytes) in vec![
            ("bad.json", b"{ not json".to_vec()),
            ("utf8.json", vec![0xff, 0xfe]),
        ] {
            storage.write("session", key, &bytes).unwrap();
            let error = store.get::<Value>("session", key).unwrap_err();

            assert_eq!(error.code().as_str(), "storage.decode_failed");
            assert_eq!(
                error.message(),
                format!("failed to decode session/{key} as json")
            );
            let details = error.details().unwrap();
            assert_eq!(details.get("scope").unwrap(), "session");
            assert_eq!(details.get("key").unwrap(), key);
            assert_eq!(details.get("format").unwrap(), "json");
            assert!(error.cause().is_some());
            assert!(storage.read("session", key).unwrap().is_some());
        }
    }

    #[test]
    fn toml_documents_support_values_raw_text_and_decode_errors() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorageService::new(dir.path(), None, None);
        let store = TomlAtomicDocumentStore::new(storage);
        let value = toml::from_str::<toml::Value>("title = \"hello\"\ncount = 1").unwrap();

        store.set("session", "config.toml", &value).unwrap();
        assert_eq!(
            store.get::<toml::Value>("session", "config.toml").unwrap(),
            Some(value)
        );
        let text = store.get_text("session", "config.toml").unwrap().unwrap();
        assert!(text.contains("title = \"hello\""));
        assert!(text.ends_with('\n'));
        assert!(!text.ends_with("\n\n"));

        store
            .set_text("session", "raw.toml", "title = \"raw\"\n")
            .unwrap();
        assert_eq!(
            store.get_text("session", "raw.toml").unwrap(),
            Some("title = \"raw\"\n".to_string())
        );
        store
            .set_text("session", "bad.toml", "key = [unclosed")
            .unwrap();
        let error = store.get::<toml::Value>("session", "bad.toml").unwrap_err();
        assert_eq!(error.code().as_str(), "storage.decode_failed");
        assert_eq!(error.message(), "failed to decode session/bad.toml as toml");
        assert_eq!(error.details().unwrap().get("format").unwrap(), "toml");
        assert!(error.cause().is_some());

        store.delete("session", "raw.toml").unwrap();
        assert_eq!(store.get_text("session", "raw.toml").unwrap(), None);
    }

    #[test]
    fn toml_rejects_invalid_utf8_as_a_decode_failure() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorageService::new(dir.path(), None, None);
        storage.write("session", "utf8.toml", &[0xff]).unwrap();
        let store = TomlAtomicDocumentStore::new(storage);

        let error = store
            .get::<toml::Value>("session", "utf8.toml")
            .unwrap_err();

        assert_eq!(error.code().as_str(), "storage.decode_failed");
        assert!(error.cause().is_some());
        assert_eq!(error.details().unwrap().get("format").unwrap(), "toml");
    }
}
