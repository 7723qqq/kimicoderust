use std::error::Error;
use std::fmt;

use serde_json::{Map, Value};

pub use super::config_pure::is_plain_object;
use super::config::{IConfigRegistry, ResolvedConfig};
use super::config_pure::describe_unknown_error;

pub fn snake_to_camel(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '_' && chars.peek().is_some_and(|next| next.is_ascii_lowercase()) {
            if let Some(next) = chars.next() {
                output.push(next.to_ascii_uppercase());
            }
        } else {
            output.push(character);
        }
    }
    output
}

pub fn camel_to_snake(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_uppercase() {
            output.push('_');
            output.push(character.to_ascii_lowercase());
        } else {
            output.push(character);
        }
    }
    output
}

pub fn transform_plain_object(data: &Map<String, Value>) -> Map<String, Value> {
    data.iter()
        .map(|(key, value)| (snake_to_camel(key), value.clone()))
        .collect()
}

pub fn plain_object_to_toml(value: &Map<String, Value>, raw: Option<&Value>) -> Map<String, Value> {
    let mut out = clone_record(raw);
    for (key, entry) in value {
        set_defined(&mut out, camel_to_snake(key), Some(entry.clone()));
    }
    out
}

fn default_from_toml(value: &Value) -> Value {
    value
        .as_object()
        .map(|value| Value::Object(transform_plain_object(value)))
        .unwrap_or_else(|| value.clone())
}

pub fn transform_toml_data(data: &Map<String, Value>, registry: &dyn IConfigRegistry) -> ResolvedConfig {
    let mut result = ResolvedConfig::new();
    for (key, value) in data {
        let domain = snake_to_camel(key);
        let from_toml = registry.get_section(&domain).and_then(|section| section.from_toml);
        result.insert(
            domain,
            match from_toml {
                Some(from_toml) => from_toml(value),
                None => default_from_toml(value),
            },
        );
    }
    result
}

pub fn apply_section_to_toml(
    raw_snake: &mut Map<String, Value>,
    domain: &str,
    value: Option<&Value>,
    registry: &dyn IConfigRegistry,
) {
    let snake_key = camel_to_snake(domain);
    let to_toml = registry.get_section(domain).and_then(|section| section.to_toml);
    let Some(value) = value else {
        raw_snake.remove(&snake_key);
        return;
    };
    if let Some(to_toml) = to_toml {
        let raw_sub = Value::Object(clone_record(raw_snake.get(&snake_key)));
        let converted = to_toml(value, &raw_sub);
        match converted {
            None | Some(Value::Null) => {
                raw_snake.remove(&snake_key);
            }
            Some(Value::Object(converted)) if converted.is_empty() => {
                raw_snake.remove(&snake_key);
            }
            Some(converted) => {
                raw_snake.insert(snake_key, converted);
            }
        }
        return;
    }
    let Some(value) = value.as_object() else {
        set_defined(raw_snake, snake_key, Some(value.clone()));
        return;
    };
    let raw_sub = clone_record(raw_snake.get(&snake_key));
    let converted = plain_object_to_toml(value, Some(&Value::Object(raw_sub)));
    if converted.is_empty() {
        raw_snake.remove(&snake_key);
    } else {
        raw_snake.insert(snake_key, Value::Object(converted));
    }
}

#[derive(Debug)]
pub struct TomlError {
    message: String,
    pub line: usize,
    pub column: usize,
}

impl TomlError {
    pub fn new(message: impl Into<String>, line: usize, column: usize) -> Self {
        Self {
            message: message.into(),
            line,
            column,
        }
    }

    fn from_decode(error: toml::de::Error) -> Self {
        let (line, column) = error.line_col().unwrap_or((0, 0));
        Self::new(error.to_string(), line + 1, column + 1)
    }
}

impl fmt::Display for TomlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TomlError {}

pub fn describe_toml_syntax_error(error: &dyn Error) -> String {
    let first_line = describe_unknown_error(error)
        .split('\n')
        .next()
        .unwrap_or_default()
        .to_string();
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<TomlError>() {
            return format!("{first_line} (line {}, column {})", error.line, error.column);
        }
        if let Some(error) = error.downcast_ref::<toml::de::Error>()
            && let Some((line, column)) = error.line_col()
        {
            return format!("{first_line} (line {}, column {})", line + 1, column + 1);
        }
        current = error.source();
    }
    first_line
}

pub fn clone_record(value: Option<&Value>) -> Map<String, Value> {
    value.and_then(Value::as_object).cloned().unwrap_or_default()
}

pub fn set_defined(target: &mut Map<String, Value>, key: String, value: Option<Value>) {
    if let Some(value) = value {
        target.insert(key, value);
    } else {
        target.remove(&key);
    }
}

pub fn parse_toml_document(text: &str) -> Result<Value, TomlError> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value = toml::from_str::<toml::Value>(text).map_err(TomlError::from_decode)?;
    serde_json::to_value(value).map_err(|error| TomlError::new(error.to_string(), 0, 0))
}
