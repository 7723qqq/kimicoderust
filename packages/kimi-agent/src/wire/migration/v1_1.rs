use serde_json::{Map, Value};

use super::migration::{WireMigration, WireMigrationRecord};

type V10ContextAppendMessageRecord = WireMigrationRecord;
type V10ContextMessage = Map<String, Value>;
type V10ToolCall = Map<String, Value>;

pub struct MigrateV10ToV11;

pub static MIGRATE_V1_0_TO_V1_1: MigrateV10ToV11 = MigrateV10ToV11;

impl WireMigration for MigrateV10ToV11 {
    fn source_version(&self) -> &'static str {
        "1.0"
    }

    fn target_version(&self) -> &'static str {
        "1.1"
    }

    fn migrate_record(&self, mut record: WireMigrationRecord) -> WireMigrationRecord {
        if record.get("type").and_then(Value::as_str) != Some("context.append_message") {
            return record;
        }
        let mut record: V10ContextAppendMessageRecord = record;
        let mut message: V10ContextMessage = record
            .get("message")
            .and_then(Value::as_object)
            .cloned()
            .unwrap();
        let tool_calls = message
            .get("toolCalls")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|tool_call| {
                Value::Object(migrate_tool_call(
                    tool_call.as_object().cloned().unwrap(),
                ))
            })
            .collect();
        message.insert("toolCalls".to_string(), Value::Array(tool_calls));
        record.insert("message".to_string(), Value::Object(message));
        record
    }
}

fn migrate_tool_call(mut tool_call: V10ToolCall) -> WireMigrationRecord {
    let function = tool_call.remove("function").unwrap();
    let function = function.as_object().unwrap();
    tool_call.remove("name");
    tool_call.remove("arguments");
    if let Some(name) = function.get("name") {
        tool_call.insert("name".to_string(), name.clone());
    }
    if let Some(arguments) = function.get("arguments") {
        tool_call.insert("arguments".to_string(), arguments.clone());
    }
    tool_call
}
