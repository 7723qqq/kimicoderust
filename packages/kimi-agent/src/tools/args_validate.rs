//! v2 `tool/args-validator.ts`: the JSON-Schema check a tool call must pass
//! before it runs.
//!
//! v2 compiles one Ajv validator per tool (`compileToolArgsValidator`) and
//! reports every failure as `Invalid args for tool "<name>": <detail>` with the
//! details joined by `; `. This engine carries no Ajv, so the validator here
//! covers the keyword set the engine's own tool schemas and MCP servers use and
//! ignores the rest — v2 runs Ajv with `strict: false`, where an unknown keyword
//! is not an error either.
//!
//! Message text follows Ajv's vocabulary (`must be string`, `must have required
//! property 'path'`, `must NOT have additional property 'x'`) so a model that
//! learned the v2 wording reads the same sentence here.
//!
//! Two deliberate differences, both recorded rather than hidden:
//!
//! - `anyOf` / `oneOf` / `allOf` report the branch verdict, not Ajv's nested
//!   per-branch errors. The actionable fact is which combinator failed; the
//!   nested trace doubles the length of a message the model has to parse.
//! - A `$ref` that does not resolve inside the same schema (an external URL) is
//!   skipped instead of rejected: the engine cannot fetch it, and refusing a
//!   call over a reference the host never sent would be a false negative.

use serde_json::Value;

/// The cap on how deeply a `$ref` chain may nest before the schema is treated
/// as unvalidatable. A self-referential schema is legal; an unbounded walk is
/// not.
const MAX_DEPTH: usize = 64;

/// Validate `value` against `schema`, returning the Ajv-shaped failure list.
///
/// `Ok(())` means every keyword this module understands was satisfied. A
/// keyword it does not understand is ignored, which is why an empty or absent
/// schema always passes.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    let Some(map) = schema.as_object() else {
        return Ok(());
    };
    if map.is_empty() {
        return Ok(());
    }
    let mut errors = Vec::new();
    validate_at(schema, schema, value, "", &mut errors, 0);
    if errors.is_empty() {
        return Ok(());
    }
    Err(errors.join("; "))
}

fn validate_at(
    root: &Value,
    schema: &Value,
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
    depth: usize,
) {
    let prefix = message_prefix(path);
    if depth > MAX_DEPTH {
        return;
    }
    let Some(schema) = schema.as_object() else {
        return;
    };

    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if let Some(target) = resolve_ref(root, reference) {
            validate_at(root, target, value, path, errors, depth + 1);
        }
        return;
    }

    if schema.get("nullable").and_then(Value::as_bool) == Some(true) && value.is_null() {
        return;
    }

    if let Some(declared) = schema.get("type") {
        let names: Vec<&str> = match declared {
            Value::String(single) => vec![single.as_str()],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.is_empty() && !names.iter().any(|name| type_matches(name, value)) {
            errors.push(format!("{prefix}must be {}", names.join(",")));
            return;
        }
    }

    if let Some(expected) = schema.get("const")
        && expected != value
    {
        errors.push(format!("{prefix}must be equal to constant"));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        errors.push(format!(
            "{prefix}must be equal to one of the allowed values"
        ));
    }

    match value {
        Value::String(text) => {
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
                && (text.chars().count() as u64) < min
            {
                errors.push(format!("{prefix}must NOT have fewer than {min} characters"));
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
                && (text.chars().count() as u64) > max
            {
                errors.push(format!("{prefix}must NOT have more than {max} characters"));
            }
            if let Some(pattern) = schema.get("pattern").and_then(Value::as_str)
                && let Ok(regex) = regex::Regex::new(pattern)
                && !regex.is_match(text)
            {
                errors.push(format!("{prefix}must match pattern \"{pattern}\""));
            }
        }
        Value::Number(number) => {
            validate_number(number, schema, path, errors);
        }
        Value::Array(items) => {
            validate_array(root, schema, items, path, errors, depth);
        }
        Value::Object(map) => {
            validate_object(root, schema, map, path, errors, depth);
        }
        _ => {}
    }

    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            validate_at(root, branch, value, path, errors, depth + 1);
        }
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array)
        && !branches
            .iter()
            .any(|branch| passes(root, branch, value, depth + 1))
    {
        errors.push(format!("{prefix}must match a schema in anyOf"));
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matches = branches
            .iter()
            .filter(|branch| passes(root, branch, value, depth + 1))
            .count();
        if matches != 1 {
            errors.push(format!("{prefix}must match exactly one schema in oneOf"));
        }
    }
    if let Some(branch) = schema.get("not")
        && passes(root, branch, value, depth + 1)
    {
        errors.push(format!("{prefix}must NOT be valid"));
    }
}

fn validate_number(
    number: &serde_json::Number,
    schema: &serde_json::Map<String, Value>,
    path: &str,
    errors: &mut Vec<String>,
) {
    let prefix = message_prefix(path);
    let Some(value) = number.as_f64() else {
        return;
    };
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
        && value < minimum
    {
        errors.push(format!("{prefix}must be >= {minimum}"));
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
        && value > maximum
    {
        errors.push(format!("{prefix}must be <= {maximum}"));
    }
    if let Some(minimum) = schema.get("exclusiveMinimum").and_then(Value::as_f64)
        && value <= minimum
    {
        errors.push(format!("{prefix}must be > {minimum}"));
    }
    if let Some(maximum) = schema.get("exclusiveMaximum").and_then(Value::as_f64)
        && value >= maximum
    {
        errors.push(format!("{prefix}must be < {maximum}"));
    }
}

fn validate_array(
    root: &Value,
    schema: &serde_json::Map<String, Value>,
    items: &[Value],
    path: &str,
    errors: &mut Vec<String>,
    depth: usize,
) {
    let prefix = message_prefix(path);
    if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
        && (items.len() as u64) < min
    {
        errors.push(format!("{prefix}must NOT have fewer than {min} items"));
    }
    if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
        && (items.len() as u64) > max
    {
        errors.push(format!("{prefix}must NOT have more than {max} items"));
    }
    match schema.get("items") {
        Some(Value::Object(_)) | Some(Value::Bool(_)) => {
            let item_schema = schema.get("items").expect("checked above");
            for (index, item) in items.iter().enumerate() {
                let child = child_path(path, &index.to_string());
                validate_at(root, item_schema, item, &child, errors, depth + 1);
            }
        }
        Some(Value::Array(tuple)) => {
            for (index, item) in items.iter().enumerate() {
                let Some(item_schema) = tuple.get(index) else {
                    break;
                };
                let child = child_path(path, &index.to_string());
                validate_at(root, item_schema, item, &child, errors, depth + 1);
            }
        }
        _ => {}
    }
}

fn validate_object(
    root: &Value,
    schema: &serde_json::Map<String, Value>,
    map: &serde_json::Map<String, Value>,
    path: &str,
    errors: &mut Vec<String>,
    depth: usize,
) {
    let prefix = message_prefix(path);
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !map.contains_key(name) {
                errors.push(format!("{prefix}must have required property '{name}'"));
            }
        }
    }
    if let Some(min) = schema.get("minProperties").and_then(Value::as_u64)
        && (map.len() as u64) < min
    {
        errors.push(format!("{prefix}must NOT have fewer than {min} properties"));
    }
    if let Some(max) = schema.get("maxProperties").and_then(Value::as_u64)
        && (map.len() as u64) > max
    {
        errors.push(format!("{prefix}must NOT have more than {max} properties"));
    }

    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(properties) = properties {
        for (name, property_schema) in properties {
            if let Some(value) = map.get(name) {
                let child = child_path(path, name);
                validate_at(root, property_schema, value, &child, errors, depth + 1);
            }
        }
    }

    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => {
            for name in map.keys() {
                let declared = properties.is_some_and(|properties| properties.contains_key(name));
                if !declared {
                    errors.push(format!(
                        "{prefix}must NOT have additional property '{name}'"
                    ));
                }
            }
        }
        Some(Value::Object(_)) => {
            let extra_schema = schema.get("additionalProperties").expect("checked above");
            for (name, value) in map {
                let declared = properties.is_some_and(|properties| properties.contains_key(name));
                if !declared {
                    let child = child_path(path, name);
                    validate_at(root, extra_schema, value, &child, errors, depth + 1);
                }
            }
        }
        _ => {}
    }
}

/// Whether `value` satisfies `schema`, with the error text discarded. Used by
/// the combinators, which report their own verdict rather than the branch's
/// failures.
fn passes(root: &Value, schema: &Value, value: &Value, depth: usize) -> bool {
    let mut errors = Vec::new();
    validate_at(root, schema, value, "", &mut errors, depth);
    errors.is_empty()
}

fn message_prefix(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("{path} ")
    }
}

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn resolve_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    if !pointer.starts_with('/') {
        return None;
    }
    root.pointer(pointer)
}

fn child_path(path: &str, segment: &str) -> String {
    let escaped = segment.replace('~', "~0").replace('/', "~1");
    format!("{path}/{escaped}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_invalid(schema: Value, value: Value, expected: &str) {
        let error = validate(&schema, &value).expect_err("must be rejected");
        assert!(
            error.contains(expected),
            "expected {expected:?} in {error:?}"
        );
    }

    #[test]
    fn an_absent_or_empty_schema_accepts_anything() {
        assert!(validate(&Value::Null, &json!({ "a": 1 })).is_ok());
        assert!(validate(&json!({}), &json!("not even an object")).is_ok());
    }

    #[test]
    fn a_wrong_type_is_rejected_with_ajv_wording() {
        assert_invalid(
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            json!({ "path": 12 }),
            "must be string",
        );
    }

    #[test]
    fn the_failing_property_is_named_by_its_json_pointer() {
        let error = validate(
            &json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            &json!({ "path": 12 }),
        )
        .unwrap_err();
        assert_eq!(error, "/path must be string");
    }

    #[test]
    fn a_missing_required_property_is_named() {
        assert_invalid(
            json!({ "type": "object", "required": ["path"] }),
            json!({}),
            "must have required property 'path'",
        );
    }

    #[test]
    fn additional_properties_are_refused_when_the_schema_says_so() {
        assert_invalid(
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "additionalProperties": false
            }),
            json!({ "path": "a", "extra": true }),
            "must NOT have additional property 'extra'",
        );
    }

    #[test]
    fn an_integer_accepts_a_whole_float_and_rejects_a_fraction() {
        let schema = json!({ "type": "integer" });
        assert!(validate(&schema, &json!(3.0)).is_ok());
        assert_invalid(schema, json!(3.5), "must be integer");
    }

    #[test]
    fn a_type_union_accepts_either_branch_and_names_both() {
        let schema = json!({ "type": ["string", "null"] });
        assert!(validate(&schema, &Value::Null).is_ok());
        assert!(validate(&schema, &json!("ok")).is_ok());
        assert_invalid(schema, json!(1), "must be string,null");
    }

    #[test]
    fn enum_and_const_are_reported_separately() {
        assert_invalid(
            json!({ "enum": ["a", "b"] }),
            json!("c"),
            "must be equal to one of the allowed values",
        );
        assert_invalid(
            json!({ "const": "a" }),
            json!("c"),
            "must be equal to constant",
        );
    }

    #[test]
    fn string_bounds_and_pattern_are_checked() {
        assert_invalid(
            json!({ "type": "string", "minLength": 3 }),
            json!("ab"),
            "must NOT have fewer than 3 characters",
        );
        assert_invalid(
            json!({ "type": "string", "maxLength": 2 }),
            json!("abc"),
            "must NOT have more than 2 characters",
        );
        assert_invalid(
            json!({ "type": "string", "pattern": "^[a-z]+$" }),
            json!("ABC"),
            "must match pattern",
        );
    }

    #[test]
    fn numeric_bounds_are_checked() {
        assert_invalid(json!({ "minimum": 5 }), json!(4), "must be >= 5");
        assert_invalid(json!({ "maximum": 5 }), json!(6), "must be <= 5");
        assert_invalid(json!({ "exclusiveMinimum": 5 }), json!(5), "must be > 5");
        assert_invalid(json!({ "exclusiveMaximum": 5 }), json!(5), "must be < 5");
    }

    #[test]
    fn array_items_and_bounds_are_checked() {
        assert_invalid(
            json!({ "type": "array", "items": { "type": "string" } }),
            json!(["a", 2]),
            "/1 must be string",
        );
        assert_invalid(
            json!({ "type": "array", "minItems": 2 }),
            json!(["a"]),
            "must NOT have fewer than 2 items",
        );
        assert_invalid(
            json!({ "type": "array", "maxItems": 1 }),
            json!(["a", "b"]),
            "must NOT have more than 1 items",
        );
    }

    #[test]
    fn combinators_report_their_own_verdict() {
        assert_invalid(
            json!({ "anyOf": [{ "type": "string" }, { "type": "number" }] }),
            json!(true),
            "must match a schema in anyOf",
        );
        assert_invalid(
            json!({ "oneOf": [{ "type": "string" }, { "type": "string" }] }),
            json!("x"),
            "must match exactly one schema in oneOf",
        );
        assert_invalid(
            json!({ "not": { "type": "string" } }),
            json!("x"),
            "must NOT be valid",
        );
    }

    #[test]
    fn all_of_validates_every_branch() {
        let schema = json!({
            "allOf": [
                { "type": "object", "required": ["path"] },
                { "type": "object", "required": ["content"] }
            ]
        });
        assert_invalid(
            schema,
            json!({ "path": "a" }),
            "required property 'content'",
        );
    }

    #[test]
    fn a_local_ref_resolves_and_an_external_one_is_skipped() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "$ref": "#/$defs/Path" } },
            "$defs": { "Path": { "type": "string" } }
        });
        assert_invalid(schema, json!({ "path": 1 }), "must be string");

        let external = json!({ "properties": { "x": { "$ref": "https://example.test/s.json" } } });
        assert!(validate(&external, &json!({ "x": 1 })).is_ok());
    }

    #[test]
    fn nullable_accepts_null_where_the_schema_permits_it() {
        let schema = json!({ "type": "string", "nullable": true });
        assert!(validate(&schema, &Value::Null).is_ok());
    }

    #[test]
    fn errors_are_joined_the_way_ajv_joins_them() {
        let error = validate(
            &json!({ "type": "object", "required": ["path", "content"] }),
            &json!({}),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "must have required property 'path'; must have required property 'content'"
        );
    }

    #[test]
    fn a_self_referential_schema_terminates() {
        let schema = json!({
            "$ref": "#",
            "type": "object"
        });
        assert!(validate(&schema, &json!({ "a": 1 })).is_ok());
    }
}

/// The keywords this validator implements, i.e. the ones Ajv enforces.
///
/// The list is a contract, not a guess:
/// `every_keyword_the_engines_own_tools_use_is_implemented` walks every built-in
/// tool schema and fails when one names a keyword that is neither here nor in
/// [`ANNOTATION_KEYWORDS`]. A future tool that needs `format` or `prefixItems`
/// therefore cannot ship without this module learning it first.
#[cfg(test)]
const KNOWN_KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
    "pattern",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "$ref",
    "$defs",
    "definitions",
    "nullable",
];

/// Keywords that carry no constraint of their own.
///
/// `format` is deliberately **not** here: Ajv validates formats through
/// `addFormats`, so a schema that starts using `format` must add the check
/// rather than be quietly allowed. Listing it in neither set is what makes the
/// coverage test name it.
#[cfg(test)]
const ANNOTATION_KEYWORDS: &[&str] = &[
    "$schema",
    "title",
    "description",
    "default",
    "examples",
    "$comment",
    "deprecated",
    "readOnly",
    "writeOnly",
];

#[cfg(test)]
mod schema_coverage {
    use super::{ANNOTATION_KEYWORDS, KNOWN_KEYWORDS};
    use serde_json::Value;
    use std::collections::BTreeSet;

    /// Walk one schema and record every keyword it uses.
    ///
    /// Only the schema-bearing keywords are recursed into, because a value under
    /// `properties` is a map of property *names*, not a map of keywords.
    fn collect(schema: &Value, found: &mut BTreeSet<String>) {
        let Some(map) = schema.as_object() else {
            return;
        };
        for (keyword, value) in map {
            found.insert(keyword.clone());
            match keyword.as_str() {
                "properties" | "$defs" | "definitions" | "dependentSchemas" => {
                    if let Some(children) = value.as_object() {
                        for child in children.values() {
                            collect(child, found);
                        }
                    }
                }
                "items"
                | "additionalProperties"
                | "not"
                | "contains"
                | "if"
                | "then"
                | "else"
                | "propertyNames" => collect(value, found),
                "anyOf" | "oneOf" | "allOf" | "prefixItems" => {
                    if let Some(branches) = value.as_array() {
                        for branch in branches {
                            collect(branch, found);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn every_keyword_the_engines_own_tools_use_is_implemented() {
        let mut schemas: Vec<(String, Value)> = Vec::new();
        for tool in crate::tools::core_tool_defs::all_native_tool_defs() {
            schemas.push((tool.name, tool.input_schema));
        }
        for tool in crate::tools::core_tool_defs::core_tool_defs() {
            schemas.push((tool.name, tool.input_schema));
        }
        assert!(!schemas.is_empty(), "the tool tables must not be empty");

        let mut unknown: Vec<String> = Vec::new();
        for (name, schema) in &schemas {
            let mut keywords = BTreeSet::new();
            collect(schema, &mut keywords);
            for keyword in keywords {
                if KNOWN_KEYWORDS.contains(&keyword.as_str())
                    || ANNOTATION_KEYWORDS.contains(&keyword.as_str())
                {
                    continue;
                }
                unknown.push(format!("{name}: {keyword}"));
            }
        }
        unknown.sort();
        unknown.dedup();
        assert!(
            unknown.is_empty(),
            "tool schemas use keywords this validator does not implement: {unknown:?}"
        );
    }
}
