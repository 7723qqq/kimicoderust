use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SET_GOAL_BUDGET_TOOL_SERVICE_ID: &str = "setGoalBudgetTool";
pub const BUDGET_UNITS: &[&str] = &["turns", "tokens", "milliseconds", "seconds", "minutes", "hours"];

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetGoalBudgetToolInput {
    #[serde(deserialize_with = "deserialize_positive_value")]
    pub value: f64,
    #[serde(deserialize_with = "deserialize_unit")]
    pub unit: String,
}

fn deserialize_positive_value<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    if value.is_nan() || value <= 0.0 {
        return Err(serde::de::Error::custom("value must be positive"));
    }
    Ok(value)
}

fn deserialize_unit<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let unit = String::deserialize(deserializer)?;
    if !BUDGET_UNITS.contains(&unit.as_str()) {
        return Err(serde::de::Error::custom("unknown budget unit"));
    }
    Ok(unit)
}

pub fn set_goal_budget_input_schema() -> Value {
    crate::tools::goal_tools::set_goal_budget_tool_def().input_schema
}
