use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::config::config::{
    ConfigSchema, EnvBindings, IConfigService, JsonConfigSchema, RegisterSectionOptions,
    StripEnvBoundFields, env_bindings, strip_env_bound_fields,
};
use crate::app::config::config_section_contributions::{
    register_config_schema_section,
};
use crate::app::config::errors::CONFIG_INVALID;
use crate::app::config::toml::{clone_record, plain_object_to_toml, transform_plain_object};
use kimi_core::primitives::errors::Error2;

use super::matches_rule::parse_permission_pattern;
use super::permission_rules::PermissionRule;

pub const PERMISSION_SECTION: &str = "permission";
pub const DANGEROUS_COMMAND_GUARD_ENV: &str = "KIMI_CODE_DANGEROUS_COMMAND_GUARD";

pub static PERMISSION_RULE_DECISION_SCHEMA: LazyLock<Value> =
    LazyLock::new(|| json!({"enum": ["allow", "deny", "ask"]}));
pub static PERMISSION_RULE_SCOPE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({"enum": ["turn-override", "session-runtime", "project", "user"]})
});
pub static PERMISSION_RULE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["decision", "pattern"],
        "properties": {
            "decision": PERMISSION_RULE_DECISION_SCHEMA.clone(),
            "scope": {
                "allOf": [PERMISSION_RULE_SCOPE_SCHEMA.clone()],
                "default": "user"
            },
            "pattern": {"type": "string", "minLength": 1},
            "reason": {"type": "string"}
        }
    })
});
pub static PERMISSION_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "rules": {"type": "array", "items": PERMISSION_RULE_SCHEMA.clone()},
            "dangerousCommandGuard": {"type": "boolean"}
        }
    })
});

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<PermissionRule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dangerous_command_guard: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionRuleConfig {
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default, rename = "match")]
    pub arg_match: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LegacyPermissionConfig {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub rules: Option<Vec<PermissionRuleConfig>>,
    #[serde(
        rename = "dangerous_command_guard",
        alias = "dangerousCommandGuard",
        default
    )]
    pub dangerous_command_guard: Option<bool>,
    #[serde(default)]
    pub deny: Option<toml::Value>,
    #[serde(default)]
    pub allow: Option<toml::Value>,
    #[serde(default)]
    pub ask: Option<toml::Value>,
}

struct PermissionSchema;

impl ConfigSchema for PermissionSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let value = JsonConfigSchema::new(PERMISSION_CONFIG_SCHEMA.clone()).parse(value)?;
        if let Some(rules) = value.get("rules").and_then(Value::as_array) {
            for (index, rule) in rules.iter().enumerate() {
                let pattern = rule.get("pattern").and_then(Value::as_str);
                if pattern.is_none_or(|pattern| parse_permission_pattern(pattern).is_none()) {
                    return Err(Error2::new(
                        CONFIG_INVALID,
                        format!(
                            "Invalid value at $.rules[{index}].pattern: Invalid permission rule pattern"
                        ),
                    ));
                }
            }
        }
        Ok(value)
    }
}

fn parse_dangerous_command_guard_env(raw: &str) -> Option<bool> {
    match raw {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

pub fn permission_env_bindings() -> EnvBindings {
    env_bindings(
        &PERMISSION_CONFIG_SCHEMA,
        &[(
            "dangerousCommandGuard",
            DANGEROUS_COMMAND_GUARD_ENV,
            parse_dangerous_command_guard_env,
        )],
    )
}

pub fn strip_permission_env() -> StripEnvBoundFields {
    strip_env_bound_fields(&permission_env_bindings())
}

pub fn is_dangerous_command_guard_enabled(config: &dyn IConfigService) -> bool {
    config
        .get(PERMISSION_SECTION)
        .as_ref()
        .and_then(|section| section.get("dangerousCommandGuard"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

pub fn permission_from_toml(raw_snake: &Value) -> Value {
    let Some(raw_snake) = raw_snake.as_object() else {
        return raw_snake.clone();
    };
    let raw = transform_plain_object(raw_snake);
    let mut rules = Vec::new();
    append_permission_rules(&mut rules, raw.get("rules"), None);
    append_permission_rules(&mut rules, raw.get("deny"), Some("deny"));
    append_permission_rules(&mut rules, raw.get("allow"), Some("allow"));
    append_permission_rules(&mut rules, raw.get("ask"), Some("ask"));
    let mut out = Map::new();
    if !rules.is_empty() {
        out.insert("rules".to_string(), Value::Array(rules));
    }
    if let Some(value) = raw.get("dangerousCommandGuard") {
        out.insert("dangerousCommandGuard".to_string(), value.clone());
    }
    Value::Object(out)
}

fn append_permission_rules(
    target: &mut Vec<Value>,
    value: Option<&Value>,
    decision: Option<&'static str>,
) {
    let Some(value) = value else {
        return;
    };
    match value {
        Value::Array(entries) => {
            for entry in entries {
                target.push(transform_permission_rule(entry, decision));
            }
        }
        entry => target.push(transform_permission_rule(entry, decision)),
    }
}

fn transform_permission_rule(value: &Value, decision: Option<&'static str>) -> Value {
    let Some(value) = value.as_object() else {
        return value.clone();
    };
    let rule = transform_plain_object(value);
    let mut out = Map::new();
    if let Some(decision) = decision
        .map(|decision| Value::String(decision.to_string()))
        .or_else(|| rule.get("decision").cloned())
    {
        out.insert("decision".to_string(), decision);
    }
    if let Some(scope) = rule.get("scope") {
        out.insert("scope".to_string(), scope.clone());
    }
    if let Some(reason) = rule.get("reason") {
        out.insert("reason".to_string(), reason.clone());
    }
    if let Some(tool) = rule.get("tool").and_then(Value::as_str) {
        let arg_pattern = rule
            .get("match")
            .and_then(Value::as_str)
            .or_else(|| rule.get("pattern").and_then(Value::as_str));
        out.insert(
            "pattern".to_string(),
            Value::String(match arg_pattern {
                Some(pattern) => format!("{tool}({pattern})"),
                None => tool.to_string(),
            }),
        );
    } else if let Some(pattern) = rule.get("pattern") {
        out.insert("pattern".to_string(), pattern.clone());
    }
    Value::Object(out)
}

pub fn permission_to_toml(value: &Value, raw_snake: &Value) -> Value {
    let Some(value) = value.as_object() else {
        return value.clone();
    };
    let mut out = clone_record(Some(raw_snake));
    out.remove("deny");
    out.remove("allow");
    out.remove("ask");
    if let Some(rules) = value.get("rules").and_then(Value::as_array) {
        out.insert(
            "rules".to_string(),
            Value::Array(
                rules
                    .iter()
                    .map(|rule| match rule.as_object() {
                        Some(rule) => Value::Object(plain_object_to_toml(rule, None)),
                        None => rule.clone(),
                    })
                    .collect(),
            ),
        );
    } else {
        out.remove("rules");
    }
    Value::Object(out)
}

pub fn rule_pattern(rule: &PermissionRuleConfig) -> Option<String> {
    if let Some(pattern) = rule.pattern.as_deref() {
        return Some(pattern.to_string());
    }
    let tool = rule.tool.as_deref()?;
    let arg = rule.arg_match.as_deref();
    Some(match arg {
        Some(arg) => format!("{tool}({arg})"),
        None => tool.to_string(),
    })
}

pub fn legacy_permission_patterns(value: &toml::Value) -> Vec<String> {
    let entries: Vec<&toml::Value> = match value {
        toml::Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    entries
        .into_iter()
        .filter_map(|entry| match entry {
            toml::Value::String(pattern) => Some(pattern.clone()),
            toml::Value::Table(table) => {
                if let Some(pattern) = table.get("pattern").and_then(toml::Value::as_str) {
                    return Some(pattern.to_string());
                }
                let tool = table.get("tool").and_then(toml::Value::as_str)?;
                let arg = table
                    .get("match")
                    .and_then(toml::Value::as_str)
                    .or_else(|| table.get("pattern").and_then(toml::Value::as_str));
                Some(match arg {
                    Some(arg) => format!("{tool}({arg})"),
                    None => tool.to_string(),
                })
            }
            _ => None,
        })
        .collect()
}

pub fn register_permission_config() {
    register_config_schema_section(
        PERMISSION_SECTION,
        Arc::new(PermissionSchema),
        RegisterSectionOptions {
            from_toml: Some(Arc::new(permission_from_toml)),
            to_toml: Some(Arc::new(|value, raw| Some(permission_to_toml(value, raw)))),
            env: Some(permission_env_bindings()),
            strip_env: Some(strip_permission_env()),
            ..Default::default()
        },
    );
}
