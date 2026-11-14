use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CRON_DELETE_TOOL_SERVICE_ID: &str = "cronDeleteTool";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CronDeleteInput {
    pub id: String,
}

pub fn cron_delete_input_schema() -> Value {
    crate::tools::cron_tools::cron_delete_tool_def().input_schema
}
