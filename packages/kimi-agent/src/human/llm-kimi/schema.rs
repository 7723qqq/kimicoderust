use std::collections::HashSet;
use std::sync::LazyLock;

use serde_json::{Map, Value};

use crate::human::llm::syntax_errors::SyntaxRequestFormatError;

#[derive(Clone, Copy)]
enum SchemaSlotKind {
    Single,
    Array,
    Map,
    SchemaOrArray,
}

struct ChildSchemaSlot {
    key: &'static str,
    kind: SchemaSlotKind,
    parent_type: Option<&'static str>,
}

const CHILD_SCHEMA_SLOTS: &[ChildSchemaSlot] = &[
    ChildSchemaSlot { key: "$defs", kind: SchemaSlotKind::Map, parent_type: None },
    ChildSchemaSlot { key: "definitions", kind: SchemaSlotKind::Map, parent_type: None },
    ChildSchemaSlot { key: "dependencies", kind: SchemaSlotKind::Map, parent_type: Some("object") },
    ChildSchemaSlot { key: "dependentSchemas", kind: SchemaSlotKind::Map, parent_type: Some("object") },
    ChildSchemaSlot { key: "patternProperties", kind: SchemaSlotKind::Map, parent_type: Some("object") },
    ChildSchemaSlot { key: "properties", kind: SchemaSlotKind::Map, parent_type: Some("object") },
    ChildSchemaSlot { key: "additionalItems", kind: SchemaSlotKind::Single, parent_type: Some("array") },
    ChildSchemaSlot { key: "additionalProperties", kind: SchemaSlotKind::Single, parent_type: Some("object") },
    ChildSchemaSlot { key: "contains", kind: SchemaSlotKind::Single, parent_type: Some("array") },
    ChildSchemaSlot { key: "contentSchema", kind: SchemaSlotKind::Single, parent_type: Some("string") },
    ChildSchemaSlot { key: "else", kind: SchemaSlotKind::Single, parent_type: None },
    ChildSchemaSlot { key: "if", kind: SchemaSlotKind::Single, parent_type: None },
    ChildSchemaSlot { key: "not", kind: SchemaSlotKind::Single, parent_type: None },
    ChildSchemaSlot { key: "propertyNames", kind: SchemaSlotKind::Single, parent_type: Some("object") },
    ChildSchemaSlot { key: "then", kind: SchemaSlotKind::Single, parent_type: None },
    ChildSchemaSlot { key: "unevaluatedItems", kind: SchemaSlotKind::Single, parent_type: Some("array") },
    ChildSchemaSlot { key: "unevaluatedProperties", kind: SchemaSlotKind::Single, parent_type: Some("object") },
    ChildSchemaSlot { key: "allOf", kind: SchemaSlotKind::Array, parent_type: None },
    ChildSchemaSlot { key: "anyOf", kind: SchemaSlotKind::Array, parent_type: None },
    ChildSchemaSlot { key: "oneOf", kind: SchemaSlotKind::Array, parent_type: None },
    ChildSchemaSlot { key: "prefixItems", kind: SchemaSlotKind::Array, parent_type: Some("array") },
    ChildSchemaSlot { key: "items", kind: SchemaSlotKind::SchemaOrArray, parent_type: Some("array") },
];

static TYPE_COMPLETION_SKIP_KEYS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    ["$ref", "allOf", "anyOf", "else", "if", "not", "oneOf", "then"]
        .into_iter()
        .collect()
});

static OBJECT_STRUCTURE_KEYS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    child_schema_keys_for_parent_type("object")
        .into_iter()
        .chain(["dependentRequired", "maxProperties", "minProperties", "required"])
        .collect()
});

static ARRAY_STRUCTURE_KEYS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    child_schema_keys_for_parent_type("array")
        .into_iter()
        .chain(["maxContains", "maxItems", "minContains", "minItems", "uniqueItems"])
        .collect()
});

static STRING_STRUCTURE_KEYS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    child_schema_keys_for_parent_type("string")
        .into_iter()
        .chain([
            "contentEncoding",
            "contentMediaType",
            "format",
            "maxLength",
            "minLength",
            "pattern",
        ])
        .collect()
});

static NUMERIC_STRUCTURE_KEYS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "exclusiveMaximum",
        "exclusiveMinimum",
        "maximum",
        "minimum",
        "multipleOf",
    ]
    .into_iter()
    .collect()
});

fn child_schema_keys_for_parent_type(parent_type: &str) -> Vec<&'static str> {
    CHILD_SCHEMA_SLOTS
        .iter()
        .filter(|slot| slot.parent_type == Some(parent_type))
        .map(|slot| slot.key)
        .collect()
}

fn has_any_key(obj: &Map<String, Value>, keys: &HashSet<&'static str>) -> bool {
    keys.iter().any(|key| obj.contains_key(*key))
}

fn has_unresolved_definition_ref(node: &Value, bucket_key: &str) -> bool {
    match node {
        Value::Array(items) => items
            .iter()
            .any(|child| has_unresolved_definition_ref(child, bucket_key)),
        Value::Object(obj) => {
            if obj
                .get("$ref")
                .and_then(Value::as_str)
                .is_some_and(|ref_| ref_.starts_with(&format!("#/{bucket_key}/")))
            {
                return true;
            }
            obj.iter().any(|(key, value)| {
                key != bucket_key && has_unresolved_definition_ref(value, bucket_key)
            })
        }
        _ => false,
    }
}

fn is_local_json_pointer_ref(ref_: &str) -> bool {
    ref_ == "#" || ref_.starts_with("#/")
}

fn unescape_json_pointer_part(part: &str) -> String {
    part.replace("~1", "/").replace("~0", "~")
}

fn parse_json_pointer_array_index(part: &str) -> Option<usize> {
    if part.is_empty()
        || (part.len() > 1 && part.starts_with('0'))
        || !part.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    part.parse().ok()
}

fn resolve_local_json_pointer<'a>(root: &'a Value, ref_: &str) -> Option<&'a Value> {
    if ref_ == "#" {
        return Some(root);
    }
    let mut current = root;
    for raw_part in ref_.strip_prefix("#/")?.split('/') {
        let part = unescape_json_pointer_part(raw_part);
        current = match current {
            Value::Object(obj) => obj.get(&part)?,
            Value::Array(items) => items.get(parse_json_pointer_array_index(&part)?)?,
            _ => return None,
        };
    }
    Some(current)
}

fn resolve_node(node: &Value, root: &Value, visited: &mut HashSet<String>) -> Value {
    match node {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve_node(item, root, visited))
                .collect(),
        ),
        Value::Object(obj) => {
            if let Some(ref_) = obj.get("$ref").and_then(Value::as_str) {
                if is_local_json_pointer_ref(ref_) {
                    if visited.contains(ref_) {
                        return node.clone();
                    }
                    if let Some(resolved_ref) = resolve_local_json_pointer(root, ref_) {
                        visited.insert(ref_.to_string());
                        let resolved = resolve_node(resolved_ref, root, visited);
                        visited.remove(ref_);
                        if let Value::Object(resolved) = resolved {
                            let mut merged = resolved;
                            for (key, value) in obj {
                                if key != "$ref" {
                                    merged.insert(
                                        key.clone(),
                                        resolve_node(value, root, visited),
                                    );
                                }
                            }
                            return Value::Object(merged);
                        }
                        return resolved;
                    }
                }
                return node.clone();
            }
            Value::Object(
                obj.iter()
                    .map(|(key, value)| {
                        (key.clone(), resolve_node(value, root, visited))
                    })
                    .collect(),
            )
        }
        _ => node.clone(),
    }
}

fn deref_value(schema: Map<String, Value>) -> Value {
    let root = Value::Object(schema);
    let mut visited = HashSet::new();
    resolve_node(&root, &root, &mut visited)
}

fn remove_resolved_definition_buckets(value: &mut Value) {
    if !has_unresolved_definition_ref(value, "$defs")
        && let Some(result) = value.as_object_mut()
    {
        result.remove("$defs");
    }
    if !has_unresolved_definition_ref(value, "definitions")
        && let Some(result) = value.as_object_mut()
    {
        result.remove("definitions");
    }
}

pub fn deref_json_schema(schema: Map<String, Value>) -> Map<String, Value> {
    let mut value = deref_value(schema);
    remove_resolved_definition_buckets(&mut value);
    value.as_object().cloned().unwrap_or_else(|| {
        panic!("JSON Schema root must normalize to an object.")
    })
}

fn infer_value_type(value: &Value) -> Option<&'static str> {
    match value {
        Value::Null => Some("null"),
        Value::Array(_) => Some("array"),
        Value::String(_) => Some("string"),
        Value::Bool(_) => Some("boolean"),
        Value::Object(_) => Some("object"),
        Value::Number(number) => {
            if number.as_i64().is_some()
                || number.as_u64().is_some()
                || number
                    .as_f64()
                    .is_some_and(|number| number.is_finite() && number.fract() == 0.0)
            {
                Some("integer")
            } else {
                Some("number")
            }
        }
    }
}

fn normalize_inferred_types(types: HashSet<&'static str>) -> Vec<&'static str> {
    let mut normalized = types;
    if normalized.contains("number") {
        normalized.remove("integer");
    }
    ["string", "number", "integer", "boolean", "object", "array", "null"]
        .into_iter()
        .filter(|type_| normalized.contains(type_))
        .collect()
}

fn infer_type_from_values(values: &[Value]) -> Result<&'static str, SyntaxRequestFormatError> {
    let mut inferred = HashSet::new();
    for value in values {
        let Some(value_type) = infer_value_type(value) else {
            return Err(SyntaxRequestFormatError::new(
                "Cannot infer JSON Schema type from non-JSON enum or const value.",
            ));
        };
        inferred.insert(value_type);
    }
    let types = normalize_inferred_types(inferred);
    if types.len() == 1 {
        return types.first().copied().ok_or_else(|| {
            SyntaxRequestFormatError::new("Cannot infer JSON Schema type from an empty enum.")
        });
    }
    if types.is_empty() {
        return Err(SyntaxRequestFormatError::new(
            "Cannot infer JSON Schema type from an empty enum.",
        ));
    }
    Err(SyntaxRequestFormatError::new(
        "Mixed JSON Schema enum or const types are not supported by Kimi tool schemas.",
    ))
}

fn infer_type_from_structure(schema: &Map<String, Value>) -> &'static str {
    if has_any_key(schema, &OBJECT_STRUCTURE_KEYS) {
        return "object";
    }
    if has_any_key(schema, &ARRAY_STRUCTURE_KEYS) {
        return "array";
    }
    if has_any_key(schema, &STRING_STRUCTURE_KEYS) {
        return "string";
    }
    if has_any_key(schema, &NUMERIC_STRUCTURE_KEYS) {
        return "number";
    }
    "string"
}

fn remove_irrelevant_structure_keys(node: &mut Map<String, Value>, new_type: &str) {
    if new_type != "object" {
        for key in OBJECT_STRUCTURE_KEYS.iter() {
            node.remove(*key);
        }
    }
    if new_type != "array" {
        for key in ARRAY_STRUCTURE_KEYS.iter() {
            node.remove(*key);
        }
    }
}

fn visit_child_schemas(
    node: &mut Map<String, Value>,
    visit: &mut dyn FnMut(&mut Value) -> Result<(), SyntaxRequestFormatError>,
) -> Result<(), SyntaxRequestFormatError> {
    for slot in CHILD_SCHEMA_SLOTS {
        let Some(value) = node.get_mut(slot.key) else {
            continue;
        };
        match slot.kind {
            SchemaSlotKind::Single => {
                if value.is_object() {
                    visit(value)?;
                }
            }
            SchemaSlotKind::Array => {
                if let Value::Array(items) = value {
                    for item in items {
                        visit(item)?;
                    }
                }
            }
            SchemaSlotKind::Map => {
                if let Value::Object(items) = value {
                    for item in items.values_mut() {
                        visit(item)?;
                    }
                }
            }
            SchemaSlotKind::SchemaOrArray => match value {
                Value::Object(_) => visit(value)?,
                Value::Array(items) => {
                    for item in items {
                        visit(item)?;
                    }
                }
                _ => {}
            },
        }
    }
    Ok(())
}

fn recurse_schema(node: &mut Map<String, Value>) -> Result<(), SyntaxRequestFormatError> {
    visit_child_schemas(node, &mut normalize_property)
}

fn normalize_property(node: &mut Value) -> Result<(), SyntaxRequestFormatError> {
    let Some(node) = node.as_object_mut() else {
        return Ok(());
    };
    let skips_type_completion = has_any_key(node, &TYPE_COMPLETION_SKIP_KEYS);
    if !node.contains_key("type") && !skips_type_completion {
        let enum_values = node.get("enum").and_then(Value::as_array).filter(|values| !values.is_empty());
        let inferred = if let Some(values) = enum_values {
            infer_type_from_values(values)?
        } else if let Some(const_value) = node.get("const") {
            infer_type_from_values(std::slice::from_ref(const_value))?
        } else {
            infer_type_from_structure(node)
        };
        node.insert("type".to_string(), Value::String(inferred.to_string()));
    } else if !skips_type_completion && node.get("type").and_then(Value::as_str).is_some() {
        let values = node
            .get("enum")
            .and_then(Value::as_array)
            .filter(|values| !values.is_empty())
            .cloned()
            .or_else(|| node.get("const").cloned().map(|value| vec![value]));
        if let Some(values) = values
            && let Ok(inferred) = infer_type_from_values(&values)
            && node.get("type").and_then(Value::as_str) != Some(inferred)
        {
            node.insert("type".to_string(), Value::String(inferred.to_string()));
            remove_irrelevant_structure_keys(node, inferred);
        }
    }
    recurse_schema(node)
}

fn ensure_kimi_property_types(value: Value) -> Result<Map<String, Value>, SyntaxRequestFormatError> {
    let mut normalized = value;
    let Some(root) = normalized.as_object_mut() else {
        return Err(SyntaxRequestFormatError::new(
            "JSON Schema root must normalize to an object.",
        ));
    };
    recurse_schema(root)?;
    Ok(root.clone())
}

pub fn normalize_kimi_tool_schema(schema: Map<String, Value>) -> Map<String, Value> {
    let mut value = deref_value(schema);
    remove_resolved_definition_buckets(&mut value);
    ensure_kimi_property_types(value).unwrap_or_else(|error| panic!("{error}"))
}
