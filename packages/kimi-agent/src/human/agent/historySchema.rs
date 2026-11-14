use serde_json::{Map, Value};

fn object(value: &Value) -> Result<&Map<String, Value>, String> {
    value.as_object().ok_or_else(|| "expected object".to_string())
}

fn string(value: &Value) -> Result<(), String> {
    value
        .as_str()
        .map(|_| ())
        .ok_or_else(|| "expected string".to_string())
}

fn number(value: &Value) -> Result<(), String> {
    value
        .as_f64()
        .map(|_| ())
        .ok_or_else(|| "expected number".to_string())
}

fn boolean(value: &Value) -> Result<(), String> {
    value
        .as_bool()
        .map(|_| ())
        .ok_or_else(|| "expected boolean".to_string())
}

fn required<'a>(obj: &'a Map<String, Value>, key: &str) -> Result<&'a Value, String> {
    obj.get(key).ok_or_else(|| format!("missing required key '{key}'"))
}

fn required_string(obj: &Map<String, Value>, key: &str) -> Result<(), String> {
    string(required(obj, key)?)
}

fn optional_string(obj: &Map<String, Value>, key: &str) -> Result<(), String> {
    if let Some(value) = obj.get(key) {
        string(value)?;
    }
    Ok(())
}

fn optional_bool(obj: &Map<String, Value>, key: &str) -> Result<(), String> {
    if let Some(value) = obj.get(key) {
        boolean(value)?;
    }
    Ok(())
}

fn literal(obj: &Map<String, Value>, key: &str, expected: &str) -> Result<(), String> {
    let value = required(obj, key)?.as_str();
    if value == Some(expected) {
        Ok(())
    } else {
        Err(format!("expected '{key}' to be '{expected}'"))
    }
}

fn array<'a>(value: &'a Value) -> Result<&'a Vec<Value>, String> {
    value.as_array().ok_or_else(|| "expected array".to_string())
}

pub fn content_part_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    match required(obj, "type")?.as_str() {
        Some("text") => {
            required_string(obj, "text")?;
            if let Some(meta) = obj.get("meta") {
                object(meta)?;
            }
            Ok(())
        }
        Some("think") => {
            required_string(obj, "think")?;
            optional_string(obj, "encrypted")?;
            if let Some(value) = obj.get("detailsIndex") {
                number(value)?;
            }
            optional_bool(obj, "hidden")?;
            optional_string(obj, "reasoningKey")
        }
        Some("image_url") => {
            let image = object(required(obj, "imageUrl")?)?;
            required_string(image, "url")?;
            optional_string(image, "id")?;
            optional_string(image, "name")
        }
        Some("audio_url") => {
            let audio = object(required(obj, "audioUrl")?)?;
            required_string(audio, "url")?;
            optional_string(audio, "id")
        }
        Some("video_url") => {
            let video = object(required(obj, "videoUrl")?)?;
            required_string(video, "url")?;
            optional_string(video, "id")?;
            optional_string(video, "name")
        }
        _ => Err("unknown content part type".to_string()),
    }
}

fn content_schema(obj: &Map<String, Value>) -> Result<(), String> {
    for part in array(required(obj, "content")?)? {
        content_part_schema(part)?;
    }
    Ok(())
}

fn tool_description_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    required_string(obj, "name")?;
    required_string(obj, "description")?;
    object(required(obj, "parameters")?)?;
    if let Some(deferred) = obj.get("deferred")
        && deferred.as_bool() != Some(true)
    {
        return Err("expected deferred to be true".to_string());
    }
    Ok(())
}

fn tool_call_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    literal(obj, "type", "function")?;
    required_string(obj, "id")?;
    required_string(obj, "name")?;
    let arguments = required(obj, "arguments")?;
    if !arguments.is_null() {
        string(arguments)?;
    }
    if let Some(extras) = obj.get("extras") {
        object(extras)?;
    }
    optional_string(obj, "rawId")?;
    if let Some(index) = obj.get("_streamIndex") {
        if number(index).is_err() {
            string(index)?;
        }
    }
    Ok(())
}

pub fn system_message_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    literal(obj, "role", "system")?;
    content_schema(obj)?;
    if let Some(tools) = obj.get("tools") {
        for tool in array(tools)? {
            tool_description_schema(tool)?;
        }
    }
    Ok(())
}

pub fn user_message_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    literal(obj, "role", "user")?;
    content_schema(obj)
}

pub fn assistant_message_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    literal(obj, "role", "assistant")?;
    content_schema(obj)?;
    for call in array(required(obj, "toolCalls")?)? {
        tool_call_schema(call)?;
    }
    Ok(())
}

pub fn tool_message_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    literal(obj, "role", "tool")?;
    content_schema(obj)?;
    required_string(obj, "toolCallId")
}

fn token_usage_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    for key in ["inputOther", "output", "inputCacheRead", "inputCacheCreation"] {
        number(required(obj, key)?)?;
    }
    if let Some(raw) = obj.get("raw") {
        object(raw)?;
    }
    Ok(())
}

fn finish_info_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    let reason = required(obj, "finishReason")?;
    if !reason.is_null() {
        let valid = reason.as_str().is_some_and(|reason| {
            matches!(
                reason,
                "completed" | "tool_calls" | "truncated" | "filtered" | "paused" | "error" | "other"
            )
        });
        if !valid {
            return Err("invalid finishReason".to_string());
        }
    }
    let raw = required(obj, "rawFinishReason")?;
    if !raw.is_null() {
        string(raw)?;
    }
    Ok(())
}

fn entry_meta_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    optional_string(obj, "source")?;
    optional_string(obj, "key")
}

fn user_meta_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    entry_meta_schema(value)?;
    optional_string(obj, "promptId")?;
    optional_bool(obj, "tracked")?;
    optional_string(obj, "createdAt")?;
    optional_string(obj, "userMessageId")
}

fn assistant_meta_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    entry_meta_schema(value)?;
    if let Some(model) = obj.get("model") {
        let model = object(model)?;
        required_string(model, "provider")?;
        required_string(model, "model")?;
    }
    token_usage_schema(required(obj, "usage")?)?;
    if let Some(headers) = obj.get("headers") {
        for value in object(headers)?.values() {
            string(value)?;
        }
    }
    if let Some(finish) = obj.get("finish") {
        finish_info_schema(finish)?;
    }
    optional_string(obj, "messageId")
}

pub fn system_entry_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    system_message_schema(required(obj, "message")?)?;
    if let Some(meta) = obj.get("meta") {
        entry_meta_schema(meta)?;
    }
    Ok(())
}

pub fn user_entry_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    user_message_schema(required(obj, "message")?)?;
    if let Some(meta) = obj.get("meta") {
        user_meta_schema(meta)?;
    }
    Ok(())
}

pub fn assistant_entry_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    assistant_message_schema(required(obj, "message")?)?;
    if let Some(meta) = obj.get("meta") {
        assistant_meta_schema(meta)?;
    }
    Ok(())
}

pub fn tool_entry_schema(value: &Value) -> Result<(), String> {
    let obj = object(value)?;
    tool_message_schema(required(obj, "message")?)?;
    if let Some(meta) = obj.get("meta") {
        entry_meta_schema(meta)?;
    }
    Ok(())
}

pub fn history_message_schema(value: &Value) -> Result<(), String> {
    if let Ok(message) = object(value).and_then(|obj| required(obj, "message")) {
        match message.get("role").and_then(Value::as_str) {
            Some("system") => return system_entry_schema(value),
            Some("user") => return user_entry_schema(value),
            Some("assistant") => return assistant_entry_schema(value),
            Some("tool") => return tool_entry_schema(value),
            _ => {}
        }
    }
    Err("invalid history message".to_string())
}
