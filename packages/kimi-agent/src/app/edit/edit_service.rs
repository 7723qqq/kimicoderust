use serde::{Deserialize, Serialize};

use super::text_model::TextModel;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditApplyInput {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    pub replace_all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ok")]
pub enum EditApplyResult {
    #[serde(rename = "true")]
    Ok {
        #[serde(rename = "rawContent")]
        raw_content: String,
        count: usize,
    },
    #[serde(rename = "false")]
    Err { error: String },
}

#[derive(Default)]
pub struct EditService;

impl EditService {
    pub fn new() -> Self {
        Self
    }

    pub fn apply(&self, model: &TextModel, input: &EditApplyInput) -> EditApplyResult {
        if input.replace_all {
            let result = model.replace_all(&input.old_string, &input.new_string);
            if result.count == 0 {
                return EditApplyResult::Err {
                    error: not_found_message(&input.path),
                };
            }
            return EditApplyResult::Ok {
                raw_content: model.materialize(&result.text),
                count: result.count,
            };
        }

        let count = model.count_occurrences(&input.old_string);
        if count == 0 {
            return EditApplyResult::Err {
                error: not_found_message(&input.path),
            };
        }
        if count > 1 {
            return EditApplyResult::Err {
                error: not_unique_message(&input.path, count),
            };
        }

        let text = model.replace_once(&input.old_string, &input.new_string);
        EditApplyResult::Ok {
            raw_content: model.materialize(&text),
            count: 1,
        }
    }
}

fn not_found_message(path: &str) -> String {
    format!(
        "old_string not found in {path}, the file contents may be out of date. Please use the Read Tool to reload the content.\n"
    )
}

fn not_unique_message(path: &str, count: usize) -> String {
    format!(
        "old_string is not unique in {path} (found {count} occurrences). To replace every occurrence, set replace_all=true. To replace only one occurrence, include more surrounding context in old_string."
    )
}
