use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type JsonSchemaObject = Map<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseFormat {
    #[serde(rename = "json_object")]
    JsonObject,
    #[serde(rename = "json_schema", rename_all = "camelCase")]
    JsonSchema {
        json_schema: JsonSchemaResponseFormat,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonSchemaResponseFormat {
    pub name: String,
    pub schema: JsonSchemaObject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
