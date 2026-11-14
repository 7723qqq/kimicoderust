use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tool_input_display::PlanReviewOption;

pub const EXIT_SPEC_MODE_TOOL_SERVICE_ID: &str = "exitSpecModeTool";
pub type ExitSpecModeOption = PlanReviewOption;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExitSpecModeInput {
    #[serde(default)]
    pub options: Option<Vec<ExitSpecModeOption>>,
}

pub fn exit_spec_mode_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "options": {
                "type": "array",
                "minItems": 1,
                "maxItems": 3,
                "items": {
                    "type": "object",
                    "properties": {
                        "label": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 80,
                            "description": "Short name for this option (1-8 words). Append \"(Recommended)\" if you recommend this option."
                        },
                        "description": {
                            "type": "string",
                            "default": "",
                            "description": "Brief summary of this approach and its trade-offs."
                        }
                    },
                    "required": ["label"],
                    "additionalProperties": false
                },
                "description": "When the design document leaves a genuine choice between approaches, list them here so the user can pick which one to execute. Provide up to 3 options; 2-3 distinct approaches work best. Each option must correspond to an approach the design document actually describes. Do not use \"Reject\", \"Revise\", \"Approve\", or \"Reject and Exit\" as labels."
            }
        },
        "additionalProperties": false
    })
}

pub fn validate_options(options: &[ExitSpecModeOption]) -> Result<(), String> {
    if options.is_empty() || options.len() > 3 {
        return Err("Expected 1-3 options.".to_string());
    }
    let mut labels = HashSet::new();
    for option in options {
        let label = normalize_option_label(&option.label);
        if option.label.is_empty() || option.label.chars().count() > 80 {
            return Err("Option label must contain 1-80 characters.".to_string());
        }
        if !labels.insert(label.clone()) {
            return Err("Option labels must be unique.".to_string());
        }
        if is_reserved_option_label(&label) {
            return Err("Option labels must not use reserved approval labels.".to_string());
        }
    }
    Ok(())
}

fn normalize_option_label(label: &str) -> String {
    label.trim().to_lowercase()
}

fn is_reserved_option_label(label: &str) -> bool {
    ["approve", "reject", "reject and exit", "revise"].contains(&label)
}
