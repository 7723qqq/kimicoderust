use serde_json::{Value, json};

use super::specs::{Field, FieldKind, GitHubToolSpec, TREE_ENTRY_FIELDS};

pub(super) fn schema_for(spec: &GitHubToolSpec) -> Value {
    let mut properties = serde_json::Map::new();
    for field in spec.fields {
        properties.insert(field.name.to_string(), field_schema(field));
    }
    let mut schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    let required: Vec<&str> = spec
        .fields
        .iter()
        .filter(|f| f.required)
        .map(|f| f.name)
        .collect();
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

fn field_schema(field: &Field) -> Value {
    let mut schema = match field.kind {
        FieldKind::Str | FieldKind::StrAny => json!({ "type": "string" }),
        FieldKind::Int => json!({ "type": "integer" }),
        FieldKind::Bool => json!({ "type": "boolean" }),
        FieldKind::StrArray => json!({ "type": "array", "items": { "type": "string" } }),
        FieldKind::TreeArray => json!({
            "type": "array",
            "items": object_schema(TREE_ENTRY_FIELDS),
        }),
    };
    match field.kind {
        FieldKind::Str => {
            if let Some(min) = field.min {
                schema["minLength"] = json!(min);
            }
        }
        FieldKind::Int => {
            schema["minimum"] = json!(field.min.unwrap_or(-9007199254740991));
            schema["maximum"] = json!(field.max.unwrap_or(9007199254740991));
        }
        FieldKind::StrArray | FieldKind::TreeArray => {
            if let Some(min) = field.min {
                schema["minItems"] = json!(min);
            }
        }
        FieldKind::StrAny | FieldKind::Bool => {}
    }
    if !field.enum_values.is_empty() {
        schema["enum"] = json!(field.enum_values);
    }
    if !field.description.is_empty() {
        schema["description"] = json!(field.description);
    }
    schema
}

fn object_schema(fields: &[Field]) -> Value {
    let mut properties = serde_json::Map::new();
    for field in fields {
        properties.insert(field.name.to_string(), field_schema(field));
    }
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    let required: Vec<&str> = fields
        .iter()
        .filter(|f| f.required)
        .map(|f| f.name)
        .collect();
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}
