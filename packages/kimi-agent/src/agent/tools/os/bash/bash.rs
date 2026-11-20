use serde::{Deserialize, Serialize};

pub const DEFAULT_TIMEOUT_S: u64 = 60;
pub const MAX_TIMEOUT_S: u64 = 5 * 60;
pub const DEFAULT_BACKGROUND_TIMEOUT_S: u64 = 10 * 60;
pub const MAX_BACKGROUND_TIMEOUT_S: u64 = 24 * 60 * 60;
pub const BASH_TASK_TIMEOUT_S_DEFAULT: u64 = DEFAULT_BACKGROUND_TIMEOUT_S;

pub fn timeout_cap_s(is_background: bool) -> u64 {
    if is_background {
        MAX_BACKGROUND_TIMEOUT_S
    } else {
        MAX_TIMEOUT_S
    }
}

pub fn is_valid_timeout_value(timeout: u64, is_background: bool) -> bool {
    timeout > 0 && timeout <= timeout_cap_s(is_background)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BashInput {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_in_background: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_timeout: Option<bool>,
}

pub fn validate_bash_input(input: &BashInput) -> Result<(), String> {
    if input.command.is_empty() {
        return Err("Command cannot be empty.".to_string());
    }
    if let Some(timeout) = input.timeout {
        let is_background = input.run_in_background == Some(true);
        if !is_valid_timeout_value(timeout, is_background) {
            return Err(format!(
                "timeout must be ≤ {}s ({})",
                timeout_cap_s(is_background),
                if is_background { "background" } else { "foreground" }
            ));
        }
    }
    Ok(())
}

pub trait IBashTool: Send + Sync {}
