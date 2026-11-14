use serde::Serialize;
use serde_json::{Map, Value};

use crate::human::store::types::{BranchHeader, EntryLine, Payload};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecErrorKind {
    Syntax,
    Schema,
    Seq,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecError {
    pub kind: CodecErrorKind,
    pub detail: String,
}

fn error(kind: CodecErrorKind, detail: impl Into<String>) -> CodecError {
    CodecError {
        kind,
        detail: detail.into(),
    }
}

fn parse_object(raw: &str) -> Result<Map<String, Value>, CodecError> {
    let value = serde_json::from_str::<Value>(raw)
        .map_err(|cause| error(CodecErrorKind::Syntax, cause.to_string()))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| error(CodecErrorKind::Schema, "is not a JSON object"))
}

fn require_string(object: &Map<String, Value>, key: &str) -> Result<String, CodecError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| error(CodecErrorKind::Schema, format!("has invalid {key}")))
}

fn timestamp(value: Option<&Value>) -> Option<u64> {
    let number = value?.as_f64()?;
    if number.is_finite()
        && number >= 0.0
        && number.fract() == 0.0
        && number <= MAX_SAFE_INTEGER as f64
    {
        Some(number as u64)
    } else {
        None
    }
}

fn require_timestamp(object: &Map<String, Value>, key: &str) -> Result<u64, CodecError> {
    timestamp(object.get(key))
        .ok_or_else(|| error(CodecErrorKind::Schema, format!("has invalid {key}")))
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, CodecError> {
    if !object.contains_key(key) {
        return Ok(None);
    }
    require_string(object, key).map(Some)
}

fn optional_seq(object: &Map<String, Value>, key: &str) -> Result<Option<u64>, CodecError> {
    match object.get(key) {
        None => Ok(None),
        Some(value) => timestamp(Some(value))
            .map(Some)
            .ok_or_else(|| error(CodecErrorKind::Schema, format!("has invalid {key}"))),
    }
}

fn decode_header(raw: &str) -> Result<BranchHeader, CodecError> {
    let object = parse_object(raw)?;
    if object.get("kind").and_then(Value::as_str) != Some("header") {
        return Err(error(CodecErrorKind::Schema, "is not a header"));
    }
    if object.get("version").and_then(Value::as_f64) != Some(1.0) {
        return Err(error(CodecErrorKind::Schema, "has unsupported version"));
    }
    Ok(BranchHeader {
        version: 1,
        tree: require_string(&object, "tree")?,
        branch: require_string(&object, "branch")?,
        created_at: require_timestamp(&object, "createdAt")?,
        parent_branch: optional_string(&object, "parentBranch")?,
        parent_seq: optional_seq(&object, "parentSeq")?,
    })
}

fn decode_payload(value: Option<&Value>) -> Result<Payload, CodecError> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| error(CodecErrorKind::Schema, "has invalid payload"))?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| error(CodecErrorKind::Schema, "has invalid payload.kind"))?
        .to_string();
    let size = object
        .get("size")
        .and_then(|value| timestamp(Some(value)))
        .ok_or_else(|| error(CodecErrorKind::Schema, "has invalid payload.size"))?;
    if let Some(reference) = object.get("ref") {
        let reference = reference
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| error(CodecErrorKind::Schema, "has invalid payload.ref"))?
            .to_string();
        return Ok(Payload::Offloaded {
            kind,
            size: size as usize,
            reference,
        });
    }
    let data = object
        .get("data")
        .cloned()
        .ok_or_else(|| error(CodecErrorKind::Schema, "has invalid payload.data"))?;
    Ok(Payload::Inline {
        kind,
        size: size as usize,
        data,
    })
}

fn decode_entry(raw: &str, expected_seq: u64) -> Result<EntryLine, CodecError> {
    let object = parse_object(raw)?;
    if object.get("kind").and_then(Value::as_str) != Some("entry") {
        return Err(error(CodecErrorKind::Schema, "is not an entry"));
    }
    let seq = timestamp(object.get("seq"))
        .ok_or_else(|| error(CodecErrorKind::Schema, "has invalid seq"))?;
    if seq != expected_seq {
        return Err(error(
            CodecErrorKind::Seq,
            format!("has seq {seq}, expected {expected_seq}"),
        ));
    }
    Ok(EntryLine {
        kind: "entry".to_string(),
        seq,
        ts: require_timestamp(&object, "ts")?,
        type_: require_string(&object, "type")?,
        payload: decode_payload(object.get("payload"))?,
    })
}

#[derive(Serialize)]
struct HeaderWire<'a> {
    kind: &'static str,
    #[serde(flatten)]
    header: &'a BranchHeader,
}

pub fn encode_header(header: &BranchHeader) -> String {
    let mut encoded = serde_json::to_string(&HeaderWire {
        kind: "header",
        header,
    })
    .expect("serializing a branch header cannot fail");
    encoded.push('\n');
    encoded
}

pub fn parse_header(raw: &str) -> Result<BranchHeader, CodecError> {
    decode_header(raw)
}

pub fn encode_line(entry: &EntryLine) -> String {
    let mut encoded = serde_json::to_string(entry).expect("serializing an entry cannot fail");
    encoded.push('\n');
    encoded
}

pub fn parse_line(raw: &str, expected_seq: u64) -> Result<EntryLine, CodecError> {
    decode_entry(raw, expected_seq)
}
