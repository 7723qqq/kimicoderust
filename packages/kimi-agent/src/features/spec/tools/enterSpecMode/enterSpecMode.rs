use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const ENTER_SPEC_MODE_TOOL_SERVICE_ID: &str = "enterSpecModeTool";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EnterSpecModeInput {}

pub fn enter_spec_mode_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}
