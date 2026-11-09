//! Wire content-part dehydration (v2 `PartsTransformer` / `AgentBlobServiceImpl`).
//!
//! v2 dehydrates a record before it reaches the journal: any media part whose
//! `data:` URI payload is at least [`BLOB_OFFLOAD_THRESHOLD`] characters is
//! written to the blob store and replaced by a `blobref:<mime>;<hash>` URL,
//! and the reference is expanded back to the data URI when the state is
//! restored (v2 `wireService.appendRecord` → `blobService.offloadParts`,
//! `eventDispatcherService.rehydrateStates` → `blobService.loadParts`).
//!
//! This engine's journal is a table, so the blob store is a sibling table
//! (`wire_blobs`) in the same database. A reference whose bytes are gone
//! becomes v2's `[media missing]` placeholder rather than a broken URL: the
//! session still opens, and the model is told the media is unavailable
//! instead of receiving an unusable `blobref:` string.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// v2 `BLOBREF_PROTOCOL`.
pub const BLOBREF_PROTOCOL: &str = "blobref:";

/// v2 `AgentBlobServiceImpl.DEFAULT_THRESHOLD`: the smallest base64 payload
/// worth a separate row.
pub const BLOB_OFFLOAD_THRESHOLD: usize = 4096;

/// v2 `MISSING_MEDIA_PLACEHOLDER`.
pub const MISSING_MEDIA_PLACEHOLDER: &str = "[media missing]";

/// The table the offloaded payloads live in.
pub const BLOB_TABLE_SQL: &str = "
    CREATE TABLE IF NOT EXISTS wire_blobs (
        hash TEXT PRIMARY KEY,
        mime TEXT NOT NULL,
        bytes BLOB NOT NULL
    );";

/// Replace every large inline `data:` media URL in `payload` with a blob
/// reference, storing the bytes in `wire_blobs`. Returns whether anything was
/// rewritten.
///
/// The walk is structural rather than record-shape-aware: v2's transformer
/// rewrites the `url` member of any media container, which is what every
/// content-part shape (`image_url`, `audio_url`, `video_url`, …) carries.
pub fn offload_data_uris(payload: &mut Value, conn: &Connection) -> rusqlite::Result<bool> {
    offload_value(payload, conn)
}

/// Whether `value` carries an inline `data:` media URL at all. Lets a writer
/// skip the clone-and-offload walk for the common media-free document.
pub fn has_inline_data_uri(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(url)) = map.get("url")
                && url.starts_with("data:")
            {
                return true;
            }
            map.values().any(has_inline_data_uri)
        }
        Value::Array(items) => items.iter().any(has_inline_data_uri),
        _ => false,
    }
}

fn offload_value(value: &mut Value, conn: &Connection) -> rusqlite::Result<bool> {
    match value {
        Value::Object(map) => {
            let mut changed = false;
            if let Some(Value::String(url)) = map.get_mut("url") {
                if let Some((mime, payload)) = parse_data_uri(url)
                    && payload.len() >= BLOB_OFFLOAD_THRESHOLD
                    && let Ok(bytes) = BASE64.decode(payload.as_bytes())
                {
                    let hash = sha256_hex(payload.as_bytes());
                    conn.execute(
                        "INSERT OR IGNORE INTO wire_blobs (hash, mime, bytes) VALUES (?1, ?2, ?3)",
                        params![hash, mime, bytes],
                    )?;
                    let reference = format!("{BLOBREF_PROTOCOL}{mime};{hash}");
                    *url = reference;
                    changed = true;
                }
            }
            for child in map.values_mut() {
                changed |= offload_value(child, conn)?;
            }
            Ok(changed)
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= offload_value(item, conn)?;
            }
            Ok(changed)
        }
        _ => Ok(false),
    }
}

/// Expand every `blobref:` URL in `payload` back to its `data:` URI, or to
/// [`MISSING_MEDIA_PLACEHOLDER`] when the bytes are gone. Returns whether
/// anything was rewritten.
pub fn rehydrate_blob_refs(payload: &mut Value, conn: &Connection) -> rusqlite::Result<bool> {
    let mut references = Vec::new();
    collect_blob_refs(payload, &mut references);
    if references.is_empty() {
        return Ok(false);
    }

    let mut resolved: HashMap<String, String> = HashMap::new();
    for (hash, mime) in references {
        if resolved.contains_key(&hash) {
            continue;
        }
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT bytes FROM wire_blobs WHERE hash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?;
        let replacement = match bytes {
            Some(bytes) => format!("data:{mime};base64,{}", BASE64.encode(bytes)),
            None => MISSING_MEDIA_PLACEHOLDER.to_string(),
        };
        resolved.insert(hash, replacement);
    }

    let mut changed = false;
    rewrite_blob_refs(payload, &resolved, &mut changed);
    Ok(changed)
}

fn collect_blob_refs(value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(url)) = map.get("url")
                && let Some((mime, hash)) = parse_blob_ref(url)
            {
                out.push((hash, mime));
            }
            for child in map.values() {
                collect_blob_refs(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_blob_refs(item, out);
            }
        }
        _ => {}
    }
}

fn rewrite_blob_refs(value: &mut Value, resolved: &HashMap<String, String>, changed: &mut bool) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(url)) = map.get_mut("url")
                && let Some((_, hash)) = parse_blob_ref(url)
                && let Some(replacement) = resolved.get(&hash)
            {
                *url = replacement.clone();
                *changed = true;
            }
            for child in map.values_mut() {
                rewrite_blob_refs(child, resolved, changed);
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_blob_refs(item, resolved, changed);
            }
        }
        _ => {}
    }
}

/// Split `data:<mime>;base64,<payload>` into `(mime, payload)`.
fn parse_data_uri(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    if mime.is_empty() {
        return None;
    }
    Some((mime, payload))
}

/// Split `blobref:<mime>;<hash>` into `(mime, hash)`.
fn parse_blob_ref(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix(BLOBREF_PROTOCOL)?;
    let (mime, hash) = rest.split_once(';')?;
    if mime.is_empty() || hash.is_empty() {
        return None;
    }
    Some((mime.to_string(), hash.to_string()))
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

    #[test]
    fn a_large_data_uri_is_offloaded_and_rehydrated() {
        let conn = memory_connection();
        let inline = BASE64.encode(vec![7u8; 8192]);
        let mut payload = serde_json::json!({
            "message": { "content": [ { "type": "image_url", "image_url": {
                "url": format!("data:image/png;base64,{inline}")
            } } ] }
        });

        assert!(offload_data_uris(&mut payload, &conn).unwrap());
        let stored = payload["message"]["content"][0]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert!(stored.starts_with(BLOBREF_PROTOCOL));
        let blobs: i64 = conn
            .query_row("SELECT COUNT(*) FROM wire_blobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(blobs, 1);

        assert!(rehydrate_blob_refs(&mut payload, &conn).unwrap());
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
        let inline = BASE64.encode(vec![1u8; 16]);
        let mut payload = serde_json::json!({
            "url": format!("data:image/png;base64,{inline}")
        });

        assert!(!offload_data_uris(&mut payload, &conn).unwrap());
        assert_eq!(
            payload["url"].as_str().unwrap(),
            format!("data:image/png;base64,{inline}")
        );
    }

    #[test]
    fn a_missing_blob_becomes_the_placeholder() {
        let conn = memory_connection();
        let mut payload = serde_json::json!({ "url": "blobref:image/png;deadbeef" });

        assert!(rehydrate_blob_refs(&mut payload, &conn).unwrap());
        assert_eq!(payload["url"].as_str().unwrap(), MISSING_MEDIA_PLACEHOLDER);
    }
}
