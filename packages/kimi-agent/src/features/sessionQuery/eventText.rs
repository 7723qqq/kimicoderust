use serde_json::Value;

pub const TEXT_FIELDS: &[&str] = &[
    "text",
    "prompt",
    "output",
    "content",
    "arguments",
    "input",
    "delta",
    "result",
    "reason",
    "description",
];

pub fn wire_record_text(record: &Value) -> String {
    let mut parts = Vec::new();
    for field in TEXT_FIELDS {
        if let Some(text) = record.get(*field).and_then(Value::as_str)
            && !text.is_empty()
        {
            parts.push(text.to_string());
        }
    }
    if let Some(content) = record.get("content").and_then(Value::as_array) {
        push_content_parts(content, &mut parts);
    }
    if let Some(message) = record.get("message").and_then(Value::as_object) {
        if let Some(content) = message.get("content").and_then(Value::as_array) {
            push_content_parts(content, &mut parts);
        }
        if let Some(text) = message.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            parts.push(text.to_string());
        }
    }
    parts.join("\n")
}

fn push_content_parts(content: &[Value], parts: &mut Vec<String>) {
    for part in content {
        if let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            parts.push(text.to_string());
        }
    }
}
