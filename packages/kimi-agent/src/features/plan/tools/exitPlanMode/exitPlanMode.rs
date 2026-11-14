use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::tool_input_display::PlanReviewOption;

pub const EXIT_PLAN_MODE_TOOL_NAME: &str = "ExitPlanMode";
pub const EXIT_PLAN_MODE_TOOL_SERVICE_ID: &str = "exitPlanModeTool";
pub type ExitPlanModeOption = PlanReviewOption;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExitPlanModeInput {
    #[serde(default, deserialize_with = "deserialize_options")]
    pub options: Option<Vec<ExitPlanModeOption>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitPlanModePlanSource {
    pub plan: String,
    pub path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionInput {
    label: String,
    #[serde(default)]
    description: String,
}

fn deserialize_options<'de, D>(deserializer: D) -> Result<Option<Vec<ExitPlanModeOption>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let options = Vec::<OptionInput>::deserialize(deserializer)?;
    let options = options
        .into_iter()
        .map(|option| ExitPlanModeOption {
            label: option.label,
            description: option.description,
        })
        .collect::<Vec<_>>();
    validate_options(&options).map_err(serde::de::Error::custom)?;
    Ok(Some(options))
}

pub fn validate_options(options: &[ExitPlanModeOption]) -> Result<(), String> {
    if options.is_empty() || options.len() > 3 {
        return Err("Expected 1-3 options.".to_string());
    }
    let mut labels = HashSet::new();
    for option in options {
        let label = trim_js(&option.label).to_lowercase();
        if option.label.is_empty() || option.label.encode_utf16().count() > 80 {
            return Err("Option label must contain 1-80 characters.".to_string());
        }
        if !labels.insert(label.clone()) {
            return Err("Option labels must be unique.".to_string());
        }
        if ["approve", "reject", "reject and exit", "revise"].contains(&label.as_str()) {
            return Err("Option labels must not use reserved approval labels.".to_string());
        }
    }
    Ok(())
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}
