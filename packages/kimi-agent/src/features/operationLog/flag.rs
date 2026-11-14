pub const OPERATION_LOG_FLAG_ID: &str = "operation_log";
pub const OPERATION_LOG_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_OPERATION_LOG";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationLogFlag {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub env: &'static str,
    pub default: bool,
    pub surface: &'static str,
}

pub const OPERATION_LOG_FLAG: OperationLogFlag = OperationLogFlag {
    id: OPERATION_LOG_FLAG_ID,
    title: "Operation Log",
    description: "Record foreground tool operations and enable repository view undo, redo, and restore.",
    env: OPERATION_LOG_FLAG_ENV,
    default: false,
    surface: "both",
};
