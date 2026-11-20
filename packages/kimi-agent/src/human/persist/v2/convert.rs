use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::human::agent::turn::{
    AssistantEntry, AssistantMeta, HistoryMessage, SystemEntry, SystemMeta, ToolEntry, ToolMeta,
    UserEntry, UserMeta,
};
use crate::human::llm::message::{
    AssistantMessage, ContentPart, Role, SystemMessage, ToolMessage, UserMessage,
};
use crate::human::llm::usage::empty_usage;
use crate::human::persist::v2::fold::{V2AssistantExtra, V2ContextMessage, V2PromptOrigin};
use crate::wire::record::WireAsyncError;

const BLOBREF_PROTOCOL: &str = "blobref:";
const MISSING_MEDIA_PLACEHOLDER: &str = "[media missing]";

pub type V2BlobResolver = Arc<
    dyn Fn(String) -> BoxFuture<'static, Result<Option<String>, WireAsyncError>> + Send + Sync,
>;

fn parse_blob_ref(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix(BLOBREF_PROTOCOL)?;
    let (mime_type, hash) = rest.split_once(';')?;
    if hash.is_empty() {
        return None;
    }
    Some((mime_type.to_string(), hash.to_string()))
}

async fn resolve_part(
    part: &ContentPart,
    resolve_blob: &V2BlobResolver,
) -> Result<ContentPart, WireAsyncError> {
    let original = serde_json::to_value(part)?;
    let Some(object) = original.as_object() else {
        return Ok(part.clone());
    };
    let mut updated = None;
    for (key, value) in object {
        let Some(nested) = value.as_object() else {
            continue;
        };
        let Some(url) = nested.get("url").and_then(Value::as_str) else {
            continue;
        };
        let Some((mime_type, hash)) = parse_blob_ref(url) else {
            continue;
        };
        let payload = resolve_blob(hash).await?;
        let resolved = payload.map_or_else(
            || MISSING_MEDIA_PLACEHOLDER.to_string(),
            |payload| format!("data:{mime_type};base64,{payload}"),
        );
        let updated = updated.get_or_insert_with(|| original.clone());
        let Some(updated) = updated.as_object_mut() else {
            continue;
        };
        let mut nested = nested.clone();
        nested.insert("url".to_string(), Value::String(resolved));
        updated.insert(key.clone(), Value::Object(nested));
    }
    match updated {
        Some(updated) => Ok(serde_json::from_value(updated)?),
        None => Ok(part.clone()),
    }
}

async fn resolve_content(
    content: &[ContentPart],
    resolve_blob: &V2BlobResolver,
) -> Result<Vec<ContentPart>, WireAsyncError> {
    let mut parts = Vec::with_capacity(content.len());
    for part in content {
        parts.push(resolve_part(part, resolve_blob).await?);
    }
    Ok(parts)
}

fn map_origin_to_source(origin: Option<&V2PromptOrigin>) -> String {
    let Some(origin) = origin else {
        return "input".to_string();
    };
    if origin.kind == "user"
        || ((origin.kind == "skill_activation" || origin.kind == "plugin_command")
            && origin.trigger.as_deref() == Some("user-slash"))
    {
        return "input".to_string();
    }
    origin.kind.clone()
}

fn map_finish_reason(reason: Option<&str>) -> Option<&'static str> {
    match reason {
        Some("tool_use") => Some("tool_calls"),
        Some("end_turn") => Some("completed"),
        Some("max_tokens") => Some("truncated"),
        Some("filtered") => Some("filtered"),
        Some("paused") => Some("paused"),
        Some("other") => Some("other"),
        _ => None,
    }
}

fn build_assistant_meta(extra: Option<&V2AssistantExtra>) -> AssistantMeta {
    let mut meta = json!({
        "usage": extra.and_then(|extra| extra.usage.clone()).unwrap_or_else(empty_usage)
    });
    let Some(extra) = extra else {
        return serde_json::from_value(meta).expect("valid assistant metadata");
    };
    if let Some(model) = &extra.model {
        meta["model"] = serde_json::to_value(model).expect("serializing a model cannot fail");
    }
    if let Some(message_id) = &extra.message_id {
        meta["messageId"] = Value::String(message_id.clone());
    }
    let raw_finish_reason = extra
        .raw_finish_reason
        .as_ref()
        .or(extra.provider_finish_reason.as_ref());
    let finish_reason = map_finish_reason(extra.finish_reason.as_deref());
    if finish_reason.is_some() || raw_finish_reason.is_some() {
        meta["finish"] = json!({
            "finishReason": finish_reason,
            "rawFinishReason": raw_finish_reason,
        });
    }
    serde_json::from_value(meta).expect("valid assistant metadata")
}

pub async fn convert_v2_message(
    message: &V2ContextMessage,
    extra: Option<&V2AssistantExtra>,
    resolve_blob: V2BlobResolver,
) -> Result<Option<HistoryMessage>, WireAsyncError> {
    let content = resolve_content(&message.content, &resolve_blob).await?;
    match message.role.as_str() {
        "system" => Ok(Some(HistoryMessage::System(SystemEntry {
            message: SystemMessage {
                role: Role::System,
                content,
                tools: None,
            },
            meta: Some(SystemMeta::default()),
        }))),
        "user" => Ok(Some(HistoryMessage::User(UserEntry {
            message: UserMessage {
                role: Role::User,
                content,
            },
            meta: Some(UserMeta {
                source: Some(map_origin_to_source(message.origin.as_ref())),
                ..UserMeta::default()
            }),
        }))),
        "assistant" => Ok(Some(HistoryMessage::Assistant(AssistantEntry {
            message: AssistantMessage {
                role: Role::Assistant,
                content,
                tool_calls: message.tool_calls.clone().unwrap_or_default(),
                ..AssistantMessage::default()
            },
            meta: Some(build_assistant_meta(extra)),
        }))),
        "tool" => Ok(Some(HistoryMessage::Tool(ToolEntry {
            message: ToolMessage {
                role: Role::Tool,
                content,
                tool_call_id: message.tool_call_id.clone().unwrap_or_default(),
            },
            meta: Some(ToolMeta {
                source: Some("tool".to_string()),
                ..ToolMeta::default()
            }),
        }))),
        _ => Ok(None),
    }
}
