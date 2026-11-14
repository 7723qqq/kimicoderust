use serde::{Deserialize, Serialize};

pub use crate::compaction::*;

const COMPACTION_INSTRUCTION_TEMPLATE: &str = include_str!("../../compaction/compaction-instruction.md");

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionInstructionInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instruction: Option<String>,
}

pub fn render_compaction_instruction(input: CompactionInstructionInput) -> String {
    let custom = input
        .custom_instruction
        .as_deref()
        .map(str::trim)
        .filter(|custom| !custom.is_empty())
        .map(|custom| format!("\nOptional user instruction:\n{custom}\n"))
        .unwrap_or_default();
    COMPACTION_INSTRUCTION_TEMPLATE
        .replace("${custom_instruction_block}", &custom)
        .trim_end()
        .to_string()
}
