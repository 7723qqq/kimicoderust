use parking_lot::Mutex;
use serde_json::Value;

use crate::human::llm::message::{ContentPart, Message, StreamedMessagePart};

pub const KNOWN_REASONING_KEYS: &[&str] = &["reasoning_content", "reasoning_details", "reasoning"];
pub type ReasoningKey = &'static str;
pub const DEFAULT_REASONING_KEY: ReasoningKey = KNOWN_REASONING_KEYS[0];
pub const REASONING_DETAILS_KEY: &str = "reasoning_details";

pub fn extract_reasoning_strings(source: &Value) -> Vec<(String, String)> {
    let Some(record) = source.as_object() else {
        return Vec::new();
    };
    KNOWN_REASONING_KEYS
        .iter()
        .filter_map(|key| {
            record
                .get(*key)
                .and_then(Value::as_str)
                .map(|value| ((*key).to_string(), value.to_string()))
        })
        .collect()
}

pub fn extract_reasoning(source: &Value, explicit_key: Option<&str>) -> Option<(String, String)> {
    if let Some(explicit_key) = explicit_key {
        let value = source.get(explicit_key)?.as_str()?;
        return Some((explicit_key.to_string(), value.to_string()));
    }
    extract_reasoning_strings(source).into_iter().next()
}

pub struct ReasoningKeyDialect {
    explicit_key: Option<String>,
    detected: Mutex<Option<String>>,
}

impl Clone for ReasoningKeyDialect {
    fn clone(&self) -> Self {
        Self {
            explicit_key: self.explicit_key.clone(),
            detected: Mutex::new(self.detected.lock().clone()),
        }
    }
}

impl ReasoningKeyDialect {
    pub fn new(explicit_key: Option<String>) -> Self {
        Self {
            explicit_key,
            detected: Mutex::new(None),
        }
    }

    pub fn observe(&self, source: &Value) -> Option<String> {
        let found = extract_reasoning(source, self.explicit_key.as_deref())?;
        if self.explicit_key.is_none() && self.detected.lock().is_none() {
            *self.detected.lock() = Some(found.0.clone());
        }
        Some(found.1)
    }

    pub fn outbound_key(&self, messages: &[Message]) -> String {
        self.explicit_key
            .clone()
            .or_else(|| self.detected.lock().clone())
            .or_else(|| history_reasoning_key(messages))
            .unwrap_or_else(|| DEFAULT_REASONING_KEY.to_string())
    }
}

pub fn history_reasoning_key(messages: &[Message]) -> Option<String> {
    for message in messages.iter().rev() {
        let content = match message {
            Message::System(message) => &message.content,
            Message::User(message) => &message.content,
            Message::Assistant(message) => &message.content,
            Message::Tool(message) => &message.content,
        };
        for part in content.iter().rev() {
            if let ContentPart::Think {
                reasoning_key: Some(key),
                ..
            } = part
                && key != REASONING_DETAILS_KEY
                && KNOWN_REASONING_KEYS.contains(&key.as_str())
            {
                return Some(key.clone());
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningDetailsElement {
    pub type_: Option<String>,
    pub index: usize,
    pub summary: Option<String>,
    pub encrypted: Option<String>,
}

fn to_reasoning_details_element(value: &Value, position: usize) -> Option<ReasoningDetailsElement> {
    let record = value.as_object()?;
    let type_ = record.get("type").and_then(Value::as_str).map(str::to_string);
    if type_.as_deref().is_some_and(|type_| type_ != "summary" && type_ != "encrypted") {
        return None;
    }
    Some(ReasoningDetailsElement {
        index: record
            .get("index")
            .and_then(Value::as_u64)
            .map(|index| index as usize)
            .unwrap_or(position),
        type_,
        summary: record.get("summary").and_then(Value::as_str).map(str::to_string),
        encrypted: record.get("encrypted").and_then(Value::as_str).map(str::to_string),
    })
}

pub fn extract_reasoning_details(source: &Value) -> Option<Vec<ReasoningDetailsElement>> {
    let values = source.get(REASONING_DETAILS_KEY)?.as_array()?;
    Some(
        values
            .iter()
            .enumerate()
            .filter_map(|(position, value)| to_reasoning_details_element(value, position))
            .collect(),
    )
}

pub fn convert_reasoning_details(
    elements: &[ReasoningDetailsElement],
    hidden_summary: bool,
) -> Vec<StreamedMessagePart> {
    let mut parts = Vec::new();
    for element in elements {
        if element.type_.as_deref() != Some("encrypted")
            && element.summary.as_ref().is_some_and(|summary| !summary.is_empty())
        {
            parts.push(StreamedMessagePart::Content(ContentPart::Think {
                think: element.summary.clone().unwrap(),
                encrypted: None,
                details_index: Some(element.index),
                hidden: hidden_summary.then_some(true),
                reasoning_key: Some(REASONING_DETAILS_KEY.to_string()),
            }));
        }
        if element.type_.as_deref() != Some("summary")
            && element.encrypted.as_ref().is_some_and(|encrypted| !encrypted.is_empty())
        {
            parts.push(StreamedMessagePart::Content(ContentPart::Think {
                think: String::new(),
                encrypted: element.encrypted.clone(),
                details_index: Some(element.index),
                hidden: None,
                reasoning_key: Some(REASONING_DETAILS_KEY.to_string()),
            }));
        }
    }
    parts
}
