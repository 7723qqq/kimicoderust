use serde::{Deserialize, Serialize};

pub use crate::native::powershell::{
    POWER_SHELL_UTF8_PREAMBLE as POWERSHELL_UTF8_PREAMBLE, PowerShellExecutable,
    PowerShellKind as PowerShellExecutableKind, ResolveDeps as PowerShellResolveDeps,
    build_args as build_power_shell_args, encode_script as encode_power_shell_script,
    normalize_output as normalize_power_shell_output, resolve as resolve_power_shell_executable,
};

pub use super::super::bash::bash::{MAX_BACKGROUND_TIMEOUT_S, MAX_TIMEOUT_S};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PowerShellInput {
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

pub fn validate_power_shell_input(input: &PowerShellInput) -> Result<(), String> {
    if input.command.is_empty() {
        return Err("Command cannot be empty.".to_string());
    }
    if let Some(timeout) = input.timeout {
        let max_seconds = if input.run_in_background == Some(true) {
            MAX_BACKGROUND_TIMEOUT_S
        } else {
            MAX_TIMEOUT_S
        };
        if timeout == 0 || timeout > max_seconds {
            return Err(format!("timeout must be ≤ {max_seconds}s"));
        }
    }
    Ok(())
}

pub trait IPowerShellTool: Send + Sync {}
