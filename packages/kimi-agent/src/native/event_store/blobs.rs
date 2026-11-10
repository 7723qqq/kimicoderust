use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD as BASE64};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const BLOBREF_PROTOCOL: &str = "blobref:";
pub const BLOB_OFFLOAD_THRESHOLD: usize = 4096;
pub const MISSING_MEDIA_PLACEHOLDER: &str = "[media missing]";
pub const BLOB_CACHE_MAX_BYTES: usize = 50 * 1024 * 1024;

const LENIENT_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

pub const BLOB_TABLE_SQL: &str = "
    CREATE TABLE IF NOT EXISTS wire_blobs (
        hash TEXT PRIMARY KEY,
        mime TEXT NOT NULL,
        bytes BLOB NOT NULL
    );";

#[derive(Debug)]
struct ByteLruCache {
    values: HashMap<String, Arc<Vec<u8>>>,
    order: VecDeque<String>,
    current_bytes: usize,
    max_bytes: usize,
}

impl ByteLruCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            values: HashMap::new(),
            order: VecDeque::new(),
            current_bytes: 0,
            max_bytes,
        }
    }

    fn get(&mut self, key: &str) -> Option<Arc<Vec<u8>>> {
        let value = self.values.get(key)?.clone();
        if let Some(index) = self.order.iter().position(|entry| entry == key) {
            self.order.remove(index);
        }
        self.order.push_back(key.to_string());
        Some(value)
    }

    fn set(&mut self, key: &str, value: Arc<Vec<u8>>) {
        self.remove(key);
        if value.len() > self.max_bytes {
            return;
        }
        while !self.values.is_empty() && self.current_bytes + value.len() > self.max_bytes {
            self.evict_oldest();
        }
        self.current_bytes += value.len();
        self.order.push_back(key.to_string());
        self.values.insert(key.to_string(), value);
    }

    fn evict_oldest(&mut self) {
        let Some(oldest) = self.order.pop_front() else {
            return;
        };
        if let Some(value) = self.values.remove(&oldest) {
            self.current_bytes -= value.len();
        }
    }

    fn remove(&mut self, key: &str) {
        let Some(value) = self.values.remove(key) else {
            return;
        };
        self.current_bytes -= value.len();
        if let Some(index) = self.order.iter().position(|entry| entry == key) {
            self.order.remove(index);
        }
    }
}

#[derive(Debug)]
pub struct BlobCache {
    inner: Mutex<ByteLruCache>,
}

impl BlobCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ByteLruCache::new(BLOB_CACHE_MAX_BYTES)),
        }
    }

    fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        self.inner.lock().get(key)
    }

    fn set(&self, key: &str, value: Arc<Vec<u8>>) {
        self.inner.lock().set(key, value);
    }
}

impl Default for BlobCache {
    fn default() -> Self {
        Self::new()
    }
}

pub fn offload_data_uris(
    payload: &mut Value,
    conn: &Connection,
    cache: &BlobCache,
) -> rusqlite::Result<bool> {
    let mut changed = false;
    transform_media_urls(payload, &mut |url| {
        if url.starts_with(BLOBREF_PROTOCOL) {
            return Ok(());
        }
        let Some((mime, base64_payload)) = parse_data_uri(url) else {
            return Ok(());
        };
        if base64_payload.encode_utf16().count() < BLOB_OFFLOAD_THRESHOLD {
            return Ok(());
        }
        let hash = sha256_hex(base64_payload.as_bytes());
        let bytes = decode_lenient_base64(base64_payload);
        conn.execute(
            "INSERT OR IGNORE INTO wire_blobs (hash, mime, bytes) VALUES (?1, ?2, ?3)",
            params![hash, mime, bytes],
        )?;
        cache.set(&hash, Arc::new(bytes));
        *url = format!("{BLOBREF_PROTOCOL}{mime};{hash}");
        changed = true;
        Ok(())
    })?;
    Ok(changed)
}

pub fn has_inline_data_uri(value: &Value) -> bool {
    match value {
        Value::Object(map) if map.contains_key("type") => map.values().any(|child| {
            if is_media_container(child) {
                matches!(child.get("url"), Some(Value::String(url)) if url.starts_with("data:"))
            } else {
                has_inline_data_uri(child)
            }
        }),
        Value::Object(map) => map.values().any(has_inline_data_uri),
        Value::Array(items) => items.iter().any(has_inline_data_uri),
        _ => false,
    }
}

pub fn rehydrate_blob_refs(
    payload: &mut Value,
    conn: &Connection,
    cache: &BlobCache,
) -> rusqlite::Result<bool> {
    let mut changed = false;
    transform_media_urls(payload, &mut |url| {
        if !url.starts_with(BLOBREF_PROTOCOL) {
            return Ok(());
        }
        let replacement = match parse_blob_ref(url) {
            Some((mime, hash)) => match read_blob(hash, conn, cache) {
                Some(bytes) => format!("data:{mime};base64,{}", BASE64.encode(bytes.as_slice())),
                None => MISSING_MEDIA_PLACEHOLDER.to_string(),
            },
            None => MISSING_MEDIA_PLACEHOLDER.to_string(),
        };
        *url = replacement;
        changed = true;
        Ok(())
    })?;
    Ok(changed)
}

fn read_blob(hash: &str, conn: &Connection, cache: &BlobCache) -> Option<Arc<Vec<u8>>> {
    if let Some(cached) = cache.get(hash) {
        return Some(cached);
    }
    let payload = conn
        .query_row(
            "SELECT bytes FROM wire_blobs WHERE hash = ?1",
            params![hash],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .ok()
        .flatten()?;
    let payload = Arc::new(payload);
    cache.set(hash, payload.clone());
    Some(payload)
}

fn transform_media_urls(
    value: &mut Value,
    transform: &mut dyn FnMut(&mut String) -> rusqlite::Result<()>,
) -> rusqlite::Result<()> {
    match value {
        Value::Object(map) => {
            let is_content_part = map.contains_key("type");
            for child in map.values_mut() {
                if is_content_part && is_media_container(child) {
                    if let Some(Value::String(url)) = child.get_mut("url") {
                        transform(url)?;
                    }
                } else {
                    transform_media_urls(child, transform)?;
                }
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                transform_media_urls(item, transform)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn is_media_container(value: &Value) -> bool {
    matches!(value, Value::Object(map) if map.contains_key("url"))
}

fn parse_data_uri(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let semicolon = rest.find(';')?;
    let mime = &rest[..semicolon];
    if mime.is_empty() {
        return None;
    }
    let payload = rest[semicolon..].strip_prefix(";base64,")?;
    Some((mime, payload))
}

fn parse_blob_ref(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix(BLOBREF_PROTOCOL)?;
    let (mime, hash) = rest.split_once(';')?;
    if hash.is_empty() {
        return None;
    }
    Some((mime, hash))
}

fn decode_lenient_base64(payload: &str) -> Vec<u8> {
    let mut cleaned = Vec::with_capacity(payload.len());
    for unit in payload.encode_utf16() {
        let byte = unit as u8;
        match byte {
            b'=' => break,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => cleaned.push(byte),
            b'-' => cleaned.push(b'+'),
            b'_' => cleaned.push(b'/'),
            _ => {}
        }
    }
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    LENIENT_BASE64.decode(cleaned).unwrap_or_default()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(BLOB_TABLE_SQL).unwrap();
        conn
    }

    fn image_part(url: String) -> Value {
        serde_json::json!({"type": "image_url", "image_url": {"url": url}})
    }

    fn part_url(payload: &Value) -> &str {
        payload["image_url"]["url"].as_str().unwrap()
    }

    fn buf(size: usize) -> Arc<Vec<u8>> {
        Arc::new(vec![0; size])
    }

    #[test]
    fn cache_returns_stored_bytes_and_none_for_a_missing_key() {
        let mut cache = ByteLruCache::new(16);
        cache.set("a", Arc::new(b"hello".to_vec()));
        assert_eq!(cache.get("a").unwrap().as_slice(), b"hello");
        assert!(cache.get("nope").is_none());
    }

    #[test]
    fn cache_evicts_the_least_recently_used_entry() {
        let mut cache = ByteLruCache::new(10);
        cache.set("a", buf(5));
        cache.set("b", buf(5));
        cache.set("c", buf(5));
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn cache_get_refreshes_recency() {
        let mut cache = ByteLruCache::new(10);
        cache.set("a", buf(5));
        cache.set("b", buf(5));
        cache.get("a");
        cache.set("c", buf(5));
        assert!(cache.get("b").is_none());
        assert!(cache.get("a").is_some());
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn cache_rejects_oversized_values_without_evicting_other_entries() {
        let mut cache = ByteLruCache::new(10);
        cache.set("a", buf(5));
        cache.set("big", buf(11));
        assert!(cache.get("big").is_none());
        assert!(cache.get("a").is_some());
    }

    #[test]
    fn cache_reaccounts_a_replaced_key() {
        let mut cache = ByteLruCache::new(10);
        cache.set("a", buf(4));
        cache.set("a", buf(9));
        cache.set("b", buf(2));
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some());
    }

    #[test]
    fn cache_evicts_other_entries_when_a_replacement_grows() {
        let mut cache = ByteLruCache::new(10);
        cache.set("a", buf(4));
        cache.set("b", buf(6));
        cache.set("a", buf(9));
        assert_eq!(cache.get("a").unwrap().len(), 9);
        assert!(cache.get("b").is_none());
    }

    #[test]
    fn cache_removes_an_existing_key_when_its_replacement_is_oversized() {
        let mut cache = ByteLruCache::new(10);
        cache.set("a", buf(4));
        cache.set("a", buf(11));
        assert!(cache.get("a").is_none());
        cache.set("b", buf(7));
        cache.set("c", buf(3));
        assert_eq!(cache.get("b").unwrap().len(), 7);
        assert_eq!(cache.get("c").unwrap().len(), 3);
    }

    #[test]
    fn cache_evicts_multiple_entries_for_one_value() {
        let mut cache = ByteLruCache::new(10);
        cache.set("a", buf(3));
        cache.set("b", buf(3));
        cache.set("c", buf(3));
        cache.set("d", buf(5));
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_none());
        assert!(cache.get("c").is_some());
        assert!(cache.get("d").is_some());
    }

    #[test]
    fn a_large_data_uri_is_offloaded_and_rehydrated() {
        let conn = memory_connection();
        let cache = BlobCache::new();
        let inline = BASE64.encode(vec![7u8; 8192]);
        let mut payload = serde_json::json!({
            "message": {"content": [image_part(format!("data:image/png;base64,{inline}"))]}
        });
        assert!(offload_data_uris(&mut payload, &conn, &cache).unwrap());
        let stored = payload["message"]["content"][0]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert!(stored.starts_with(BLOBREF_PROTOCOL));
        let blobs: i64 = conn
            .query_row("SELECT COUNT(*) FROM wire_blobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(blobs, 1);
        assert!(rehydrate_blob_refs(&mut payload, &conn, &cache).unwrap());
        assert_eq!(
            payload["message"]["content"][0]["image_url"]["url"]
                .as_str()
                .unwrap(),
            format!("data:image/png;base64,{inline}")
        );
    }

    #[test]
    fn a_small_data_uri_stays_inline() {
        let conn = memory_connection();
        let cache = BlobCache::new();
        let inline = BASE64.encode(vec![1u8; 16]);
        let uri = format!("data:image/png;base64,{inline}");
        let mut payload = image_part(uri.clone());
        assert!(!offload_data_uris(&mut payload, &conn, &cache).unwrap());
        assert_eq!(part_url(&payload), uri);
    }

    #[test]
    fn only_direct_media_fields_of_content_parts_are_rewritten() {
        let conn = memory_connection();
        let cache = BlobCache::new();
        let uri = format!("data:image/png;base64,{}", "A".repeat(5000));
        let mut payload = serde_json::json!({
            "metadata": {"url": uri},
            "content": [image_part(uri.clone())]
        });
        assert!(!has_inline_data_uri(&payload["metadata"]));
        assert!(offload_data_uris(&mut payload, &conn, &cache).unwrap());
        assert_eq!(payload["metadata"]["url"].as_str().unwrap(), uri);
        assert!(
            payload["content"][0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with(BLOBREF_PROTOCOL)
        );
    }

    #[test]
    fn malformed_blob_refs_become_the_placeholder() {
        let conn = memory_connection();
        let cache = BlobCache::new();
        for reference in ["blobref:", "blobref:image/png", "blobref:image/png;"] {
            let mut payload = image_part(reference.to_string());
            assert!(rehydrate_blob_refs(&mut payload, &conn, &cache).unwrap());
            assert_eq!(part_url(&payload), MISSING_MEDIA_PLACEHOLDER);
        }
    }

    #[test]
    fn an_empty_mime_type_is_preserved() {
        let conn = memory_connection();
        let cache = BlobCache::new();
        let inline = BASE64.encode(vec![7u8; 8192]);
        let mut payload = image_part(format!("data:image/png;base64,{inline}"));
        offload_data_uris(&mut payload, &conn, &cache).unwrap();
        let reference = part_url(&payload).replacen("image/png", "", 1);
        payload["image_url"]["url"] = Value::String(reference);
        rehydrate_blob_refs(&mut payload, &conn, &cache).unwrap();
        assert_eq!(part_url(&payload), format!("data:;base64,{inline}"));
    }

    #[test]
    fn one_hash_rehydrates_with_each_references_own_mime_type() {
        let conn = memory_connection();
        let cache = BlobCache::new();
        let inline = BASE64.encode(vec![7u8; 8192]);
        let mut payload = serde_json::json!({"content": [
            image_part(format!("data:image/png;base64,{inline}")),
            image_part(format!("data:image/jpeg;base64,{inline}"))
        ]});
        offload_data_uris(&mut payload, &conn, &cache).unwrap();
        rehydrate_blob_refs(&mut payload, &conn, &cache).unwrap();
        assert_eq!(
            payload["content"][0]["image_url"]["url"].as_str().unwrap(),
            format!("data:image/png;base64,{inline}")
        );
        assert_eq!(
            payload["content"][1]["image_url"]["url"].as_str().unwrap(),
            format!("data:image/jpeg;base64,{inline}")
        );
    }

    #[test]
    fn a_database_read_failure_is_treated_as_missing_media() {
        let conn = memory_connection();
        let cache = BlobCache::new();
        conn.execute("DROP TABLE wire_blobs", []).unwrap();
        let mut payload = image_part("blobref:image/png;deadbeef".to_string());
        assert!(rehydrate_blob_refs(&mut payload, &conn, &cache).unwrap());
        assert_eq!(part_url(&payload), MISSING_MEDIA_PLACEHOLDER);
    }

    #[test]
    fn base64_decoding_matches_buffer_leniency() {
        assert_eq!(decode_lenient_base64("YQ"), b"a");
        assert_eq!(decode_lenient_base64("Y Q = =\n"), b"a");
        assert_eq!(decode_lenient_base64("Y-Q_"), [0x63, 0xe4, 0x3f]);
        assert_eq!(decode_lenient_base64("YQ!"), b"a");
        assert_eq!(decode_lenient_base64("YQ==Yg=="), b"a");
        assert_eq!(decode_lenient_base64("AA=A"), [0]);
        assert!(decode_lenient_base64("A").is_empty());
        assert!(decode_lenient_base64("😀YQ").is_empty());
        assert_eq!(decode_lenient_base64("𝄞YQ"), [0xe1, 0x84]);
        assert_eq!(decode_lenient_base64("YQ😀Yg"), b"a");
    }
}
