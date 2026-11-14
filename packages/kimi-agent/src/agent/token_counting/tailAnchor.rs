use serde::{Deserialize, Serialize};

pub const TAIL_CONTRACT_MAX_CHARS: usize = 1200;

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{9}'..='\u{D}'
            | '\u{20}'
            | '\u{A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailAnchorInput {
    pub estimated_tokens: i64,
    pub threshold: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_anchor_tokens: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailContractInput {
    pub goal: String,
    pub constraints: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailContractPlanInput {
    #[serde(flatten)]
    pub anchor: TailAnchorInput,
    #[serde(flatten)]
    pub contract: TailContractInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_injected: Option<String>,
}

pub fn should_reanchor_tail(input: &TailAnchorInput) -> bool {
    if input.threshold <= 0 {
        return false;
    }
    input.estimated_tokens as f64 - input.last_anchor_tokens.unwrap_or(0) as f64
        >= input.threshold as f64
}

pub fn render_tail_contract(input: &TailContractInput) -> String {
    let mut lines = vec![format!("Objective: {}", js_trim(&input.goal))];
    if !input.constraints.is_empty() {
        lines.push("Hard constraints:".to_string());
        for constraint in &input.constraints {
            lines.push(format!("- {}", js_trim(constraint)));
        }
    }
    let text = js_trim(&lines.join("\n")).to_string();
    if text.encode_utf16().count() > TAIL_CONTRACT_MAX_CHARS {
        let units = text.encode_utf16().take(TAIL_CONTRACT_MAX_CHARS).collect::<Vec<_>>();
        String::from_utf16_lossy(&units)
            .trim_end_matches(is_js_whitespace)
            .to_string()
    } else {
        text
    }
}

pub fn plan_tail_contract(input: &TailContractPlanInput) -> Option<String> {
    if input
        .last_injected
        .as_deref()
        .is_some_and(|last| !js_trim(last).is_empty())
    {
        return None;
    }
    if !should_reanchor_tail(&input.anchor) {
        return None;
    }
    Some(render_tail_contract(&input.contract))
}
