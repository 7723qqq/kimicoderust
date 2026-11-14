use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::persistence::{FileStorageService, JsonAtomicDocumentStore, StorageResult};
use crate::server::envelope::error_codes;
use kimi_core::primitives::errors::Error2;

pub type FileError = (u16, u32, String);

fn validation(message: impl Into<String>) -> FileError {
    (400, error_codes::VALIDATION_FAILED, message.into())
}

fn not_found(file_id: &str) -> FileError {
    (
        404,
        error_codes::FILE_NOT_FOUND,
        format!("file {file_id} not found"),
    )
}

fn internal(message: impl Into<String>) -> FileError {
    (500, error_codes::INTERNAL_ERROR, message.into())
}

fn internal_storage(error: Error2) -> FileError {
    internal(format!("[{}] {}", error.code().as_str(), error.message()))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    pub id: String,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

pub fn is_file_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("f_") else {
        return false;
    };
    let mut chars = rest.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
}

fn new_file_id() -> String {
    format!("f_{:016x}{:016x}", fastrand::u64(..), fastrand::u64(..))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn is_expired(meta: &FileMeta) -> bool {
    let Some(expires_at) = meta.expires_at.as_deref() else {
        return false;
    };
    match chrono::DateTime::parse_from_rfc3339(expires_at) {
        Ok(expires) => expires.with_timezone(&chrono::Utc) <= chrono::Utc::now(),
        Err(_) => false,
    }
}

#[derive(Debug, Clone, Default)]
pub struct FileStore {
    root: Option<PathBuf>,
}

pub fn resolve_attachment_reference(store: &FileStore, value: &str) -> Option<PathBuf> {
    let id = crate::llm::media_resolver::parse_daemon_file_url(value).unwrap_or(value);
    if !is_file_id(id) {
        return None;
    }
    let path = store.blob_path(id)?;
    path.is_file().then_some(path)
}

impl FileStore {
    pub fn new() -> Self {
        Self {
            root: data_home().map(|home| home.join("blobs")),
        }
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self { root: Some(root) }
    }

    fn paths(&self) -> Option<(PathBuf, PathBuf)> {
        let root = self.root.as_ref()?;
        Some((root.join("files"), root.join("file").join("index.json")))
    }

    fn read_index(index_path: &Path) -> StorageResult<Vec<FileMeta>> {
        let store = Self::index_documents(index_path);
        let Some(payload) = store.get::<Value>("file", "index.json")? else {
            return Ok(Vec::new());
        };
        let Some(items) = payload.get("files").and_then(Value::as_array) else {
            let cause = std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the index payload does not contain a files array",
            );
            return Err(crate::persistence::atomic_document_store::decode_error(
                "file",
                "index.json",
                "json",
                Box::new(cause),
            ));
        };
        items
            .iter()
            .map(|item| {
                serde_json::from_value::<FileMeta>(item.clone()).map_err(|cause| {
                    crate::persistence::atomic_document_store::decode_error(
                        "file",
                        "index.json",
                        "json",
                        Box::new(cause),
                    )
                })
            })
            .collect()
    }

    fn write_index(index_path: &Path, metas: &[FileMeta]) -> StorageResult<()> {
        let payload = json!({ "version": 1, "files": metas });
        Self::index_documents(index_path).set("file", "index.json", &payload)
    }

    fn index_documents(index_path: &Path) -> JsonAtomicDocumentStore {
        let base = index_path
            .parent()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new(""));
        JsonAtomicDocumentStore::new(FileStorageService::new(base, None, None))
    }

    fn write_blob(blob: &Path, bytes: &[u8]) -> StorageResult<()> {
        let parent = blob.parent().unwrap_or_else(|| Path::new("."));
        let key = blob.file_name().unwrap_or_default().to_string_lossy();
        FileStorageService::new(parent, None, None).write("", &key, bytes)
    }

    fn prune(blob_dir: &Path, metas: &mut Vec<FileMeta>) -> bool {
        let mut removed = Vec::new();
        metas.retain(|meta| {
            if is_expired(meta) {
                removed.push(meta.id.clone());
                false
            } else {
                true
            }
        });
        for id in &removed {
            let _ = std::fs::remove_file(blob_dir.join(id));
        }
        !removed.is_empty()
    }

    pub fn save(
        &self,
        name: &str,
        media_type: &str,
        expires_in_sec: Option<u64>,
        bytes: &[u8],
    ) -> Result<FileMeta, FileError> {
        let (blob_dir, index_path) = self
            .paths()
            .ok_or_else(|| internal("cannot resolve the file store location"))?;
        let id = new_file_id();
        std::fs::create_dir_all(&blob_dir)
            .map_err(|error| internal(format!("cannot create {}: {error}", blob_dir.display())))?;
        let blob = blob_dir.join(&id);
        Self::write_blob(&blob, bytes).map_err(internal_storage)?;

        let created_at = now_rfc3339();
        let meta = FileMeta {
            id,
            name: if name.is_empty() {
                "upload".to_string()
            } else {
                name.to_string()
            },
            media_type: if media_type.is_empty() {
                "application/octet-stream".to_string()
            } else {
                media_type.to_string()
            },
            size: bytes.len() as u64,
            created_at,
            expires_at: expires_in_sec
                .map(|seconds| chrono::Utc::now() + chrono::Duration::seconds(seconds as i64))
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        };

        let mut metas = Self::read_index(&index_path).map_err(internal_storage)?;
        Self::prune(&blob_dir, &mut metas);
        metas.push(meta.clone());
        Self::write_index(&index_path, &metas).map_err(internal_storage)?;
        Ok(meta)
    }

    pub fn save_with_id(
        &self,
        id: &str,
        name: &str,
        media_type: &str,
        expires_in_sec: Option<u64>,
        bytes: &[u8],
    ) -> Result<FileMeta, FileError> {
        if !is_file_id(id) {
            return Err(internal(format!("not a file id: {id}")));
        }
        let (blob_dir, index_path) = self
            .paths()
            .ok_or_else(|| internal("cannot resolve the file store location"))?;
        std::fs::create_dir_all(&blob_dir)
            .map_err(|error| internal(format!("cannot create {}: {error}", blob_dir.display())))?;
        let blob = blob_dir.join(id);
        if blob.is_file() {
            let mut metas = Self::read_index(&index_path).map_err(internal_storage)?;
            Self::prune(&blob_dir, &mut metas);
            if let Some(existing) = metas.iter().find(|existing| existing.id == id) {
                return Ok(existing.clone());
            }
            let size = std::fs::metadata(&blob)
                .map(|metadata| metadata.len())
                .unwrap_or(bytes.len() as u64);
            let meta = FileMeta {
                id: id.to_string(),
                name: if name.is_empty() {
                    "attachment".to_string()
                } else {
                    name.to_string()
                },
                media_type: if media_type.is_empty() {
                    "application/octet-stream".to_string()
                } else {
                    media_type.to_string()
                },
                size,
                created_at: now_rfc3339(),
                expires_at: None,
            };
            metas.push(meta.clone());
            Self::write_index(&index_path, &metas).map_err(internal_storage)?;
            return Ok(meta);
        }

        Self::write_blob(&blob, bytes).map_err(internal_storage)?;

        let meta = FileMeta {
            id: id.to_string(),
            name: if name.is_empty() {
                "attachment".to_string()
            } else {
                name.to_string()
            },
            media_type: if media_type.is_empty() {
                "application/octet-stream".to_string()
            } else {
                media_type.to_string()
            },
            size: bytes.len() as u64,
            created_at: now_rfc3339(),
            expires_at: expires_in_sec
                .map(|seconds| chrono::Utc::now() + chrono::Duration::seconds(seconds as i64))
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        };

        let mut metas = Self::read_index(&index_path).map_err(internal_storage)?;
        Self::prune(&blob_dir, &mut metas);
        metas.retain(|existing| existing.id != meta.id);
        metas.push(meta.clone());
        Self::write_index(&index_path, &metas).map_err(internal_storage)?;
        Ok(meta)
    }

    pub fn blob_path(&self, id: &str) -> Option<PathBuf> {
        let (blob_dir, _) = self.paths()?;
        Some(blob_dir.join(id))
    }

    pub fn list(&self) -> Result<Vec<FileMeta>, FileError> {
        let (blob_dir, index_path) = self
            .paths()
            .ok_or_else(|| internal("cannot resolve the file store location"))?;
        let mut metas = Self::read_index(&index_path).map_err(internal_storage)?;
        if Self::prune(&blob_dir, &mut metas) {
            Self::write_index(&index_path, &metas).map_err(internal_storage)?;
        }
        Ok(metas)
    }

    pub fn get(&self, file_id: &str) -> Result<(FileMeta, PathBuf), FileError> {
        if !is_file_id(file_id) {
            return Err(not_found(file_id));
        }
        let (blob_dir, index_path) = self
            .paths()
            .ok_or_else(|| internal("cannot resolve the file store location"))?;
        let mut metas = Self::read_index(&index_path).map_err(internal_storage)?;
        let mut dirty = Self::prune(&blob_dir, &mut metas);
        let blob = blob_dir.join(file_id);
        if !blob.is_file() {
            if metas.iter().any(|meta| meta.id == file_id) {
                metas.retain(|meta| meta.id != file_id);
                dirty = true;
            }
            if dirty {
                Self::write_index(&index_path, &metas).map_err(internal_storage)?;
            }
            return Err(not_found(file_id));
        }
        let Some(meta) = metas.iter().find(|meta| meta.id == file_id).cloned() else {
            if dirty {
                Self::write_index(&index_path, &metas).map_err(internal_storage)?;
            }
            return Err(not_found(file_id));
        };
        if dirty {
            Self::write_index(&index_path, &metas).map_err(internal_storage)?;
        }
        Ok((meta, blob))
    }

    pub fn delete(&self, file_id: &str) -> Result<(), FileError> {
        let (_, index_path) = self
            .paths()
            .ok_or_else(|| internal("cannot resolve the file store location"))?;
        let (_, blob) = self.get(file_id)?;
        std::fs::remove_file(&blob)
            .map_err(|error| internal(format!("cannot delete {}: {error}", blob.display())))?;
        let mut metas = Self::read_index(&index_path).map_err(internal_storage)?;
        metas.retain(|meta| meta.id != file_id);
        Self::write_index(&index_path, &metas).map_err(internal_storage)?;
        Ok(())
    }
}

fn data_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("KIMI_CODE_HOME") {
        return Some(PathBuf::from(home));
    }
    crate::config::dirs_home().map(|home| home.join(".kimi-code"))
}

#[derive(Debug, Clone)]
pub struct MultipartPart {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

pub fn boundary_of(content_type: &str) -> Option<String> {
    if !content_type
        .to_ascii_lowercase()
        .starts_with("multipart/form-data")
    {
        return None;
    }
    for parameter in content_type.split(';').skip(1) {
        let Some((key, value)) = parameter.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("boundary") {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

pub fn parse_multipart(body: &[u8], boundary: &str) -> Result<Vec<MultipartPart>, String> {
    let delimiter = format!("--{boundary}");
    let delimiter = delimiter.as_bytes();
    let mut parts = Vec::new();
    let mut cursor = find_bytes(body, delimiter, 0).ok_or("boundary not found")?;
    loop {
        let mut after = cursor + delimiter.len();
        if body[after..].starts_with(b"--") {
            break;
        }

        if body[after..].starts_with(b"\r\n") {
            after += 2;
        }

        let next = find_bytes(body, delimiter, after).ok_or("unterminated part")?;
        let mut end = next;
        if body[..end].ends_with(b"\r\n") {
            end -= 2;
        }
        parts.push(parse_part(&body[after..end])?);
        cursor = next;
    }
    Ok(parts)
}

fn parse_part(raw: &[u8]) -> Result<MultipartPart, String> {
    let Some(header_end) = find_bytes(raw, b"\r\n\r\n", 0) else {
        return Err("part headers are unterminated".into());
    };
    let headers = String::from_utf8_lossy(&raw[..header_end]);
    let bytes = raw[header_end + 4..].to_vec();
    let mut name = String::new();
    let mut filename = None;
    let mut content_type = None;
    for line in headers.split("\r\n") {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "content-disposition" {
            for parameter in value.split(';').skip(1) {
                let Some((key, value)) = parameter.split_once('=') else {
                    continue;
                };
                let value = value.trim().trim_matches('"');
                match key.trim().to_ascii_lowercase().as_str() {
                    "name" => name = value.to_string(),
                    "filename" => filename = Some(value.to_string()),
                    _ => {}
                }
            }
        } else if key == "content-type" {
            content_type = Some(value.to_string());
        }
    }
    Ok(MultipartPart {
        name,
        filename,
        content_type,
        bytes,
    })
}

pub fn upload(store: &FileStore, content_type: &str, body: &[u8]) -> Result<Value, FileError> {
    let Some(boundary) = boundary_of(content_type) else {
        return Err(validation("multipart not initialized"));
    };
    let parts = parse_multipart(body, &boundary).map_err(validation)?;
    let Some(file) = parts
        .iter()
        .find(|part| part.name == "file" && part.filename.is_some())
    else {
        return Err(validation("missing `file` field"));
    };
    let name_override = parts
        .iter()
        .find(|part| part.name == "name")
        .and_then(|part| String::from_utf8(part.bytes.clone()).ok())
        .filter(|value| !value.is_empty());
    let expires_in_sec = parts
        .iter()
        .find(|part| part.name == "expires_in_sec")
        .and_then(|part| String::from_utf8(part.bytes.clone()).ok())
        .and_then(|value| value.trim().parse::<u64>().ok());

    let filename = file.filename.clone().unwrap_or_else(|| "upload".into());
    let meta = store.save(
        name_override.as_deref().unwrap_or(&filename),
        file.content_type.as_deref().unwrap_or_default(),
        expires_in_sec,
        &file.bytes,
    )?;
    serde_json::to_value(meta).map_err(|error| internal(error.to_string()))
}

pub fn parse_range(header: &str, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let header = header.trim();
    let spec = header.strip_prefix("bytes=")?;
    let (start_raw, end_raw) = spec.split_once('-')?;
    if start_raw.is_empty() && end_raw.is_empty() {
        return None;
    }
    let (start, end) = if start_raw.is_empty() {
        let suffix: u64 = end_raw.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        (size.saturating_sub(suffix), size - 1)
    } else {
        let start: u64 = start_raw.parse().ok()?;
        if start >= size {
            return None;
        }
        let end = if end_raw.is_empty() {
            size - 1
        } else {
            end_raw.parse().ok()?
        };
        (start, end)
    };
    if start > end {
        return None;
    }
    Some((start, end.min(size - 1)))
}

pub fn content_disposition(name: &str, media_type: &str) -> String {
    let disposition = if media_type.starts_with("image/")
        || media_type.starts_with("video/")
        || media_type.starts_with("audio/")
    {
        "inline"
    } else {
        "attachment"
    };
    let simple = !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ". ()+[]_-".contains(character));
    if simple {
        format!("{disposition}; filename=\"{name}\"")
    } else {
        disposition.to_string()
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_resolve_attachment_reference() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::with_root(dir.path().to_path_buf());
        let meta = store
            .save_with_id("f_mcp_test", "attachment.png", "image/png", None, b"bytes")
            .unwrap();

        let by_reference =
            resolve_attachment_reference(&store, &format!("kimi-file://{}", meta.id)).unwrap();
        assert!(by_reference.is_file());
        assert_eq!(
            by_reference,
            resolve_attachment_reference(&store, &meta.id).unwrap()
        );

        assert!(
            resolve_attachment_reference(&store, &format!("kimi-file://{}?x=1", meta.id)).is_some()
        );

        assert!(resolve_attachment_reference(&store, "src/main.rs").is_none());
        assert!(resolve_attachment_reference(&store, "kimi-file://f_nope").is_none());
    }

    #[test]
    fn test_save_with_id_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::with_root(dir.path().to_path_buf());
        store
            .save_with_id("f_dup", "first", "image/png", None, b"one")
            .unwrap();
        store
            .save_with_id("f_dup", "second", "image/png", None, b"two")
            .unwrap();

        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 1);

        assert_eq!(entries[0].name, "first");
        let (_, path) = store.get("f_dup").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one");

        assert!(
            store
                .save_with_id("nope", "x", "text/plain", None, b"x")
                .is_err()
        );
    }

    use super::*;

    fn temp_store(tag: &str) -> FileStore {
        let dir = std::env::temp_dir().join(format!("kimi-files-{tag}-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        FileStore::with_root(dir)
    }

    #[test]
    fn saves_lists_and_deletes_files() {
        let store = temp_store("crud");
        let meta = store
            .save("note.txt", "text/plain", None, b"hello")
            .unwrap();
        assert!(is_file_id(&meta.id));
        assert_eq!(meta.size, 5);
        assert_eq!(meta.name, "note.txt");
        assert_eq!(meta.expires_at, None);

        let (found, path) = store.get(&meta.id).unwrap();
        assert_eq!(found, meta);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert_eq!(store.list().unwrap().len(), 1);

        store.delete(&meta.id).unwrap();
        assert!(store.get(&meta.id).is_err());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn expired_entries_disappear() {
        let store = temp_store("expiry");
        let meta = store
            .save("gone.txt", "text/plain", Some(0), b"old")
            .unwrap();
        assert!(meta.expires_at.is_some());
        assert!(store.get(&meta.id).is_err(), "zero-second expiry is past");
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn file_ids_follow_the_reference_regex() {
        assert!(is_file_id("f_abc"));
        assert!(is_file_id("f_A1-_b"));
        assert!(!is_file_id("f_"));
        assert!(!is_file_id("f_-abc"));
        assert!(!is_file_id("x_abc"));
        assert!(!is_file_id("f_a b"));
    }

    #[test]
    fn parses_multipart_with_binary_payloads() {
        let boundary = "----test-boundary";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"name\"\r\n\r\nblob.bin\r\n",
        );
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"blob.bin\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");

        body.extend_from_slice(b"\x00\x01\r\n--not-a-boundary\xff\r\n");
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let parts = parse_multipart(&body, boundary).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "name");
        assert_eq!(parts[0].bytes, b"blob.bin");
        assert_eq!(parts[1].name, "file");
        assert_eq!(parts[1].filename.as_deref(), Some("blob.bin"));
        assert_eq!(
            parts[1].content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(parts[1].bytes, b"\x00\x01\r\n--not-a-boundary\xff\r\n");
    }

    #[test]
    fn upload_reads_fields_and_stores_the_file() {
        let store = temp_store("upload");
        let boundary = "boundary-42";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"name\"\r\n\r\nMy image\r\n",
        );
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"photo.png\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        body.extend_from_slice(b"PNGDATA");
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let content_type = format!("multipart/form-data; boundary={boundary}");
        let meta = upload(&store, &content_type, &body).unwrap();
        assert_eq!(meta["name"], "My image");
        assert_eq!(meta["media_type"], "image/png");
        assert_eq!(meta["size"], 7);
        assert!(meta["id"].as_str().unwrap().starts_with("f_"));

        let error = upload(&store, "application/json", b"{}").unwrap_err();
        assert_eq!(error.1, error_codes::VALIDATION_FAILED);
    }

    #[test]
    fn malformed_index_is_a_decode_error_instead_of_an_empty_list() {
        let store = temp_store("malformed-index");
        let (_, index_path) = store.paths().unwrap();
        std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
        std::fs::write(&index_path, "{not-json").unwrap();

        let error = store.list().unwrap_err();
        assert!(
            error.2.contains("storage.decode_failed"),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn an_invalid_index_entry_is_not_silently_dropped() {
        let store = temp_store("invalid-index-entry");
        let (_, index_path) = store.paths().unwrap();
        std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
        std::fs::write(&index_path, r#"{"version":1,"files":[{"id":123}]}"#).unwrap();

        let error = store.list().unwrap_err();
        assert!(
            error.2.contains("storage.decode_failed"),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn save_cannot_overwrite_an_index_it_cannot_decode() {
        let store = temp_store("corrupt-index-save");
        let (_, index_path) = store.paths().unwrap();
        std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
        std::fs::write(&index_path, "{not-json").unwrap();

        let error = store
            .save("new.txt", "text/plain", None, b"new")
            .unwrap_err();
        assert!(
            error.2.contains("storage.decode_failed"),
            "unexpected error: {error:?}"
        );
        assert_eq!(std::fs::read_to_string(index_path).unwrap(), "{not-json");
    }

    #[test]
    fn repeated_saves_replace_the_existing_index() {
        let store = temp_store("repeat-save");
        let first = store.save("first.txt", "text/plain", None, b"one").unwrap();
        let second = store
            .save("second.txt", "text/plain", None, b"two")
            .unwrap();

        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| entry.id == first.id));
        assert!(entries.iter().any(|entry| entry.id == second.id));
    }

    #[test]
    fn ranges_cover_suffix_and_open_ended_forms() {
        assert_eq!(parse_range("bytes=0-3", 10), Some((0, 3)));
        assert_eq!(parse_range("bytes=4-", 10), Some((4, 9)));
        assert_eq!(parse_range("bytes=-3", 10), Some((7, 9)));
        assert_eq!(parse_range("bytes=8-99", 10), Some((8, 9)));
        assert_eq!(parse_range("bytes=10-", 10), None);
        assert_eq!(parse_range("bytes=5-2", 10), None);
        assert_eq!(parse_range("bytes=-", 10), None);
        assert_eq!(parse_range("items=0-2", 10), None);
        assert_eq!(parse_range("bytes=0-2", 0), None);
    }

    #[test]
    fn content_disposition_inlines_media() {
        assert_eq!(
            content_disposition("photo.png", "image/png"),
            "inline; filename=\"photo.png\""
        );
        assert_eq!(
            content_disposition("notes.txt", "text/plain"),
            "attachment; filename=\"notes.txt\""
        );
        assert_eq!(
            content_disposition("weird\"name", "text/plain"),
            "attachment"
        );
    }
}
