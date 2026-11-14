use serde_json::Value;

pub fn to_input_json_schema(mut schema: Value) -> Value {
    if let Some(object) = schema.as_object_mut() {
        object.insert(
            "$schema".into(),
            Value::String("http://json-schema.org/draft-07/schema#".into()),
        );
    }
    close_object_nodes(&mut schema);
    if schema.get("type").is_none()
        && (schema.get("anyOf").is_some() || schema.get("oneOf").is_some())
    {
        if let Some(object) = schema.as_object_mut() {
            object.insert("type".into(), Value::String("object".into()));
        }
    }
    schema
}

fn close_object_nodes(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                close_object_nodes(item);
            }
        }
        Value::Object(node) => {
            if node.get("type").and_then(Value::as_str) == Some("object")
                && !node.contains_key("additionalProperties")
            {
                node.insert("additionalProperties".into(), Value::Bool(false));
            }
            for child in node.values_mut() {
                close_object_nodes(child);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_object_node_is_closed_recursively() {
        let schema = json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": { "inner": { "type": "object", "properties": {} } }
                },
                "items": {
                    "type": "array",
                    "items": { "type": "object", "properties": {} }
                }
            }
        });
        let closed = to_input_json_schema(schema);
        assert_eq!(
            closed["$schema"],
            json!("http://json-schema.org/draft-07/schema#")
        );
        assert_eq!(closed["additionalProperties"], json!(false));
        assert_eq!(
            closed["properties"]["outer"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            closed["properties"]["outer"]["properties"]["inner"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            closed["properties"]["items"]["items"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn a_root_union_becomes_an_object() {
        let closed = to_input_json_schema(json!({ "anyOf": [{ "type": "string" }] }));
        assert_eq!(closed["type"], json!("object"));
        assert_eq!(closed["anyOf"][0]["type"], json!("string"));
    }

    #[test]
    fn an_explicit_additional_properties_value_is_preserved() {
        let closed = to_input_json_schema(json!({
            "type": "object",
            "additionalProperties": { "type": "string" }
        }));
        assert_eq!(closed["additionalProperties"], json!({ "type": "string" }));
    }
}
