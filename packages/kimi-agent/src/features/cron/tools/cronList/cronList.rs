use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CRON_LIST_TOOL_SERVICE_ID: &str = "cronListTool";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CronListInput {}

pub fn cron_list_input_schema() -> Value {
    crate::tools::cron_tools::cron_list_tool_def().input_schema
}
