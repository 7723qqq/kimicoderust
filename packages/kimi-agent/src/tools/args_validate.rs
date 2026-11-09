use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Draft {
    D07,
    D2019,
    D2020,
}

const DRAFT_2019_KEYWORDS: &[&str] = &[
    "dependentRequired",
    "dependentSchemas",
    "maxContains",
    "minContains",
    "unevaluatedItems",
    "unevaluatedProperties",
    "$recursiveAnchor",
    "$recursiveRef",
];

const DRAFT_2020_KEYWORDS: &[&str] = &["prefixItems", "$dynamicAnchor", "$dynamicRef"];

#[derive(Clone, Copy)]
struct Ctx<'a> {
    root: &'a Value,
    draft: Draft,
}

struct Guard {
    stack: Vec<(usize, usize)>,
    fatal: Option<String>,
}

fn draft_for(schema: &Value) -> Draft {
    if let Some(uri) = schema.get("$schema").and_then(Value::as_str) {
        if uri.contains("2020-12") {
            return Draft::D2020;
        }
        if uri.contains("2019-09") {
            return Draft::D2019;
        }
        return Draft::D07;
    }
    if contains_keyword(schema, DRAFT_2020_KEYWORDS) {
        return Draft::D2020;
    }
    if contains_keyword(schema, DRAFT_2019_KEYWORDS) {
        return Draft::D2019;
    }
    Draft::D07
}

fn contains_keyword(value: &Value, keywords: &[&str]) -> bool {
    match value {
        Value::Array(items) => items.iter().any(|item| contains_keyword(item, keywords)),
        Value::Object(map) => map.iter().any(|(key, child)| {
            keywords.contains(&key.as_str()) || contains_keyword(child, keywords)
        }),
        _ => false,
    }
}

pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    match schema {
        Value::Object(map) if map.is_empty() => return Ok(()),
        Value::Bool(true) => return Ok(()),
        Value::Object(_) | Value::Bool(false) => {}
        _ => return Ok(()),
    }
    if let Some(reference) = unresolvable_ref(schema, schema) {
        return Err(format!("can't resolve reference {reference} from id #"));
    }
    if let Some(message) = invalid_pattern(schema) {
        return Err(message);
    }
    let ctx = Ctx {
        root: schema,
        draft: draft_for(schema),
    };
    let mut errors = Vec::new();
    let mut guard = Guard {
        stack: Vec::new(),
        fatal: None,
    };
    validate_at(ctx, schema, value, "", &mut errors, &mut guard);
    if let Some(fatal) = guard.fatal {
        return Err(fatal);
    }
    if errors.is_empty() {
        return Ok(());
    }
    Err(errors.join("; "))
}

fn unresolvable_ref(root: &Value, node: &Value) -> Option<String> {
    match node {
        Value::Array(items) => items.iter().find_map(|item| unresolvable_ref(root, item)),
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(Value::as_str)
                && resolve_ref(root, reference).is_none()
            {
                return Some(reference.to_string());
            }
            map.values().find_map(|child| unresolvable_ref(root, child))
        }
        _ => None,
    }
}

fn invalid_pattern(node: &Value) -> Option<String> {
    match node {
        Value::Array(items) => items.iter().find_map(invalid_pattern),
        Value::Object(map) => {
            if let Some(pattern) = map.get("pattern").and_then(Value::as_str)
                && let Err(reason) = super::regex_js::validate(pattern)
            {
                return Some(pattern_compile_error(pattern, &reason));
            }
            if let Some(patterns) = map.get("patternProperties").and_then(Value::as_object) {
                for pattern in patterns.keys() {
                    if let Err(reason) = super::regex_js::validate(pattern) {
                        return Some(pattern_compile_error(pattern, &reason));
                    }
                }
            }
            map.values().find_map(invalid_pattern)
        }
        _ => None,
    }
}

fn pattern_compile_error(pattern: &str, reason: &str) -> String {
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("Invalid regular expression: /{pattern}/u: {reason}")
}

fn js_is_match(pattern: &str, text: &str) -> bool {
    super::regex_js::is_match(pattern, text).unwrap_or(false)
}

fn validate_at(
    ctx: Ctx<'_>,
    schema: &Value,
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
    guard: &mut Guard,
) {
    let prefix = message_prefix(path);
    match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errors.push(format!("{prefix}boolean schema is false"));
            return;
        }
        Value::Object(_) => {}
        _ => return,
    }
    let Value::Object(schema) = schema else {
        return;
    };

    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if let Some(target) = resolve_ref(ctx.root, reference) {
            let entry = (
                target as *const Value as usize,
                value as *const Value as usize,
            );
            if guard.stack.contains(&entry) {
                guard
                    .fatal
                    .get_or_insert_with(|| "Maximum call stack size exceeded".to_string());
                return;
            }
            guard.stack.push(entry);
            validate_at(ctx, target, value, path, errors, guard);
            guard.stack.pop();
        }
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
                && super::regex_js::is_match(pattern, text).is_ok_and(|matched| !matched)
            {
                errors.push(format!("{prefix}must match pattern \"{pattern}\""));
            }

            if let Some(name) = schema.get("format").and_then(Value::as_str)
                && super::args_format::rejects_string(name, text)
            {
                errors.push(format!("{prefix}must match format \"{name}\""));
            }
        }
        Value::Number(number) => {
            validate_number(number, schema, path, errors);
        }
        Value::Array(items) => {
            validate_array(ctx, schema, items, path, errors, guard);
        }
        Value::Object(map) => {
            validate_object(ctx, schema, value, map, path, errors, guard);
        }
        _ => {}
    }

    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            validate_at(ctx, branch, value, path, errors, guard);
        }
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        let mut nested = Vec::new();
        let mut matched = false;
        for branch in branches {
            let branch_errors = collect_errors(ctx, branch, value, path, guard);
            if branch_errors.is_empty() {
                matched = true;
            } else {
                nested.extend(branch_errors);
            }
        }
        if !matched {
            errors.extend(nested);
            errors.push(format!("{prefix}must match a schema in anyOf"));
        }
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let mut nested = Vec::new();
        let mut matched = 0usize;
        for branch in branches {
            let branch_errors = collect_errors(ctx, branch, value, path, guard);
            if branch_errors.is_empty() {
                matched += 1;
            } else {
                nested.extend(branch_errors);
            }
        }
        if matched != 1 {
            errors.extend(nested);
            errors.push(format!("{prefix}must match exactly one schema in oneOf"));
        }
    }
    if let Some(condition) = schema.get("if") {
        let clause = if passes(ctx, condition, value, guard) {
            "then"
        } else {
            "else"
        };
        if let Some(clause_schema) = schema.get(clause) {
            let mut nested = collect_errors(ctx, clause_schema, value, path, guard);
            if !nested.is_empty() {
                errors.append(&mut nested);
                errors.push(format!("{prefix}must match \"{clause}\" schema"));
            }
        }
    }
    if let Some(branch) = schema.get("not")
        && passes(ctx, branch, value, guard)
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
    if let Some(limit_value) = schema.get("multipleOf")
        && let Some(limit) = limit_value.as_f64()
    {
        let quotient = value / limit;
        if limit == 0.0 || quotient != quotient.trunc() {
            errors.push(format!("{prefix}must be multiple of {limit_value}"));
        }
    }

    if let Some(name) = schema.get("format").and_then(Value::as_str)
        && super::args_format::rejects_number(name, number)
    {
        errors.push(format!("{prefix}must match format \"{name}\""));
    }
}

fn validate_array(
    ctx: Ctx<'_>,
    schema: &serde_json::Map<String, Value>,
    items: &[Value],
    path: &str,
    errors: &mut Vec<String>,
    guard: &mut Guard,
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
    if schema.get("uniqueItems").and_then(Value::as_bool) == Some(true) && items.len() > 1 {
        'pairs: for i in (1..items.len()).rev() {
            for j in (0..i).rev() {
                if items[i] == items[j] {
                    errors.push(format!(
                        "{prefix}must NOT have duplicate items (items ## {j} and {i} are identical)"
                    ));
                    break 'pairs;
                }
            }
        }
    }

    let mut covered = 0usize;
    if let Some(tuple) = schema.get("items").and_then(Value::as_array) {
        for (index, item_schema) in tuple.iter().enumerate() {
            let Some(item) = items.get(index) else {
                break;
            };
            let child = child_path(path, &index.to_string());
            validate_at(ctx, item_schema, item, &child, errors, guard);
        }
        covered = tuple.len();
        if items.len() > covered && ctx.draft == Draft::D07 {
            match schema.get("additionalItems") {
                Some(Value::Bool(false)) => {
                    errors.push(format!("{prefix}must NOT have more than {covered} items"));
                }
                Some(extra @ Value::Object(_)) => {
                    for (index, item) in items.iter().enumerate().skip(covered) {
                        let child = child_path(path, &index.to_string());
                        validate_at(ctx, extra, item, &child, errors, guard);
                    }
                }
                _ => {}
            }
        }
    } else {
        if ctx.draft == Draft::D2020
            && let Some(tuple) = schema.get("prefixItems").and_then(Value::as_array)
        {
            for (index, item_schema) in tuple.iter().enumerate() {
                let Some(item) = items.get(index) else {
                    break;
                };
                let child = child_path(path, &index.to_string());
                validate_at(ctx, item_schema, item, &child, errors, guard);
            }
            covered = tuple.len();
        }
        match schema.get("items") {
            Some(Value::Bool(false)) if ctx.draft == Draft::D2020 => {
                if items.len() > covered {
                    errors.push(format!("{prefix}must NOT have more than {covered} items"));
                }
            }
            Some(item_schema @ (Value::Bool(_) | Value::Object(_))) => {
                for (index, item) in items.iter().enumerate().skip(covered) {
                    let child = child_path(path, &index.to_string());
                    validate_at(ctx, item_schema, item, &child, errors, guard);
                }
            }
            _ => {}
        }
    }

    if let Some(contains) = schema.get("contains") {
        let min = if ctx.draft == Draft::D07 {
            1
        } else {
            schema
                .get("minContains")
                .and_then(Value::as_u64)
                .unwrap_or(1) as usize
        };
        let max = if ctx.draft == Draft::D07 {
            None
        } else {
            schema
                .get("maxContains")
                .and_then(Value::as_u64)
                .map(|value| value as usize)
        };
        if max.is_none() && min == 0 {
        } else if max.is_some_and(|max| min > max) {
            errors.push(contains_error(&prefix, min, max));
        } else {
            let mut nested = Vec::new();
            let mut count = 0usize;
            let mut exceeded = false;
            for (index, item) in items.iter().enumerate() {
                let child = child_path(path, &index.to_string());
                let item_errors = collect_errors(ctx, contains, item, &child, guard);
                if item_errors.is_empty() {
                    count += 1;
                    if max.is_none() && count >= min {
                        break;
                    }
                    if max.is_some_and(|max| count > max) {
                        exceeded = true;
                        break;
                    }
                } else {
                    nested.extend(item_errors);
                }
            }
            let satisfied = !exceeded && count >= min && max.is_none_or(|max| count <= max);
            if !satisfied {
                errors.extend(nested);
                errors.push(contains_error(&prefix, min, max));
            }
        }
    }
}

fn contains_error(prefix: &str, min: usize, max: Option<usize>) -> String {
    match max {
        Some(max) => {
            format!("{prefix}must contain at least {min} and no more than {max} valid item(s)")
        }
        None => format!("{prefix}must contain at least {min} valid item(s)"),
    }
}

fn validate_object(
    ctx: Ctx<'_>,
    schema: &serde_json::Map<String, Value>,
    value: &Value,
    map: &serde_json::Map<String, Value>,
    path: &str,
    errors: &mut Vec<String>,
    guard: &mut Guard,
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

    let pattern_properties: Vec<(&str, &Value)> = schema
        .get("patternProperties")
        .and_then(Value::as_object)
        .map(|patterns| {
            patterns
                .iter()
                .filter_map(|(pattern, property_schema)| {
                    super::regex_js::validate(pattern)
                        .ok()
                        .map(|()| (pattern.as_str(), property_schema))
                })
                .collect()
        })
        .unwrap_or_default();

    if let Some(properties) = properties {
        for (name, property_schema) in properties {
            if let Some(value) = map.get(name) {
                let child = child_path(path, name);
                validate_at(ctx, property_schema, value, &child, errors, guard);
            }
        }
    }
    for (pattern, property_schema) in &pattern_properties {
        for (name, value) in map {
            if js_is_match(pattern, name) {
                let child = child_path(path, name);
                validate_at(ctx, property_schema, value, &child, errors, guard);
            }
        }
    }

    let unmatched = |name: &String| {
        !properties.is_some_and(|properties| properties.contains_key(name))
            && !pattern_properties
                .iter()
                .any(|(pattern, _)| js_is_match(pattern, name))
    };

    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => {
            for name in map.keys() {
                if unmatched(name) {
                    errors.push(format!(
                        "{prefix}must NOT have additional property '{name}'"
                    ));
                }
            }
        }
        Some(extra_schema @ Value::Object(_)) => {
            for (name, value) in map {
                if unmatched(name) {
                    let child = child_path(path, name);
                    validate_at(ctx, extra_schema, value, &child, errors, guard);
                }
            }
        }
        _ => {}
    }

    if let Some(name_schema) = schema.get("propertyNames") {
        for name in map.keys() {
            let name_value = Value::String(name.clone());

            let mut nested = collect_errors(ctx, name_schema, &name_value, path, guard);
            if !nested.is_empty() {
                errors.append(&mut nested);
                errors.push(format!("{prefix}property name must be valid"));
            }
        }
    }

    if let Some(dependencies) = schema.get("dependencies").and_then(Value::as_object) {
        for (property, dependency) in dependencies {
            if !map.contains_key(property) {
                continue;
            }
            match dependency {
                Value::Array(required) => {
                    push_missing_dependencies(property, required, map, &prefix, errors);
                }
                Value::Object(_) | Value::Bool(_) => {
                    validate_at(ctx, dependency, value, path, errors, guard);
                }
                _ => {}
            }
        }
    }
    if let Some(dependencies) = schema.get("dependentRequired").and_then(Value::as_object) {
        for (property, dependency) in dependencies {
            if !map.contains_key(property) {
                continue;
            }
            if let Some(required) = dependency.as_array() {
                push_missing_dependencies(property, required, map, &prefix, errors);
            }
        }
    }
    if let Some(dependencies) = schema.get("dependentSchemas").and_then(Value::as_object) {
        for (property, dependency) in dependencies {
            if map.contains_key(property) {
                validate_at(ctx, dependency, value, path, errors, guard);
            }
        }
    }
}

fn push_missing_dependencies(
    property: &str,
    required: &[Value],
    map: &serde_json::Map<String, Value>,
    prefix: &str,
    errors: &mut Vec<String>,
) {
    let names: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
    if names.is_empty() {
        return;
    }
    for name in &names {
        if !map.contains_key(*name) {
            errors.push(format!(
                "{prefix}must have {} {} when property {property} is present",
                if names.len() == 1 {
                    "property"
                } else {
                    "properties"
                },
                names.join(", ")
            ));
        }
    }
}

fn collect_errors(
    ctx: Ctx<'_>,
    schema: &Value,
    value: &Value,
    path: &str,
    guard: &mut Guard,
) -> Vec<String> {
    let mut errors = Vec::new();
    validate_at(ctx, schema, value, path, &mut errors, guard);
    errors
}

fn passes(ctx: Ctx<'_>, schema: &Value, value: &Value, guard: &mut Guard) -> bool {
    collect_errors(ctx, schema, value, "", guard).is_empty()
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
    fn format_is_checked_by_the_runtime_type_not_the_declared_one() {
        assert_invalid(
            json!({ "type": "string", "format": "email" }),
            json!("not-an-email"),
            "must match format \"email\"",
        );

        assert!(validate(&json!({ "format": "email" }), &json!(123)).is_ok());

        let error = validate(&json!({ "type": "number", "format": "email" }), &json!("x"))
            .expect_err("must be rejected");
        assert_eq!(error, "must be number; must match format \"email\"");

        assert_invalid(
            json!({ "format": "int32" }),
            json!(2147483648i64),
            "must match format \"int32\"",
        );
        assert!(validate(&json!({ "format": "int32" }), &json!("123")).is_ok());

        assert!(validate(&json!({ "format": "nonsense-xyz" }), &json!("whatever")).is_ok());

        let error = validate(
            &json!({ "type": "object", "properties": { "mail": { "format": "email" } } }),
            &json!({ "mail": "x" }),
        )
        .expect_err("must be rejected");
        assert_eq!(error, "/mail must match format \"email\"");
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
    fn combinators_report_branch_errors_with_their_verdict() {
        let error = validate(
            &json!({ "anyOf": [{ "type": "string" }, { "type": "number" }] }),
            &json!(true),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "must be string; must be number; must match a schema in anyOf"
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
    fn a_local_ref_resolves_and_an_unresolvable_one_fails_like_ajv() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "$ref": "#/$defs/Path" } },
            "$defs": { "Path": { "type": "string" } }
        });
        assert_invalid(schema, json!({ "path": 1 }), "must be string");

        let external = json!({ "properties": { "x": { "$ref": "https://example.test/s.json" } } });
        let error = validate(&external, &json!({ "x": 1 })).expect_err("must be rejected");
        assert_eq!(
            error,
            "can't resolve reference https://example.test/s.json from id #"
        );

        let missing = json!({ "properties": { "x": { "$ref": "#/$defs/Absent" } } });
        let error = validate(&missing, &json!({ "x": 1 })).expect_err("must be rejected");
        assert_eq!(error, "can't resolve reference #/$defs/Absent from id #");
    }

    #[test]
    fn an_uncompilable_pattern_is_reported_instead_of_skipped() {
        let schema = json!({ "type": "string", "pattern": "(?<=a" });
        let error = validate(&schema, &json!("abc")).expect_err("must be rejected");
        assert!(error.starts_with("Invalid regular expression: /(?<=a/u: "));

        let schema = json!({
            "type": "object",
            "patternProperties": { "(?<=a": { "type": "string" } }
        });
        let error = validate(&schema, &json!({})).expect_err("must be rejected");
        assert!(error.starts_with("Invalid regular expression: /(?<=a/u: "));
    }

    #[test]
    fn a_js_only_construct_is_accepted_the_way_ajv_accepts_it() {
        let schema = json!({ "type": "string", "pattern": "(?<=a)b" });
        assert!(validate(&schema, &json!("ab")).is_ok());
        let error = validate(&schema, &json!("cb")).expect_err("must be rejected");
        assert!(error.contains("must match pattern \"(?<=a)b\""));
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
    fn a_self_referential_schema_reports_the_js_stack_overflow() {
        let schema = json!({
            "$ref": "#",
            "type": "object"
        });
        let error = validate(&schema, &json!({ "a": 1 })).expect_err("must be rejected");
        assert_eq!(error, "Maximum call stack size exceeded");
    }

    #[test]
    fn a_recursive_ref_over_nested_data_terminates() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "$ref": "#" } }
        });
        assert!(validate(&schema, &json!({ "a": { "a": {} } })).is_ok());
    }

    #[test]
    fn numeric_multiple_of_is_checked() {
        assert!(validate(&json!({ "multipleOf": 3 }), &json!(9)).is_ok());
        assert_invalid(
            json!({ "multipleOf": 3 }),
            json!(7),
            "must be multiple of 3",
        );
        assert_invalid(
            json!({ "multipleOf": 0.5 }),
            json!(0.75),
            "must be multiple of 0.5",
        );
    }

    #[test]
    fn duplicate_array_items_are_named_by_index() {
        assert_invalid(
            json!({ "type": "array", "uniqueItems": true }),
            json!([1, 2, 1]),
            "must NOT have duplicate items (items ## 0 and 2 are identical)",
        );
        assert!(validate(&json!({ "uniqueItems": true }), &json!([1, 2, 3])).is_ok());
    }

    #[test]
    fn contains_reports_the_failing_items_before_its_verdict() {
        let error = validate(
            &json!({ "type": "array", "contains": { "type": "integer" } }),
            &json!(["a", "b"]),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "/0 must be integer; /1 must be integer; must contain at least 1 valid item(s)"
        );
        assert!(
            validate(
                &json!({ "contains": { "type": "integer" } }),
                &json!(["a", 1])
            )
            .is_ok()
        );
    }

    #[test]
    fn contains_bounds_follow_the_2019_draft() {
        let schema = json!({
            "contains": { "type": "integer" },
            "minContains": 2,
            "maxContains": 2
        });
        assert!(validate(&schema, &json!([1, "a", 3])).is_ok());
        assert_invalid(
            schema,
            json!([1, "a", 3, 4]),
            "must contain at least 2 and no more than 2 valid item(s)",
        );
    }

    #[test]
    fn pattern_properties_participate_in_additional_properties() {
        let schema = json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "patternProperties": { "^x-": { "type": "string" } },
            "additionalProperties": false
        });
        assert!(validate(&schema, &json!({ "name": "a", "x-flag": "b" })).is_ok());
        assert_invalid(schema, json!({ "x-flag": 1 }), "/x-flag must be string");
    }

    #[test]
    fn property_dependencies_are_reported_per_missing_property() {
        assert_invalid(
            json!({ "dependencies": { "a": ["b", "c"] } }),
            json!({ "a": 1 }),
            "must have properties b, c when property a is present",
        );
        assert_invalid(
            json!({ "dependentRequired": { "a": ["b"] } }),
            json!({ "a": 1 }),
            "must have property b when property a is present",
        );
    }

    #[test]
    fn schema_dependencies_validate_the_whole_object() {
        assert_invalid(
            json!({ "dependencies": { "a": { "required": ["b"] } } }),
            json!({ "a": 1 }),
            "must have required property 'b'",
        );
        assert_invalid(
            json!({ "dependentSchemas": { "a": { "minProperties": 2 } } }),
            json!({ "a": 1 }),
            "must NOT have fewer than 2 properties",
        );
    }

    #[test]
    fn if_applies_then_or_else_and_names_the_clause() {
        let schema = json!({
            "if": { "properties": { "kind": { "const": "file" } }, "required": ["kind"] },
            "then": { "required": ["path"] },
            "else": { "required": ["glob"] }
        });
        assert_invalid(
            schema.clone(),
            json!({ "kind": "file" }),
            "must match \"then\" schema",
        );
        assert_invalid(schema.clone(), json!({}), "must match \"else\" schema");
        assert!(validate(&schema, &json!({ "kind": "file", "path": "a" })).is_ok());
    }

    #[test]
    fn property_names_are_checked_at_the_parent_path() {
        let error = validate(
            &json!({ "propertyNames": { "pattern": "^[a-z]+$" } }),
            &json!({ "OK": 1 }),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "must match pattern \"^[a-z]+$\"; property name must be valid"
        );
    }

    #[test]
    fn boolean_schemas_are_enforced() {
        assert_invalid(json!(false), json!({}), "boolean schema is false");
        assert!(validate(&json!(true), &json!({ "a": 1 })).is_ok());
        assert_invalid(
            json!({ "properties": { "a": false } }),
            json!({ "a": 1 }),
            "/a boolean schema is false",
        );
    }

    #[test]
    fn ref_siblings_are_evaluated_like_ajv_8() {
        assert_invalid(
            json!({
                "$defs": { "short": { "type": "string" } },
                "properties": { "a": { "$ref": "#/$defs/short", "minLength": 5 } }
            }),
            json!({ "a": "x" }),
            "must NOT have fewer than 5 characters",
        );
    }

    #[test]
    fn tuple_items_use_additional_items_in_draft_07() {
        assert_invalid(
            json!({ "items": [{ "type": "string" }], "additionalItems": false }),
            json!(["a", "b"]),
            "must NOT have more than 1 items",
        );
        assert_invalid(
            json!({ "items": [{ "type": "string" }], "additionalItems": { "type": "number" } }),
            json!(["a", "b"]),
            "/1 must be number",
        );
    }

    #[test]
    fn a_closed_tail_without_prefix_items_rejects_each_item() {
        assert_invalid(
            json!({ "items": false }),
            json!(["a"]),
            "/0 boolean schema is false",
        );
    }

    #[test]
    fn prefix_items_and_closed_tail_follow_draft_2020() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "prefixItems": [{ "type": "string" }],
            "items": false
        });
        assert!(validate(&schema, &json!(["a"])).is_ok());
        assert_invalid(schema, json!(["a", "b"]), "must NOT have more than 1 items");
    }
}

#[cfg(test)]
const KNOWN_KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "patternProperties",
    "required",
    "additionalProperties",
    "items",
    "additionalItems",
    "prefixItems",
    "contains",
    "minContains",
    "maxContains",
    "uniqueItems",
    "propertyNames",
    "dependencies",
    "dependentRequired",
    "dependentSchemas",
    "enum",
    "const",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
    "$ref",
    "$defs",
    "definitions",
    "nullable",
];

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
