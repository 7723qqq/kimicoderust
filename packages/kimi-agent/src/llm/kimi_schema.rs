use serde_json::{Map, Value};
use std::collections::HashSet;

type Schema = Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum JsonSchemaType {
    String,
    Number,
    Integer,
    Boolean,
    Object,
    Array,
    Null,
}

impl JsonSchemaType {
    fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Object => "object",
            Self::Array => "array",
            Self::Null => "null",
        }
    }
}

const TYPE_COMPLETION_SKIP_KEYS: &[&str] = &[
    "$ref", "allOf", "anyOf", "else", "if", "not", "oneOf", "then",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Single,
    Array,
    Map,
    SchemaOrArray,
}

const CHILD_SCHEMA_SLOTS: &[(&str, SlotKind)] = &[
    ("$defs", SlotKind::Map),
    ("definitions", SlotKind::Map),
    ("dependencies", SlotKind::Map),
    ("dependentSchemas", SlotKind::Map),
    ("patternProperties", SlotKind::Map),
    ("properties", SlotKind::Map),
    ("additionalItems", SlotKind::Single),
    ("additionalProperties", SlotKind::Single),
    ("contains", SlotKind::Single),
    ("contentSchema", SlotKind::Single),
    ("else", SlotKind::Single),
    ("if", SlotKind::Single),
    ("not", SlotKind::Single),
    ("propertyNames", SlotKind::Single),
    ("then", SlotKind::Single),
    ("unevaluatedItems", SlotKind::Single),
    ("unevaluatedProperties", SlotKind::Single),
    ("allOf", SlotKind::Array),
    ("anyOf", SlotKind::Array),
    ("oneOf", SlotKind::Array),
    ("prefixItems", SlotKind::Array),
    ("items", SlotKind::SchemaOrArray),
];

const OBJECT_STRUCTURE_KEYS: &[&str] = &[
    "dependencies",
    "dependentRequired",
    "dependentSchemas",
    "maxProperties",
    "minProperties",
    "patternProperties",
    "properties",
    "propertyNames",
    "required",
];

const ARRAY_STRUCTURE_KEYS: &[&str] = &[
    "additionalItems",
    "contains",
    "items",
    "maxContains",
    "maxItems",
    "minContains",
    "minItems",
    "prefixItems",
    "unevaluatedItems",
    "uniqueItems",
];

const STRING_STRUCTURE_KEYS: &[&str] = &[
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
    "format",
    "maxLength",
    "minLength",
    "pattern",
];

const NUMERIC_STRUCTURE_KEYS: &[&str] = &[
    "exclusiveMaximum",
    "exclusiveMinimum",
    "maximum",
    "minimum",
    "multipleOf",
];

pub fn deref_json_schema(schema: &Schema) -> Schema {
    let root = Value::Object(schema.clone());
    let mut visited = HashSet::new();
    let resolved = resolve_node(&root, &root, &mut visited);
    let mut map = match resolved {
        Value::Object(map) => map,
        _ => schema.clone(),
    };
    if !has_unresolved_definition_ref(&Value::Object(map.clone()), "$defs") {
        map.remove("$defs");
    }
    if !has_unresolved_definition_ref(&Value::Object(map.clone()), "definitions") {
        map.remove("definitions");
    }
    map
}

pub fn normalize_kimi_tool_schema(value: &Value) -> Result<Value, String> {
    let mut map = match value {
        Value::Object(map) => deref_json_schema(map),
        _ => return Err("JSON Schema root must normalize to an object.".to_string()),
    };
    recurse_schema(&mut map)?;
    Ok(Value::Object(map))
}

fn has_unresolved_definition_ref(node: &Value, bucket_key: &str) -> bool {
    match node {
        Value::Array(items) => items
            .iter()
            .any(|child| has_unresolved_definition_ref(child, bucket_key)),
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref")
                && reference.starts_with(&format!("#/{bucket_key}/"))
            {
                return true;
            }
            for (key, value) in map {
                if key == bucket_key {
                    continue;
                }
                if has_unresolved_definition_ref(value, bucket_key) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn resolve_node(node: &Value, root: &Value, visited: &mut HashSet<String>) -> Value {
    match node {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve_node(item, root, visited))
                .collect(),
        ),
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref")
                && is_local_json_pointer_ref(reference)
            {
                if visited.contains(reference) {
                    return node.clone();
                }
                if let Some(target) = resolve_local_json_pointer(root, reference) {
                    visited.insert(reference.clone());
                    let resolved = resolve_node(&target, root, visited);
                    visited.remove(reference);
                    if let Value::Object(mut merged) = resolved {
                        for (key, value) in map {
                            if key == "$ref" {
                                continue;
                            }
                            merged.insert(key.clone(), resolve_node(value, root, visited));
                        }
                        return Value::Object(merged);
                    }
                    return resolved;
                }
                return node.clone();
            }
            let mut resolved = Schema::new();
            for (key, value) in map {
                resolved.insert(key.clone(), resolve_node(value, root, visited));
            }
            Value::Object(resolved)
        }
        _ => node.clone(),
    }
}

fn is_local_json_pointer_ref(reference: &str) -> bool {
    reference == "#" || reference.starts_with("#/")
}

fn resolve_local_json_pointer(root: &Value, reference: &str) -> Option<Value> {
    if reference == "#" {
        return Some(root.clone());
    }
    let mut current = root.clone();
    for raw_part in reference[2..].split('/') {
        let part = unescape_json_pointer_part(raw_part);
        match current {
            Value::Object(ref map) => {
                if !map.contains_key(&part) {
                    return None;
                }
                current = map.get(&part)?.clone();
            }
            Value::Array(ref items) => {
                let index = parse_json_pointer_array_index(&part)?;
                if index >= items.len() {
                    return None;
                }
                current = items[index].clone();
            }
            _ => return None,
        }
    }
    Some(current)
}

fn unescape_json_pointer_part(part: &str) -> String {
    part.replace("~1", "/").replace("~0", "~")
}

fn parse_json_pointer_array_index(part: &str) -> Option<usize> {
    let bytes = part.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes.len() > 1 && bytes[0] == b'0' {
        return None;
    }
    part.parse().ok()
}

fn recurse_schema(node: &mut Schema) -> Result<(), String> {
    visit_child_schemas(node, &mut normalize_property)
}

fn visit_child_schemas(
    node: &mut Schema,
    visit: &mut dyn FnMut(&mut Schema) -> Result<(), String>,
) -> Result<(), String> {
    for (key, kind) in CHILD_SCHEMA_SLOTS {
        match kind {
            SlotKind::Single => {
                if let Some(Value::Object(child)) = node.get_mut(*key) {
                    visit(child)?;
                }
            }
            SlotKind::Array => {
                if let Some(Value::Array(items)) = node.get_mut(*key) {
                    for item in items.iter_mut() {
                        if let Value::Object(child) = item {
                            visit(child)?;
                        }
                    }
                }
            }
            SlotKind::Map => {
                if let Some(Value::Object(map)) = node.get_mut(*key) {
                    for item in map.values_mut() {
                        if let Value::Object(child) = item {
                            visit(child)?;
                        }
                    }
                }
            }
            SlotKind::SchemaOrArray => match node.get_mut(*key) {
                Some(Value::Object(child)) => visit(child)?,
                Some(Value::Array(items)) => {
                    for item in items.iter_mut() {
                        if let Value::Object(child) = item {
                            visit(child)?;
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(())
}

fn normalize_property(node: &mut Schema) -> Result<(), String> {
    let has_type = node.contains_key("type");
    let has_skip_key = has_any_key(node, TYPE_COMPLETION_SKIP_KEYS);

    if !has_type && !has_skip_key {
        let enum_values = node
            .get("enum")
            .and_then(Value::as_array)
            .filter(|values| !values.is_empty())
            .cloned();
        if let Some(values) = enum_values {
            let inferred = infer_type_from_values(&values)?;
            node.insert(
                "type".to_string(),
                Value::String(inferred.as_str().to_string()),
            );
        } else if let Some(const_value) = node.get("const").cloned() {
            let inferred = infer_type_from_values(&[const_value])?;
            node.insert(
                "type".to_string(),
                Value::String(inferred.as_str().to_string()),
            );
        } else {
            let inferred = infer_type_from_structure(node);
            node.insert(
                "type".to_string(),
                Value::String(inferred.as_str().to_string()),
            );
        }
    } else if !has_skip_key && let Some(Value::String(declared)) = node.get("type").cloned() {
        let enum_values = node
            .get("enum")
            .and_then(Value::as_array)
            .filter(|values| !values.is_empty())
            .cloned();
        let overridden = if let Some(values) = enum_values {
            Some(infer_type_from_values(&values)?)
        } else if let Some(const_value) = node.get("const").cloned() {
            Some(infer_type_from_values(&[const_value])?)
        } else {
            None
        };
        if let Some(inferred) = overridden
            && declared != inferred.as_str()
        {
            node.insert(
                "type".to_string(),
                Value::String(inferred.as_str().to_string()),
            );
            remove_irrelevant_structure_keys(node, inferred);
        }
    }

    recurse_schema(node)
}

fn remove_irrelevant_structure_keys(node: &mut Schema, new_type: JsonSchemaType) {
    if new_type != JsonSchemaType::Object {
        for key in OBJECT_STRUCTURE_KEYS {
            node.remove(*key);
        }
    }
    if new_type != JsonSchemaType::Array {
        for key in ARRAY_STRUCTURE_KEYS {
            node.remove(*key);
        }
    }
}

fn infer_type_from_structure(schema: &Schema) -> JsonSchemaType {
    if has_any_key(schema, OBJECT_STRUCTURE_KEYS) {
        return JsonSchemaType::Object;
    }
    if has_any_key(schema, ARRAY_STRUCTURE_KEYS) {
        return JsonSchemaType::Array;
    }
    if has_any_key(schema, STRING_STRUCTURE_KEYS) {
        return JsonSchemaType::String;
    }
    if has_any_key(schema, NUMERIC_STRUCTURE_KEYS) {
        return JsonSchemaType::Number;
    }
    JsonSchemaType::String
}

fn infer_type_from_values(values: &[Value]) -> Result<JsonSchemaType, String> {
    let mut inferred = HashSet::new();
    for value in values {
        let value_type = infer_value_type(value).ok_or_else(|| {
            "Cannot infer JSON Schema type from non-JSON enum or const value.".to_string()
        })?;
        inferred.insert(value_type);
    }
    let types = normalize_inferred_types(&inferred);
    if types.len() == 1 {
        return Ok(types[0]);
    }
    Err("Mixed JSON Schema enum or const types are not supported by Kimi tool schemas.".to_string())
}

fn infer_value_type(value: &Value) -> Option<JsonSchemaType> {
    match value {
        Value::Null => Some(JsonSchemaType::Null),
        Value::Array(_) => Some(JsonSchemaType::Array),
        Value::String(_) => Some(JsonSchemaType::String),
        Value::Number(number) => Some(if is_integer(number) {
            JsonSchemaType::Integer
        } else {
            JsonSchemaType::Number
        }),
        Value::Bool(_) => Some(JsonSchemaType::Boolean),
        Value::Object(_) => Some(JsonSchemaType::Object),
    }
}

fn is_integer(number: &serde_json::Number) -> bool {
    if number.is_i64() || number.is_u64() {
        return true;
    }
    number.as_f64().is_some_and(|value| value.fract() == 0.0)
}

fn normalize_inferred_types(types: &HashSet<JsonSchemaType>) -> Vec<JsonSchemaType> {
    let mut normalized = types.clone();
    if normalized.contains(&JsonSchemaType::Number) {
        normalized.remove(&JsonSchemaType::Integer);
    }
    [
        JsonSchemaType::String,
        JsonSchemaType::Number,
        JsonSchemaType::Integer,
        JsonSchemaType::Boolean,
        JsonSchemaType::Object,
        JsonSchemaType::Array,
        JsonSchemaType::Null,
    ]
    .into_iter()
    .filter(|candidate| normalized.contains(candidate))
    .collect()
}

fn has_any_key(schema: &Schema, keys: &[&str]) -> bool {
    keys.iter().any(|key| schema.contains_key(*key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dereferences_a_local_ref_and_merges_sibling_keys() {
        let schema = serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "name": { "$ref": "#/$defs/Name", "description": "who" }
            },
            "$defs": { "Name": { "type": "string" } }
        }))
        .unwrap();
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        let properties = normalized
            .get("properties")
            .and_then(Value::as_object)
            .unwrap();
        let name = properties.get("name").and_then(Value::as_object).unwrap();
        assert_eq!(name.get("type").and_then(Value::as_str), Some("string"));
        assert_eq!(name.get("description").and_then(Value::as_str), Some("who"));
        assert!(normalized.get("$defs").is_none());
    }

    #[test]
    fn drops_a_definition_bucket_once_no_ref_points_at_it() {
        let schema: Value = serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "a": { "$ref": "#/$defs/Name" },
                "b": { "$ref": "#/$defs/Name" }
            },
            "required": ["a"],
            "$defs": { "Name": { "type": "string" } }
        }))
        .unwrap();
        let normalized = deref_json_schema(schema.as_object().unwrap());
        assert!(normalized.get("$defs").is_none());
        let properties = normalized
            .get("properties")
            .and_then(Value::as_object)
            .unwrap();
        assert!(
            properties
                .get("a")
                .and_then(Value::as_object)
                .unwrap()
                .get("$ref")
                .is_none()
        );
    }

    #[test]
    fn infers_type_from_enum_const_and_structure() {
        let schema = serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "color": { "enum": ["red", "green"] },
                "count": { "const": 3 },
                "nested": { "properties": { "x": { "type": "number" } } },
                "tags": { "items": { "type": "string" } }
            }
        }))
        .unwrap();
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        let properties = normalized
            .get("properties")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            properties
                .get("color")
                .and_then(Value::as_object)
                .and_then(|node| node.get("type"))
                .and_then(Value::as_str),
            Some("string")
        );
        assert_eq!(
            properties
                .get("count")
                .and_then(Value::as_object)
                .and_then(|node| node.get("type"))
                .and_then(Value::as_str),
            Some("integer")
        );
        assert_eq!(
            properties
                .get("nested")
                .and_then(Value::as_object)
                .and_then(|node| node.get("type"))
                .and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(
            properties
                .get("tags")
                .and_then(Value::as_object)
                .and_then(|node| node.get("type"))
                .and_then(Value::as_str),
            Some("array")
        );
    }

    #[test]
    fn an_enum_overrides_a_wrong_declared_type_and_drops_object_keys() {
        let schema = serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "flag": {
                    "type": "string",
                    "enum": [true, false],
                    "properties": { "x": { "type": "string" } }
                }
            }
        }))
        .unwrap();
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        let flag = normalized
            .get("properties")
            .and_then(Value::as_object)
            .unwrap()
            .get("flag")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(flag.get("type").and_then(Value::as_str), Some("boolean"));
        assert!(flag.get("properties").is_none());
    }

    #[test]
    fn mixed_enum_types_are_refused() {
        let schema = serde_json::from_value(json!({
            "type": "object",
            "properties": { "mixed": { "enum": ["a", 1] } }
        }))
        .unwrap();
        let error = normalize_kimi_tool_schema(&schema).unwrap_err();
        assert!(error.contains("Mixed JSON Schema enum or const types"));
    }

    #[test]
    fn a_non_object_root_is_refused() {
        let error = normalize_kimi_tool_schema(&json!(["not", "an", "object"])).unwrap_err();
        assert_eq!(error, "JSON Schema root must normalize to an object.");
    }

    #[test]
    fn a_circular_ref_resolves_without_recursing_forever() {
        let schema = serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "node": { "$ref": "#/$defs/Node" }
            },
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": { "child": { "$ref": "#/$defs/Node" } }
                }
            }
        }))
        .unwrap();
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        assert!(normalized.get("$defs").is_some());
    }
}
