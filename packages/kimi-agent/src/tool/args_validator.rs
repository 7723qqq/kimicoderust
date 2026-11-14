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

struct Guard<'a> {
    stack: Vec<(usize, usize)>,
    fatal: Option<String>,
    dynamic_scopes: Vec<(String, &'a Value)>,
    recursive_scopes: Vec<&'a Value>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemCoverage {
    None,
    Count(usize),
    All,
}

impl Default for ItemCoverage {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Clone, Default)]
struct Evaluated {
    properties: std::collections::BTreeSet<String>,
    all_properties: bool,
    items: ItemCoverage,
}

impl Evaluated {
    fn merge(&mut self, other: Self) {
        self.all_properties |= other.all_properties;
        self.properties.extend(other.properties);
        self.items = match (self.items, other.items) {
            (ItemCoverage::All, _) | (_, ItemCoverage::All) => ItemCoverage::All,
            (ItemCoverage::Count(left), ItemCoverage::Count(right)) => {
                ItemCoverage::Count(left.max(right))
            }
            (ItemCoverage::Count(count), ItemCoverage::None)
            | (ItemCoverage::None, ItemCoverage::Count(count)) => ItemCoverage::Count(count),
            (ItemCoverage::None, ItemCoverage::None) => ItemCoverage::None,
        };
    }

    fn covers_property(&self, name: &str) -> bool {
        self.all_properties || self.properties.contains(name)
    }
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
    if let Some(message) = invalid_schema(schema) {
        return Err(message);
    }
    if let Some(message) = invalid_dynamic_ref(schema) {
        return Err(message);
    }
    if let Some((reference, base)) = unresolvable_ref(schema, schema) {
        return Err(format!("can't resolve reference {reference} from id {base}"));
    }
    if let Some(message) = invalid_pattern(schema) {
        return Err(message);
    }
    let ctx = Ctx {
        root: schema,
        draft: draft_for(schema),
    };
    let mut errors = Vec::new();
    collect_dynamic_anchors(schema)?;
    let mut guard = Guard {
        stack: Vec::new(),
        fatal: None,
        dynamic_scopes: Vec::new(),
        recursive_scopes: Vec::new(),
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

fn invalid_schema(root: &Value) -> Option<String> {
    if root.get("$schema").is_some_and(|schema| !schema.is_string()) {
        return Some("$schema must be a string".to_string());
    }
    let draft = draft_for(root);
    invalid_schema_at(root, "", false, draft).or_else(|| invalid_schema_at(root, "", true, draft))
}

fn invalid_schema_at(
    node: &Value,
    path: &str,
    nullable_pass: bool,
    draft: Draft,
) -> Option<String> {
    let Value::Object(map) = node else {
        return None;
    };
    if nullable_pass {
        if let Some(nullable) = map.get("nullable") {
            if !map.contains_key("type") {
                return Some("\"nullable\" cannot be used without \"type\"".to_string());
            }
            if !nullable.is_boolean() {
                return Some("nullable value must be [\"boolean\"]".to_string());
            }
            let has_null = match map.get("type") {
                Some(Value::String(name)) => name == "null",
                Some(Value::Array(names)) => names.iter().any(|name| name.as_str() == Some("null")),
                _ => false,
            };
            if has_null && nullable.as_bool() == Some(false) {
                return Some("type: null contradicts nullable: false".to_string());
            }
        }
    } else if let Some(declared) = map.get("type")
        && let Some(message) = invalid_type_schema(declared, path)
    {
        return Some(message);
    }

    for keyword in [
        "additionalItems",
        "additionalProperties",
        "contains",
        "items",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "unevaluatedItems",
        "unevaluatedProperties",
    ] {
        if (draft == Draft::D07 && matches!(keyword, "unevaluatedItems" | "unevaluatedProperties"))
            || (draft == Draft::D2020 && keyword == "additionalItems")
        {
            continue;
        }
        if let Some(child) = map.get(keyword) {
            let child_path = child_path(path, keyword);
            if let Some(message) = invalid_schema_at(child, &child_path, nullable_pass, draft) {
                return Some(message);
            }
        }
    }
    if let Some(tuple) = map.get("items").and_then(Value::as_array) {
        for (index, child) in tuple.iter().enumerate() {
            let child_path = child_path(&child_path(path, "items"), &index.to_string());
            if let Some(message) = invalid_schema_at(child, &child_path, nullable_pass, draft) {
                return Some(message);
            }
        }
    }
    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if draft == Draft::D07 && matches!(keyword, "$defs" | "dependentSchemas") {
            continue;
        }
        if let Some(children) = map.get(keyword).and_then(Value::as_object) {
            for (name, child) in children {
                let child_path = child_path(&child_path(path, keyword), name);
                if let Some(message) = invalid_schema_at(child, &child_path, nullable_pass, draft) {
                    return Some(message);
                }
            }
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if draft != Draft::D2020 && keyword == "prefixItems" {
            continue;
        }
        if let Some(children) = map.get(keyword).and_then(Value::as_array) {
            for (index, child) in children.iter().enumerate() {
                let child_path = child_path(&child_path(path, keyword), &index.to_string());
                if let Some(message) = invalid_schema_at(child, &child_path, nullable_pass, draft) {
                    return Some(message);
                }
            }
        }
    }
    if let Some(dependencies) = map.get("dependencies").and_then(Value::as_object) {
        for (name, child) in dependencies {
            if matches!(child, Value::Object(_) | Value::Bool(_)) {
                let child_path = child_path(&child_path(path, "dependencies"), name);
                if let Some(message) = invalid_schema_at(child, &child_path, nullable_pass, draft) {
                    return Some(message);
                }
            }
        }
    }
    None
}

fn invalid_type_schema(declared: &Value, path: &str) -> Option<String> {
    const TYPES: &[&str] = &[
        "array", "boolean", "integer", "null", "number", "object", "string",
    ];
    let valid = match declared {
        Value::String(name) => TYPES.contains(&name.as_str()),
        Value::Array(names) => {
            !names.is_empty()
                && names.iter().all(|name| {
                    name.as_str().is_some_and(|name| TYPES.contains(&name))
                })
                && {
                    let mut unique = true;
                    'pairs: for i in (1..names.len()).rev() {
                        for j in (0..i).rev() {
                            if names[i] == names[j] {
                                unique = false;
                                break 'pairs;
                            }
                        }
                    }
                    unique
                }
        }
        _ => false,
    };
    if valid {
        return None;
    }
    let type_path = format!("data{}/type", path);
    let mut errors = vec![format!(
        "{type_path} must be equal to one of the allowed values"
    )];
    if let Value::Array(names) = declared {
        for (index, name) in names.iter().enumerate() {
            if !name.as_str().is_some_and(|name| TYPES.contains(&name)) {
                errors.push(format!(
                    "{type_path}/{index} must be equal to one of the allowed values"
                ));
            }
        }
        if names.is_empty() {
            errors.push(format!("{type_path} must NOT have fewer than 1 items"));
        }
        'pairs: for i in (1..names.len()).rev() {
            for j in (0..i).rev() {
                if names[i] == names[j] {
                    errors.push(format!(
                        "{type_path} must NOT have duplicate items (items ## {j} and {i} are identical)"
                    ));
                    break 'pairs;
                }
            }
        }
    } else {
        errors.push(format!("{type_path} must be array"));
    }
    errors.push(format!("{type_path} must match a schema in anyOf"));
    Some(format!("schema is invalid: {}", errors.join(", ")))
}

fn invalid_dynamic_ref(node: &Value) -> Option<String> {
    match node {
        Value::Array(items) => items.iter().find_map(invalid_dynamic_ref),
        Value::Object(map) => {
            for keyword in ["$dynamicRef", "$recursiveRef"] {
                if let Some(reference) = map.get(keyword).and_then(Value::as_str)
                    && !reference.starts_with('#')
                {
                    return Some(format!(
                        "\"{keyword}\" only supports hash fragment reference"
                    ));
                }
            }
            map.values().find_map(invalid_dynamic_ref)
        }
        _ => None,
    }
}

fn unresolvable_ref(root: &Value, node: &Value) -> Option<(String, String)> {
    match node {
        Value::Array(items) => items.iter().find_map(|item| unresolvable_ref(root, item)),
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(Value::as_str)
                && resolve_ref(root, node, reference).is_none()
            {
                let base = resource_context(root, node)
                    .map(|(_, base)| if base.is_empty() { "#".to_string() } else { base })
                    .unwrap_or_else(|| "#".to_string());
                return Some((reference.to_string(), base));
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
                && let Err(reason) = crate::tools::regex_js::validate(pattern)
            {
                return Some(pattern_compile_error(pattern, &reason));
            }
            if let Some(patterns) = map.get("patternProperties").and_then(Value::as_object) {
                for pattern in patterns.keys() {
                    if let Err(reason) = crate::tools::regex_js::validate(pattern) {
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
    crate::tools::regex_js::is_match(pattern, text).unwrap_or(false)
}

fn validate_reference<'a>(
    ctx: Ctx<'a>,
    target: &'a Value,
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
    guard: &mut Guard<'a>,
    _reference: &str,
) {
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

fn validate_at<'a>(
    ctx: Ctx<'a>,
    schema: &'a Value,
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
    guard: &mut Guard<'a>,
) {
    let dynamic_anchor = if ctx.draft == Draft::D07 {
        None
    } else {
        schema.get("$dynamicAnchor").and_then(Value::as_str)
    };
    let recursive_anchor = ctx.draft != Draft::D07
        && schema.get("$recursiveAnchor").and_then(Value::as_bool) == Some(true);
    if let Some(anchor) = dynamic_anchor {
        guard.dynamic_scopes.push((anchor.to_string(), schema));
    }
    if recursive_anchor {
        guard.recursive_scopes.push(schema);
    }
    validate_at_inner(ctx, schema, value, path, errors, guard);
    if dynamic_anchor.is_some() {
        guard.dynamic_scopes.pop();
    }
    if recursive_anchor {
        guard.recursive_scopes.pop();
    }
}

fn validate_at_inner<'a>(
    ctx: Ctx<'a>,
    schema: &'a Value,
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
    guard: &mut Guard<'a>,
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
    let schema_value = schema;
    let Value::Object(schema) = schema else {
        return;
    };

    if ctx.draft != Draft::D07 {
        if let Some(reference) = schema.get("$recursiveRef").and_then(Value::as_str) {
            let target = guard
                .recursive_scopes
                .first()
                .copied()
                .unwrap_or(schema_value);
            validate_reference(ctx, target, value, path, errors, guard, reference);
        }
        if let Some(reference) = schema.get("$dynamicRef").and_then(Value::as_str) {
            let anchor = reference.strip_prefix('#').unwrap_or(reference);
            let target = guard
                .dynamic_scopes
                .iter()
                .find(|(name, _)| name == anchor)
                .map(|(_, schema)| *schema)
                .unwrap_or(schema_value);
            validate_reference(ctx, target, value, path, errors, guard, reference);
        }
    }

    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if let Some(target) = resolve_ref(ctx.root, schema_value, reference) {
            validate_reference(ctx, target, value, path, errors, guard, reference);
        }
    }

    if let Some(declared) = schema.get("type") {
        let mut names: Vec<&str> = match declared {
            Value::String(single) => vec![single.as_str()],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if schema.get("nullable").and_then(Value::as_bool) == Some(true)
            && !names.contains(&"null")
        {
            names.push("null");
        }
        if !names.is_empty() && !names.iter().any(|name| type_matches(name, value)) {
            errors.push(format!("{prefix}must be {}", names.join(",")));
        }
    }

    if let Some(expected) = schema.get("const")
        && !json_equal(expected, value)
    {
        errors.push(format!("{prefix}must be equal to constant"));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.iter().any(|allowed| json_equal(allowed, value))
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
                && crate::tools::regex_js::is_match(pattern, text).is_ok_and(|matched| !matched)
            {
                errors.push(format!("{prefix}must match pattern \"{pattern}\""));
            }

            if let Some(name) = schema.get("format").and_then(Value::as_str)
                && crate::tools::args_format::rejects_string(name, text)
            {
                errors.push(format!("{prefix}must match format \"{name}\""));
            }
        }
        Value::Number(number) => {
            validate_number(number, schema, path, errors);
        }
        Value::Array(items) => {
            validate_array(ctx, schema_value, schema, value, items, path, errors, guard);
        }
        Value::Object(map) => {
            validate_object(ctx, schema_value, schema, value, map, path, errors, guard);
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
        && crate::tools::args_format::rejects_number(name, number)
    {
        errors.push(format!("{prefix}must match format \"{name}\""));
    }
}

fn validate_array<'a>(
    ctx: Ctx<'a>,
    schema_value: &'a Value,
    schema: &'a serde_json::Map<String, Value>,
    value: &Value,
    items: &[Value],
    path: &str,
    errors: &mut Vec<String>,
    guard: &mut Guard<'a>,
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
                if json_equal(&items[i], &items[j]) {
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
        if items.len() > covered && ctx.draft != Draft::D2020 {
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

    if ctx.draft != Draft::D07
        && let Some(unevaluated) = schema.get("unevaluatedItems")
    {
        let evaluated = evaluated_at(ctx, schema_value, value, guard);
        let covered = match evaluated.items {
            ItemCoverage::All => items.len(),
            ItemCoverage::Count(count) => count,
            ItemCoverage::None => 0,
        };
        if unevaluated == &Value::Bool(false) {
            if items.len() > covered {
                errors.push(format!("{prefix}must NOT have more than {covered} items"));
            }
        } else {
            for (index, item) in items.iter().enumerate().skip(covered) {
                let child = child_path(path, &index.to_string());
                validate_at(ctx, unevaluated, item, &child, errors, guard);
            }
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

fn validate_object<'a>(
    ctx: Ctx<'a>,
    schema_value: &'a Value,
    schema: &'a serde_json::Map<String, Value>,
    value: &Value,
    map: &serde_json::Map<String, Value>,
    path: &str,
    errors: &mut Vec<String>,
    guard: &mut Guard<'a>,
) {
    let prefix = message_prefix(path);
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !map.contains_key(name) {
                errors.push(format!("must have required property '{name}'"));
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
                    crate::tools::regex_js::validate(pattern)
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
                    errors.push(format!("must NOT have additional property '{name}'"));
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

    if ctx.draft != Draft::D07
        && !schema.contains_key("additionalProperties")
        && let Some(unevaluated) = schema.get("unevaluatedProperties")
    {
        let evaluated = evaluated_at(ctx, schema_value, value, guard);
        for (name, value) in map {
            if evaluated.covers_property(name) {
                continue;
            }
            if unevaluated == &Value::Bool(false) {
                errors.push(format!("{prefix}must NOT have unevaluated properties"));
            } else {
                let child = child_path(path, name);
                validate_at(ctx, unevaluated, value, &child, errors, guard);
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

fn collect_errors<'a>(
    ctx: Ctx<'a>,
    schema: &'a Value,
    value: &Value,
    path: &str,
    guard: &mut Guard<'a>,
) -> Vec<String> {
    let mut errors = Vec::new();
    validate_at(ctx, schema, value, path, &mut errors, guard);
    errors
}

fn passes<'a>(ctx: Ctx<'a>, schema: &'a Value, value: &Value, guard: &mut Guard<'a>) -> bool {
    collect_errors(ctx, schema, value, "", guard).is_empty()
}

fn evaluated_at<'a>(
    ctx: Ctx<'a>,
    schema: &'a Value,
    value: &Value,
    guard: &mut Guard<'a>,
) -> Evaluated {
    evaluated_at_inner(ctx, schema, value, guard, &mut Vec::new())
}

fn evaluated_at_inner<'a>(
    ctx: Ctx<'a>,
    schema: &'a Value,
    value: &Value,
    guard: &mut Guard<'a>,
    seen: &mut Vec<usize>,
) -> Evaluated {
    let Value::Object(map) = schema else {
        return Evaluated::default();
    };
    let mut evaluated = Evaluated::default();

    if let Some(properties) = map.get("properties").and_then(Value::as_object) {
        evaluated.properties.extend(properties.keys().cloned());
    }
    if let Some(patterns) = map.get("patternProperties").and_then(Value::as_object)
        && let Some(object) = value.as_object()
    {
        for (pattern, _) in patterns {
            for name in object.keys() {
                if js_is_match(pattern, name) {
                    evaluated.properties.insert(name.clone());
                }
            }
        }
    }
    if map.contains_key("additionalProperties") {
        evaluated.all_properties = true;
    }

    match map.get("items") {
        Some(Value::Array(tuple)) if ctx.draft != Draft::D2020 => {
            evaluated.items = if map.contains_key("additionalItems") {
                ItemCoverage::All
            } else {
                ItemCoverage::Count(tuple.len())
            };
        }
        Some(Value::Bool(_) | Value::Object(_)) => {
            evaluated.items = ItemCoverage::All;
        }
        _ => {}
    }
    if ctx.draft == Draft::D2020 {
        if let Some(tuple) = map.get("prefixItems").and_then(Value::as_array) {
            evaluated.items = ItemCoverage::Count(tuple.len());
        }
        if matches!(map.get("items"), Some(Value::Bool(_) | Value::Object(_))) {
            evaluated.items = ItemCoverage::All;
        }
    }
    if let Some(contains) = map.get("contains") {
        let min = if ctx.draft == Draft::D07 {
            1
        } else {
            map.get("minContains").and_then(Value::as_u64).unwrap_or(1)
        };
        let max = if ctx.draft == Draft::D07 {
            None
        } else {
            map.get("maxContains").and_then(Value::as_u64)
        };
        let always_valid = matches!(contains, Value::Bool(true))
            || contains.as_object().is_some_and(|schema| schema.is_empty());
        if !always_valid && !(max.is_none() && min == 0) && !max.is_some_and(|max| min > max) {
            evaluated.items = ItemCoverage::All;
        }
    }

    if let Some(reference) = map.get("$ref").and_then(Value::as_str)
        && let Some(target) = resolve_ref(ctx.root, schema, reference)
    {
        let identity = target as *const Value as usize;
        if !seen.contains(&identity) {
            seen.push(identity);
            let nested = evaluated_at_inner(ctx, target, value, guard, seen);
            seen.pop();
            evaluated.merge(nested);
        }
    }

    if let Some(branches) = map.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            evaluated.merge(evaluated_at_inner(ctx, branch, value, guard, seen));
        }
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = map.get(keyword).and_then(Value::as_array) {
            for branch in branches {
                if passes(ctx, branch, value, guard) {
                    evaluated.merge(evaluated_at_inner(ctx, branch, value, guard, seen));
                }
            }
        }
    }
    if map.contains_key("if")
        && (map.contains_key("then") || map.contains_key("else"))
        && let Some(condition) = map.get("if")
    {
        let condition_passes = passes(ctx, condition, value, guard);
        evaluated.merge(evaluated_at_inner(ctx, condition, value, guard, seen));
        let clause = if condition_passes { "then" } else { "else" };
        if let Some(branch) = map.get(clause) {
            evaluated.merge(evaluated_at_inner(ctx, branch, value, guard, seen));
        }
    }
    if let Some(dependencies) = map.get("dependentSchemas").and_then(Value::as_object)
        && let Some(object) = value.as_object()
    {
        for (property, dependency) in dependencies {
            if object.contains_key(property) {
                evaluated.merge(evaluated_at_inner(ctx, dependency, value, guard, seen));
            }
        }
    }
    evaluated
}

fn message_prefix(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("{path} ")
    }
}

fn json_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| json_equal(left, right))
                })
        }
        _ => left == right,
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

fn collect_dynamic_anchors(root: &Value) -> Result<(), String> {
    fn walk<'a>(
        node: &'a Value,
        is_root: bool,
        anchors: &mut std::collections::HashMap<String, &'a Value>,
    ) -> Result<(), String> {
        match node {
            Value::Array(items) => {
                for item in items {
                    walk(item, false, anchors)?;
                }
            }
            Value::Object(map) => {
                for keyword in ["$anchor", "$dynamicAnchor"] {
                    if let Some(anchor) = map.get(keyword).and_then(Value::as_str) {
                        if !valid_anchor(anchor) {
                            return Err(format!("invalid anchor \"{anchor}\""));
                        }
                        if !is_root
                            && let Some(previous) = anchors.insert(anchor.to_string(), node)
                            && !json_equal(previous, node)
                        {
                            return Err(format!(
                                "reference \"#{anchor}\" resolves to more than one schema"
                            ));
                        }
                    }
                }
                for child in map.values() {
                    walk(child, false, anchors)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut anchors = std::collections::HashMap::new();
    walk(root, true, &mut anchors)?;
    Ok(())
}

fn valid_anchor(anchor: &str) -> bool {
    let mut chars = anchor.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_'))
}

fn resolve_ref<'a>(root: &'a Value, current: &'a Value, reference: &str) -> Option<&'a Value> {
    let (resource, base) = resource_context(root, current).unwrap_or((root, String::new()));
    if let Some(fragment) = reference.strip_prefix('#') {
        return resolve_fragment(resource, fragment);
    }
    let resolved = resolve_uri(&base, reference);
    let (id, fragment) = resolved
        .split_once('#')
        .unwrap_or((resolved.as_str(), ""));
    let target = find_resource_by_id(root, String::new(), id)?;
    if resolved.contains('#') {
        resolve_fragment(target, fragment)
    } else {
        Some(target)
    }
}

fn resource_context<'a>(root: &'a Value, current: &'a Value) -> Option<(&'a Value, String)> {
    fn walk<'a>(
        node: &'a Value,
        current: &'a Value,
        resource: &'a Value,
        base: String,
    ) -> Option<(&'a Value, String)> {
        let (resource, base) = match node.get("$id").and_then(Value::as_str) {
            Some(id) => (node, resolve_uri(&base, id)),
            None => (resource, base),
        };
        if std::ptr::eq(node, current) {
            return Some((resource, base));
        }
        match node {
            Value::Array(items) => {
                for item in items {
                    if let Some(found) = walk(item, current, resource, base.clone()) {
                        return Some(found);
                    }
                }
            }
            Value::Object(map) => {
                for child in map.values() {
                    if let Some(found) = walk(child, current, resource, base.clone()) {
                        return Some(found);
                    }
                }
            }
            _ => {}
        }
        None
    }
    walk(root, current, root, String::new())
}

fn find_resource_by_id<'a>(node: &'a Value, base: String, target: &str) -> Option<&'a Value> {
    let mut found = None;
    let base = match node.get("$id").and_then(Value::as_str) {
        Some(id) => {
            let resolved = resolve_uri(&base, id);
            if resolved == target {
                found = Some(node);
            }
            resolved
        }
        None => base,
    };
    if found.is_some() {
        return found;
    }
    match node {
        Value::Array(items) => {
            for item in items {
                if let Some(found) = find_resource_by_id(item, base.clone(), target) {
                    return Some(found);
                }
            }
        }
        Value::Object(map) => {
            for child in map.values() {
                if let Some(found) = find_resource_by_id(child, base.clone(), target) {
                    return Some(found);
                }
            }
        }
        _ => {}
    }
    None
}

fn resolve_fragment<'a>(resource: &'a Value, fragment: &str) -> Option<&'a Value> {
    if fragment.is_empty() {
        return Some(resource);
    }
    if fragment.starts_with('/') {
        return resource.pointer(fragment);
    }
    find_anchor(resource, fragment)
}

fn find_anchor<'a>(node: &'a Value, anchor: &str) -> Option<&'a Value> {
    if ["$anchor", "$dynamicAnchor"]
        .iter()
        .filter_map(|keyword| node.get(*keyword).and_then(Value::as_str))
        .any(|candidate| candidate == anchor)
    {
        return Some(node);
    }
    match node {
        Value::Array(items) => items.iter().find_map(|item| find_anchor(item, anchor)),
        Value::Object(map) => map.values().find_map(|child| find_anchor(child, anchor)),
        _ => None,
    }
}

fn resolve_uri(base: &str, reference: &str) -> String {
    let resolved = if base.is_empty() {
        reference.to_string()
    } else if let Ok(base) = url::Url::parse(base) {
        base.join(reference)
            .map(|url| url.to_string())
            .unwrap_or_else(|_| reference.to_string())
    } else if reference.contains("://") || reference.starts_with("urn:") {
        reference.to_string()
    } else if let Some((directory, _)) = base.rsplit_once('/') {
        format!("{directory}/{reference}")
    } else {
        reference.to_string()
    };
    resolved.trim_end_matches('#').to_string()
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
    fn an_unknown_type_is_rejected_while_compiling_the_schema() {
        let error = validate(&json!({ "type": "nonsense" }), &Value::Null).unwrap_err();
        assert_eq!(
            error,
            "schema is invalid: data/type must be equal to one of the allowed values, data/type must be array, data/type must match a schema in anyOf"
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
    fn required_and_additional_property_errors_omit_the_parent_path() {
        let error = validate(
            &json!({
                "properties": {
                    "outer": {
                        "type": "object",
                        "required": ["x"],
                        "additionalProperties": false
                    }
                }
            }),
            &json!({ "outer": { "y": 1 } }),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "must have required property 'x'; must NOT have additional property 'y'"
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
    fn numeric_json_values_use_javascript_number_equality() {
        assert!(validate(&json!({ "const": 1 }), &json!(1.0)).is_ok());
        assert!(validate(&json!({ "enum": [1] }), &json!(1.0)).is_ok());
        assert!(validate(&json!({ "const": { "n": 1 } }), &json!({ "n": 1.0 })).is_ok());
        assert_invalid(
            json!({ "uniqueItems": true }),
            json!([1, 1.0]),
            "must NOT have duplicate items (items ## 0 and 1 are identical)",
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
    fn anchors_and_internal_ids_resolve_like_local_references() {
        let anchored = json!({
            "$schema": "https://json-schema.org/draft/2019-09/schema",
            "$defs": { "Path": { "$anchor": "path", "type": "string" } },
            "properties": { "path": { "$ref": "#path" } }
        });
        assert_invalid(anchored, json!({ "path": 1 }), "/path must be string");

        let identified = json!({
            "$id": "https://example.test/root.json",
            "$defs": { "Path": { "$id": "path.json", "type": "string" } },
            "properties": { "path": { "$ref": "path.json" } }
        });
        assert_invalid(identified, json!({ "path": 1 }), "/path must be string");

        let nested = json!({
            "$defs": {
                "inner": {
                    "$id": "inner.json",
                    "$defs": { "Path": { "type": "string" } },
                    "properties": { "path": { "$ref": "#/$defs/Path" } }
                }
            },
            "$ref": "inner.json"
        });
        assert_invalid(nested, json!({ "path": 1 }), "/path must be string");

        let missing = json!({
            "$id": "https://example.test/root.json",
            "$ref": "missing.json"
        });
        assert_eq!(
            validate(&missing, &Value::Null).unwrap_err(),
            "can't resolve reference missing.json from id https://example.test/root.json"
        );
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
        let error = validate(&schema, &json!(1)).unwrap_err();
        assert_eq!(error, "must be string,null");
    }

    #[test]
    fn nullable_does_not_bypass_other_constraints_or_compile_checks() {
        assert_invalid(
            json!({ "type": "string", "nullable": true, "enum": ["x"] }),
            Value::Null,
            "must be equal to one of the allowed values",
        );
        assert_eq!(
            validate(&json!({ "nullable": true }), &Value::Null).unwrap_err(),
            "\"nullable\" cannot be used without \"type\""
        );
        assert_eq!(
            validate(&json!({ "type": "null", "nullable": false }), &Value::Null).unwrap_err(),
            "type: null contradicts nullable: false"
        );
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
    fn tuple_items_use_additional_items_in_draft_2019() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2019-09/schema",
            "items": [{ "type": "string" }],
            "additionalItems": false
        });
        assert_invalid(schema, json!(["a", "b"]), "must NOT have more than 1 items");
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

    #[test]
    fn unevaluated_properties_cover_only_the_unmatched_names() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2019-09/schema",
            "properties": { "known": { "type": "string" } },
            "unevaluatedProperties": false
        });
        assert!(validate(&schema, &json!({ "known": "ok" })).is_ok());
        assert_invalid(
            schema.clone(),
            json!({ "known": "ok", "extra": 1 }),
            "must NOT have unevaluated properties",
        );

        let with_additional = json!({
            "$schema": "https://json-schema.org/draft/2019-09/schema",
            "properties": { "known": { "type": "string" } },
            "additionalProperties": true,
            "unevaluatedProperties": false
        });
        assert!(validate(&with_additional, &json!({ "known": "ok", "extra": 1 })).is_ok());

        let string_extras = json!({
            "$schema": "https://json-schema.org/draft/2019-09/schema",
            "properties": { "known": { "type": "string" } },
            "unevaluatedProperties": { "type": "string" }
        });
        assert!(validate(&string_extras, &json!({ "known": "ok", "extra": "text" })).is_ok());
        assert_invalid(
            string_extras,
            json!({ "known": "ok", "extra": 1 }),
            "/extra must be string",
        );
    }

    #[test]
    fn unevaluated_properties_inherit_annotations_from_applicators() {
        let all_of = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "allOf": [{ "properties": { "a": { "type": "string" } } }],
            "unevaluatedProperties": false
        });
        assert!(validate(&all_of, &json!({ "a": "x" })).is_ok());
        assert_invalid(
            all_of,
            json!({ "a": "x", "b": 1 }),
            "unevaluated properties",
        );

        let reference = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": { "base": { "properties": { "a": { "type": "string" } } } },
            "$ref": "#/$defs/base",
            "unevaluatedProperties": false
        });
        assert!(validate(&reference, &json!({ "a": "x" })).is_ok());
        assert_invalid(
            reference,
            json!({ "a": "x", "b": 1 }),
            "unevaluated properties",
        );

        let any_of = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "anyOf": [
                { "properties": { "a": { "type": "string" } } },
                { "properties": { "b": { "type": "number" } } }
            ],
            "unevaluatedProperties": false
        });
        assert!(validate(&any_of, &json!({ "a": "x" })).is_ok());
        assert!(validate(&any_of, &json!({ "b": 1 })).is_ok());
        assert!(validate(&any_of, &json!({ "a": "x", "b": 1 })).is_ok());
        assert_invalid(any_of, json!({ "c": true }), "unevaluated properties");
    }

    #[test]
    fn unevaluated_items_inherit_annotations_from_applicators() {
        let all_of = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "allOf": [{ "prefixItems": [{ "type": "string" }] }],
            "unevaluatedItems": false
        });
        assert!(validate(&all_of, &json!(["a"])).is_ok());
        assert_invalid(all_of, json!(["a", "b"]), "must NOT have more than 1 items");
    }

    #[test]
    fn unevaluated_items_respect_prefix_items_and_full_evaluation_shortcuts() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "prefixItems": [{ "type": "string" }],
            "unevaluatedItems": false
        });
        assert_invalid(schema, json!(["ok", 1]), "must NOT have more than 1 items");

        let single_items = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "items": { "type": "string" },
            "unevaluatedItems": false
        });
        assert!(validate(&single_items, &json!(["a", "b"])).is_ok());

        let additional_items = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "prefixItems": [{ "type": "string" }],
            "additionalItems": true,
            "unevaluatedItems": false
        });
        assert_invalid(
            additional_items,
            json!(["a", "b"]),
            "must NOT have more than 1 items",
        );

        let contains = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "contains": { "type": "string" },
            "unevaluatedItems": false
        });
        assert!(validate(&contains, &json!(["a", "b"])).is_ok());
    }

    #[test]
    fn recursive_ref_uses_the_active_recursive_scope() {
        let schema = json!({
            "$recursiveAnchor": true,
            "type": "object",
            "properties": { "node": { "$recursiveRef": "#" } }
        });
        assert!(validate(&schema, &json!({ "node": { "node": {} } })).is_ok());
        assert_invalid(schema, json!({ "node": 1 }), "/node must be object");

        let shadowed = json!({
            "$recursiveAnchor": true,
            "type": "object",
            "$defs": { "inner": { "$recursiveAnchor": true, "type": "string" } },
            "properties": {
                "x": { "$ref": "#/$defs/inner" },
                "y": { "$recursiveRef": "#" }
            }
        });
        assert!(validate(&shadowed, &json!({ "x": "s", "y": {} })).is_ok());
        assert_invalid(shadowed, json!({ "x": "s", "y": "s" }), "/y must be object");
    }

    #[test]
    fn dynamic_ref_uses_only_the_active_dynamic_scope() {
        let schema = json!({
            "$defs": {
                "node": {
                    "$dynamicAnchor": "node",
                    "type": "object",
                    "properties": { "child": { "$dynamicRef": "#node" } }
                }
            },
            "$ref": "#/$defs/node"
        });
        assert!(validate(&schema, &json!({ "child": { "child": {} } })).is_ok());
        assert_invalid(schema, json!({ "child": 1 }), "/child must be object");

        let shadowed = json!({
            "$dynamicAnchor": "node",
            "type": "object",
            "$defs": { "inner": { "$dynamicAnchor": "node", "type": "string" } },
            "properties": {
                "x": { "$ref": "#/$defs/inner" },
                "y": { "$dynamicRef": "#node" }
            }
        });
        assert!(validate(&shadowed, &json!({ "x": "s", "y": {} })).is_ok());
        assert_invalid(shadowed, json!({ "x": "s", "y": "s" }), "/y must be object");
    }

    #[test]
    fn dynamic_ref_without_an_active_anchor_repeats_the_current_validator() {
        let missing = json!({ "$dynamicRef": "#missing", "type": "string" });
        for value in [json!("ok"), json!(1)] {
            assert_eq!(
                validate(&missing, &value).expect_err("must recurse"),
                "Maximum call stack size exceeded"
            );
        }

        let inactive = json!({
            "$defs": { "positive": { "$dynamicAnchor": "positive", "type": "number" } },
            "$dynamicRef": "#positive"
        });
        assert_eq!(
            validate(&inactive, &json!(1)).expect_err("must recurse"),
            "Maximum call stack size exceeded"
        );
    }

    #[test]
    fn dynamic_and_recursive_refs_require_hash_fragments() {
        let dynamic = json!({ "$dynamicRef": "https://example.test/schema" });
        let error = validate(&dynamic, &json!(1)).expect_err("must be rejected");
        assert_eq!(
            error,
            "\"$dynamicRef\" only supports hash fragment reference"
        );

        let recursive = json!({ "$recursiveRef": "https://example.test/schema" });
        let error = validate(&recursive, &json!(1)).expect_err("must be rejected");
        assert_eq!(
            error,
            "\"$recursiveRef\" only supports hash fragment reference"
        );
    }

    #[test]
    fn duplicate_dynamic_anchors_are_rejected() {
        let schema = json!({
            "$defs": {
                "a": { "$dynamicAnchor": "same", "type": "string" },
                "b": { "$dynamicAnchor": "same", "type": "number" }
            }
        });
        let error = validate(&schema, &json!(1)).expect_err("must be rejected");
        assert_eq!(
            error,
            "reference \"#same\" resolves to more than one schema"
        );
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
