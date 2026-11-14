use std::error::Error;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use super::migration::migration::{
    WIRE_MIN_READER_VERSION, WIRE_PROTOCOL_VERSION, assert_wire_reader_version,
};

pub const AGENT_WIRE_RECORD_KEY: &str = "wire.jsonl";

pub type WireRecord = Map<String, Value>;
pub type WireMetadataRecord = WireRecord;
pub type WireAsyncError = Box<dyn Error + Send + Sync>;
pub type PartsTransformer = Arc<
    dyn Fn(Vec<Value>) -> BoxFuture<'static, Result<Vec<Value>, WireAsyncError>> + Send + Sync,
>;
pub type RecordDehydrator = Arc<
    dyn Fn(
            WireRecord,
            PartsTransformer,
        ) -> BoxFuture<'static, Result<WireRecord, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireLineRange {
    pub start: u64,
    pub end: u64,
}

pub fn is_wire_record(record: &Value) -> bool {
    record
        .as_object()
        .is_some_and(|record| record.get("type").is_some_and(Value::is_string))
}

pub fn create_wire_metadata_record(now: Option<i64>) -> WireMetadataRecord {
    let mut record = WireRecord::new();
    record.insert("type".to_string(), "metadata".into());
    record.insert(
        "protocol_version".to_string(),
        WIRE_PROTOCOL_VERSION.into(),
    );
    record.insert(
        "min_protocol_version".to_string(),
        WIRE_MIN_READER_VERSION.into(),
    );
    record.insert(
        "created_at".to_string(),
        now.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()).into(),
    );
    record
}

pub fn is_wire_metadata_record(record: &WireRecord) -> bool {
    let min_reader_version = record.get("min_protocol_version");
    record.get("type").and_then(Value::as_str) == Some("metadata")
        && record
            .get("protocol_version")
            .is_some_and(Value::is_string)
        && record.get("created_at").is_some_and(Value::is_number)
        && (min_reader_version.is_none()
            || min_reader_version
                .and_then(Value::as_str)
                .is_some_and(is_dotted_numeric_version))
}

pub fn assert_wire_records_reader_version(
    records: &[WireRecord],
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let Some(first) = records.first() else {
        return Ok(());
    };
    if first.get("type").and_then(Value::as_str) != Some("metadata") {
        return Ok(());
    }
    if !is_wire_metadata_record(first) {
        return Err("Agent wire metadata is malformed".into());
    }
    assert_wire_reader_version(
        first
            .get("min_protocol_version")
            .map(|value| value.as_str().unwrap()),
    )?;
    Ok(())
}

fn is_dotted_numeric_version(version: &str) -> bool {
    !version.is_empty()
        && version.split('.').all(|part| {
            !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
        })
}
