use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CRON_CREATE_TOOL_SERVICE_ID: &str = "cronCreateTool";
pub const MAX_CRON_JOBS_PER_SESSION: usize = 50;
pub const MAX_PROMPT_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CronCreateInput {
    pub cron: String,
    pub prompt: String,
    #[serde(default = "default_recurring")]
    pub recurring: bool,
}

fn default_recurring() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CronCreateOutput {
    pub id: String,
    pub cron: String,
    pub human_schedule: String,
    pub recurring: bool,
    pub next_fire_at: Option<i64>,
}

pub fn cron_create_input_schema() -> Value {
    let mut schema = crate::tools::cron_tools::cron_create_tool_def().input_schema;
    if let Some(recurring) = schema
        .pointer_mut("/properties/recurring")
        .and_then(Value::as_object_mut)
    {
        recurring.insert("default".to_string(), Value::Bool(true));
    }
    schema
}
