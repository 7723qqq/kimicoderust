use serde::Deserialize;
use serde_json::Value;

use crate::app::config::toml::{plain_object_to_toml, transform_plain_object};

pub use super::internal::types::HookDef as HookDefConfig;
use super::internal::types::HOOK_EVENT_TYPES;

pub const HOOKS_SECTION: &str = "hooks";

pub fn hooks_from_toml(raw_snake: Value) -> Value {
    match raw_snake {
        Value::Array(hooks) => Value::Array(
            hooks
                .into_iter()
                .map(|hook| match hook {
                    Value::Object(hook) => Value::Object(transform_plain_object(&hook)),
                    hook => hook,
                })
                .collect(),
        ),
        raw_snake => raw_snake,
    }
}

pub fn hooks_to_toml(value: Value, _raw_snake: &Value) -> Value {
    match value {
        Value::Array(hooks) => Value::Array(
            hooks
                .into_iter()
                .map(|hook| match hook {
                    Value::Object(hook) => Value::Object(plain_object_to_toml(&hook, None)),
                    hook => hook,
                })
                .collect(),
        ),
        value => value,
    }
}

pub(crate) fn deserialize_hook_event<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let event = String::deserialize(deserializer)?;
    if HOOK_EVENT_TYPES.contains(&event.as_str()) {
        Ok(event)
    } else {
        Err(serde::de::Error::custom(format!(
            "unknown hook event '{event}'; expected one of {}",
            HOOK_EVENT_TYPES.join(", ")
        )))
    }
}

pub(crate) fn deserialize_hook_command<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let command = String::deserialize(deserializer)?;
    if command.is_empty() {
        return Err(serde::de::Error::custom("hook command must not be empty"));
    }
    Ok(command)
}

pub(crate) fn deserialize_hook_timeout<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let seconds = value
        .as_f64()
        .ok_or_else(|| serde::de::Error::custom("hook timeout must be a number when present"))?;
    if !seconds.is_finite() || seconds.fract() != 0.0 || !(1.0..=600.0).contains(&seconds) {
        return Err(serde::de::Error::custom(format!(
            "hook timeout must be between 1 and 600 seconds, got {seconds}"
        )));
    }
    Ok(Some(seconds as u64))
}

pub(crate) fn deserialize_hook_cwd<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::String(dir) if dir.is_empty() => Err(serde::de::Error::custom(
            "hook cwd must not be empty when present",
        )),
        Value::String(dir) => Ok(Some(dir)),
        _ => Err(serde::de::Error::custom(
            "hook cwd must be a string when present",
        )),
    }
}

pub(crate) fn deserialize_hook_env<'de, D>(
    deserializer: D,
) -> Result<Option<std::collections::HashMap<String, String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if !value.is_object() {
        return Err(serde::de::Error::custom(
            "hook env must be a string map when present",
        ));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn hook_def_is_validated_like_v2() {
        let valid: HookDefConfig = serde_json::from_value(json!({
            "event": "PreToolUse",
            "command": "echo hi",
            "timeout": 30
        }))
        .unwrap();
        assert_eq!(valid.event, "PreToolUse");
        for (label, value) in [
            (
                "unknown event",
                json!({ "event": "NotAnEvent", "command": "echo hi" }),
            ),
            (
                "timeout below range",
                json!({ "event": "PreToolUse", "command": "echo hi", "timeout": 0 }),
            ),
            (
                "timeout above range",
                json!({ "event": "PreToolUse", "command": "echo hi", "timeout": 601 }),
            ),
            (
                "empty command",
                json!({ "event": "PreToolUse", "command": "" }),
            ),
            (
                "unknown field",
                json!({ "event": "PreToolUse", "command": "echo hi", "extra": true }),
            ),
            (
                "null timeout",
                json!({ "event": "PreToolUse", "command": "echo hi", "timeout": null }),
            ),
        ] {
            assert!(
                serde_json::from_value::<HookDefConfig>(value).is_err(),
                "{label} must be rejected"
            );
        }
    }
}
