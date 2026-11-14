use serde::Serialize;

pub const REQUEST_INVALID: &str = "request.invalid";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentCommandInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCommandError {
    pub code: &'static str,
    pub message: String,
}

impl AgentCommandError {
    pub fn unknown_command(name: &str) -> Self {
        Self {
            code: REQUEST_INVALID,
            message: format!("Unknown command \"{name}\""),
        }
    }
}

impl std::fmt::Display for AgentCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AgentCommandError {}

pub trait AgentCommand {
    fn list(&self) -> Vec<AgentCommandInfo>;

    fn run(&self, name: &str, args: &str) -> Result<(), AgentCommandError>;
}
