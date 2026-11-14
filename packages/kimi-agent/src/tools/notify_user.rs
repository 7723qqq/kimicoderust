pub use crate::tools::core_tool_defs::{NOTIFY_USER_TOOL_NAME, notify_user_tool_def};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotifyUserInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub message: String,
}
