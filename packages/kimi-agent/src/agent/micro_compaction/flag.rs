use std::sync::LazyLock;

use crate::app::flag::flag_registry::{FlagDefinitionInput, FlagSurface, register_flag_definition};

pub const MICRO_COMPACTION_FLAG_ID: &str = "micro_compaction";
pub const MICRO_COMPACTION_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_MICRO_COMPACTION";

pub static MICRO_COMPACTION_FLAG: LazyLock<FlagDefinitionInput> = LazyLock::new(|| {
    FlagDefinitionInput {
        id: MICRO_COMPACTION_FLAG_ID.to_string(),
        title: "Micro compaction (cache-miss tool-result truncation)".to_string(),
        description: "After a prompt-cache miss, replace old oversized tool results in the outgoing request with a marker so the rebuilt prefix stays small.".to_string(),
        env: MICRO_COMPACTION_FLAG_ENV.to_string(),
        default: false,
        surface: FlagSurface::Core,
        is_exposed: None,
    }
});

pub fn register_micro_compaction_flag() {
    register_flag_definition(MICRO_COMPACTION_FLAG.clone());
}
