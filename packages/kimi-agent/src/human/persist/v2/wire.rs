use std::fmt;

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

pub const V2_WIRE_PROTOCOL_VERSION: &str = "1.5";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug)]
pub struct V2WireRecord {
    pub type_: String,
    pub time: Option<f64>,
    pub fields: Map<String, Value>,
}

impl V2WireRecord {
    fn from_object(mut object: Map<String, Value>) -> Option<Self> {
        let type_ = object.remove("type")?.as_str()?.to_string();
        let time = object.remove("time").and_then(|time| time.as_f64());
        Some(Self {
            type_,
            time,
            fields: object,
        })
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match key {
            "type" => None,
            "time" => None,
            _ => self.fields.get(key),
        }
    }

    pub fn insert(&mut self, key: impl Into<String>, value: Value) {
        let key = key.into();
        match key.as_str() {
            "type" => {
                if let Some(type_) = value.as_str() {
                    self.type_ = type_.to_string();
                }
            }
            "time" => self.time = value.as_f64(),
            _ => {
                self.fields.insert(key, value);
            }
        }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        match key {
            "type" => true,
            "time" => self.time.is_some(),
            _ => self.fields.contains_key(key),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut object = self.fields.clone();
        object.insert("type".to_string(), Value::String(self.type_.clone()));
        if let Some(time) = self.time {
            object.insert(
                "time".to_string(),
                serde_json::Number::from_f64(time)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            );
        }
        Value::Object(object)
    }
}

#[derive(Debug)]
pub struct V2WireError {
    pub code: String,
    pub message: String,
}

impl V2WireError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for V2WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for V2WireError {}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'..='\u{000d}'
                | ' '
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

fn has_js_decimal_syntax(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut digits = 0_usize;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        digits += 1;
        index += 1;
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            digits += 1;
            index += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            return false;
        }
    }
    index == bytes.len()
}

fn js_number(raw: &str) -> f64 {
    let raw = js_trim(raw);
    if raw.is_empty() {
        return 0.0;
    }
    match raw {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0b", 2), ("0B", 2), ("0o", 8), ("0O", 8)] {
        if let Some(digits) = raw.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            let mut value = 0.0;
            for digit in digits.chars() {
                let Some(digit) = digit.to_digit(radix) else {
                    return f64::NAN;
                };
                value = value * f64::from(radix) + f64::from(digit);
            }
            return value;
        }
    }
    if !has_js_decimal_syntax(raw) {
        return f64::NAN;
    }
    raw.parse::<f64>().unwrap_or(f64::NAN)
}

fn compare_wire_versions(left: &str, right: &str) -> f64 {
    let left = left.split('.').collect::<Vec<_>>();
    let right = right.split('.').collect::<Vec<_>>();
    for index in 0..left.len().max(right.len()) {
        let a = js_number(left.get(index).unwrap_or(&"0"));
        let b = js_number(right.get(index).unwrap_or(&"0"));
        let difference = a - b;
        if difference != 0.0 {
            return difference;
        }
    }
    0.0
}

fn migrate_v1_0_tool_call(tool_call: &Value) -> Value {
    let Some(record) = tool_call.as_object() else {
        return tool_call.clone();
    };
    let Some(function) = record.get("function").and_then(Value::as_object) else {
        return tool_call.clone();
    };
    let mut migrated = record.clone();
    migrated.remove("function");
    if let Some(name) = function.get("name") {
        migrated.insert("name".to_string(), name.clone());
    }
    if let Some(arguments) = function.get("arguments") {
        migrated.insert("arguments".to_string(), arguments.clone());
    }
    Value::Object(migrated)
}

fn migrate_v1_0_record(record: V2WireRecord) -> V2WireRecord {
    if record.type_ != "context.append_message" {
        return record;
    }
    let Some(message) = record.fields.get("message").and_then(Value::as_object) else {
        return record;
    };
    let Some(tool_calls) = message.get("toolCalls").and_then(Value::as_array) else {
        return record;
    };
    let mut migrated = record;
    let mut message = message.clone();
    message.insert(
        "toolCalls".to_string(),
        Value::Array(tool_calls.iter().map(migrate_v1_0_tool_call).collect()),
    );
    migrated
        .fields
        .insert("message".to_string(), Value::Object(message));
    migrated
}

fn is_valid_compaction_record(record: &V2WireRecord) -> bool {
    if record.get("summary").is_some_and(Value::is_string)
        && record.get("compactedCount").is_some_and(Value::is_number)
    {
        return true;
    }
    if record.get("contextSummary").is_some_and(Value::is_string)
        && record.get("compactedCount").is_some_and(Value::is_number)
    {
        return true;
    }
    record.contains_key("summary") && record.get("count").is_some_and(Value::is_number)
}

fn positive_safe_integer(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_f64)
        .is_some_and(|number| {
            number.is_finite()
                && number.fract() == 0.0
                && number > 0.0
                && number <= MAX_SAFE_INTEGER as f64
        })
}

fn passes_validation(record: &V2WireRecord) -> bool {
    match record.type_.as_str() {
        "context.undo" => positive_safe_integer(record.get("count")),
        "context.apply_compaction" => is_valid_compaction_record(record),
        "turn.prompt" => record
            .get("promptId")
            .is_none_or(|prompt_id| prompt_id.is_string()),
        "turn.ended" => {
            record.get("turnId").is_some_and(Value::is_number)
                && record
                    .get("reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| {
                        matches!(reason, "completed" | "cancelled" | "failed" | "blocked")
                    })
        }
        "llm.request" => {
            record.get("provider").is_some_and(Value::is_string)
                && record.get("model").is_some_and(Value::is_string)
        }
        "tools.update_store" => record.get("key").is_some_and(Value::is_string),
        _ => true,
    }
}

fn consumed(type_: &str) -> bool {
    matches!(
        type_,
        "context.append_message"
            | "context.append_loop_event"
            | "context.apply_compaction"
            | "context.clear"
            | "context.undo"
            | "turn.prompt"
            | "turn.cancel"
            | "turn.ended"
            | "llm.request"
            | "tools.update_store"
    )
}

pub async fn read_v2_wire_records(
    path: &str,
    agent_id: &str,
) -> Result<Vec<V2WireRecord>, V2WireError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| V2WireError::new("io", error.to_string()))?;
    let mut lines = BufReader::new(file).lines();
    let mut first = true;
    let mut migrate = false;
    let mut records = Vec::new();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => return Err(V2WireError::new("io", error.to_string())),
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            break;
        };
        let Some(object) = value.as_object().cloned() else {
            break;
        };
        let Some(mut record) = V2WireRecord::from_object(object) else {
            break;
        };
        if first {
            first = false;
            if record.type_ == "metadata" {
                let version = record
                    .get("protocol_version")
                    .and_then(Value::as_str)
                    .unwrap_or(V2_WIRE_PROTOCOL_VERSION);
                if compare_wire_versions(version, V2_WIRE_PROTOCOL_VERSION) > 0.0 {
                    return Err(V2WireError::new(
                        "unsupported-wire-version",
                        format!(
                            "wire protocol version {version} is newer than supported {V2_WIRE_PROTOCOL_VERSION}"
                        ),
                    ));
                }
                migrate = compare_wire_versions(version, "1.1") < 0.0;
                continue;
            }
        }
        if record.type_ == "metadata" {
            continue;
        }
        if migrate {
            record = migrate_v1_0_record(record);
        }
        if !consumed(&record.type_) {
            continue;
        }
        match record.get("agentId") {
            None => record.insert("agentId", Value::String(agent_id.to_string())),
            Some(Value::String(current)) if current == agent_id => {}
            _ => continue,
        }
        if !passes_validation(&record) {
            continue;
        }
        records.push(record);
    }
    Ok(records)
}
