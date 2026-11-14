use std::sync::LazyLock;

use crate::app::flag::flag_registry::{FlagDefinitionInput, FlagSurface};

pub const WAIT_FOR_FLAG_ID: &str = "wait_for";
pub const WAIT_FOR_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_WAIT_FOR";

pub static WAIT_FOR_FLAG: LazyLock<FlagDefinitionInput> = LazyLock::new(|| FlagDefinitionInput {
    id: WAIT_FOR_FLAG_ID.to_string(),
    title: "WaitFor tool".to_string(),
    description: "Give the model the WaitFor tool so it can wait for background tasks inside the current turn instead of ending the turn and being re-invoked.".to_string(),
    env: WAIT_FOR_FLAG_ENV.to_string(),
    default: true,
    surface: FlagSurface::Core,
    is_exposed: None,
});

pub fn wait_for_flag() -> FlagDefinitionInput {
    WAIT_FOR_FLAG.clone()
}
