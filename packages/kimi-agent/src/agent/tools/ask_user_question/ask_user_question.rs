use std::collections::HashSet;
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct AskUserQuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct AskUserQuestionItem {
    pub question: String,
    #[serde(default)]
    pub header: String,
    pub options: Vec<AskUserQuestionOption>,
    #[serde(default, rename = "multi_select")]
    pub multi_select: Option<bool>,
    #[serde(default, rename = "multiSelect")]
    pub multi_select_alias: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestionInput {
    #[serde(default)]
    pub background: Option<bool>,
    pub questions: Vec<AskUserQuestionItem>,
}

pub fn question_multi_select(item: &AskUserQuestionItem) -> bool {
    item.multi_select.or(item.multi_select_alias).unwrap_or(false)
}

pub const QUESTION_UNIQUENESS_MESSAGE: &str = "Question texts must be unique across questions, and option labels must be unique within each question.";

pub fn question_uniqueness_error(questions: &[AskUserQuestionItem]) -> Option<String> {
    let mut texts: HashSet<&str> = HashSet::new();
    for question in questions {
        if !texts.insert(question.question.as_str()) {
            return Some(format!(
                "Invalid questions: duplicate question text {}. {QUESTION_UNIQUENESS_MESSAGE} Rephrase the duplicates and call the tool again.",
                serde_json::to_string(&question.question).ok()?
            ));
        }
        let mut labels: HashSet<&str> = HashSet::new();
        for option in &question.options {
            if !labels.insert(option.label.as_str()) {
                return Some(format!(
                    "Invalid questions: duplicate option label {} in question {}. {QUESTION_UNIQUENESS_MESSAGE} Rephrase the duplicates and call the tool again.",
                    serde_json::to_string(&option.label).ok()?,
                    serde_json::to_string(&question.question).ok()?
                ));
            }
        }
    }
    None
}

fn question_item_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "question": {
                "type": "string",
                "minLength": 1,
                "description": "A specific, actionable question. End with '?'."
            },
            "header": {
                "type": "string",
                "description": "Short category tag (max 12 chars, e.g. 'Auth', 'Style')."
            },
            "options": {
                "type": "array",
                "minItems": 2,
                "maxItems": 4,
                "description": "2-4 meaningful, distinct options. Do NOT include an 'Other' option — the system adds one automatically.",
                "items": {
                    "type": "object",
                    "properties": {
                        "label": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Concise display text (1-5 words). If recommended, append '(Recommended)'."
                        },
                        "description": {
                            "type": "string",
                            "description": "Brief explanation of trade-offs or implications."
                        }
                    },
                    "required": ["label"]
                }
            },
            "multi_select": {
                "type": "boolean",
                "description": "Whether the user can select multiple options."
            },
            "multiSelect": {
                "type": "boolean",
                "description": "Deprecated camelCase alias of multi_select; prefer multi_select."
            }
        },
        "required": ["question", "options"]
    })
}

pub static ASK_USER_QUESTION_INPUT_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "minItems": 1,
                "maxItems": 4,
                "description": "The questions to ask the user (1-4 questions).",
                "items": question_item_schema()
            }
        },
        "required": ["questions"]
    })
});

pub static ASK_USER_QUESTION_INPUT_SCHEMA_WITH_BACKGROUND: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "minItems": 1,
                "maxItems": 4,
                "description": "The questions to ask the user (1-4 questions).",
                "items": question_item_schema()
            },
            "background": {
                "type": "boolean",
                "default": false,
                "description": "Set true to ask in the background and return immediately with a background task_id; you are notified automatically when the user answers — do not poll with TaskOutput while the question is pending."
            }
        },
        "required": ["questions"]
    })
});
