use std::error::Error;
use std::fmt;
use std::ops::Deref;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::_base::text::frontmatter::{FrontmatterError, parse_frontmatter};
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::{
    Error2, Error2Options, VALIDATION_FAILED_CODE,
};

use super::types::{AgentFileDefinition, AgentFileSource};

#[derive(Debug)]
pub struct AgentFileParseError {
    inner: Error2,
}

impl AgentFileParseError {
    pub fn new(message: impl Into<String>, cause: Option<WireAsyncError>) -> Self {
        Self {
            inner: Error2::with_options(
                VALIDATION_FAILED_CODE,
                message,
                Error2Options {
                    cause,
                    name: Some("AgentFileParseError".to_string()),
                    ..Default::default()
                },
            ),
        }
    }

    pub fn reason(&self) -> Option<&(dyn Error + Send + Sync)> {
        self.inner.cause()
    }
}

impl Deref for AgentFileParseError {
    type Target = Error2;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl fmt::Display for AgentFileParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl Error for AgentFileParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.inner.source()
    }
}

pub struct ParseAgentFileOptions {
    pub path: String,
    pub source: AgentFileSource,
    pub text: String,
}

static AGENT_NAME_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]+(?:-[a-z0-9]+)*$").unwrap());

pub fn parse_agent_file_text(
    options: ParseAgentFileOptions,
) -> Result<AgentFileDefinition, WireAsyncError> {
    let parsed = match parse_frontmatter(&options.text) {
        Ok(parsed) => parsed,
        Err(error) => {
            let message = format!("Invalid frontmatter in {}: {error}", options.path);
            return Err(Box::new(AgentFileParseError::new(
                message,
                Some(Box::new(error)),
            )));
        }
    };
    let frontmatter = parsed.data;
    if frontmatter.is_null() {
        return Err(Box::new(AgentFileParseError::new(
            format!("Missing frontmatter in {}", options.path),
            None,
        )));
    }
    let Some(frontmatter) = frontmatter.as_object() else {
        return Err(Box::new(AgentFileParseError::new(
            format!(
                "Frontmatter in {} must be a mapping at the top level",
                options.path
            ),
            None,
        )));
    };
    let name_field = frontmatter.get("name");
    if name_field.is_some_and(|value| !value.is_null() && !value.is_string()) {
        return Err(parse_error(format!(
            "Frontmatter field \"name\" in {} must be a non-empty string",
            options.path
        )));
    }
    let name = non_empty_string(name_field)
        .or_else(|| derive_name_from_path(&options.path))
        .ok_or_else(|| {
            parse_error(format!(
                "Missing required frontmatter field \"name\" in {}",
                options.path
            ))
        })?;
    if !AGENT_NAME_PATTERN.is_match(&name) {
        return Err(parse_error(format!(
            "Invalid agent name \"{name}\" in {}: expected kebab-case (e.g. \"code-reviewer\")",
            options.path
        )));
    }
    let description = required_non_empty_string(
        frontmatter.get("description"),
        "description",
        &options.path,
    )?;
    let override_ = parse_boolean(frontmatter.get("override"), "override", &options.path)?;
    let raw_tools = parse_string_list(frontmatter.get("tools"), "tools", &options.path)?;
    let tools = match raw_tools {
        Some(tools) if tools.len() == 1 && tools[0] == "*" => None,
        tools => tools,
    };
    let disallowed_tools = parse_string_list(
        frontmatter.get("disallowedTools"),
        "disallowedTools",
        &options.path,
    )?;
    let subagents = parse_string_list(
        frontmatter.get("subagents"),
        "subagents",
        &options.path,
    )?;
    let prompt = js_trim(&parsed.body).to_string();
    if prompt.is_empty() {
        return Err(parse_error(format!(
            "Missing prompt body in {}",
            options.path
        )));
    }
    Ok(AgentFileDefinition {
        name,
        description,
        when_to_use: non_empty_string(frontmatter.get("whenToUse")),
        override_,
        tools,
        disallowed_tools,
        subagents,
        prompt,
        path: options.path,
        source: options.source,
    })
}

fn parse_boolean(
    value: Option<&Value>,
    field: &str,
    file_path: &str,
) -> Result<bool, WireAsyncError> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(parse_error(format!(
            "Frontmatter field \"{field}\" in {file_path} must be a boolean"
        ))),
    }
}

fn parse_string_list(
    value: Option<&Value>,
    field: &str,
    file_path: &str,
) -> Result<Option<Vec<String>>, WireAsyncError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(
            value
                .split(',')
                .map(js_trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect(),
        )),
        Some(Value::Array(values)) => {
            let mut out = Vec::new();
            for item in values {
                let Some(item) = item.as_str().map(js_trim).filter(|item| !item.is_empty()) else {
                    return Err(parse_error(format!(
                        "Frontmatter field \"{field}\" in {file_path} must be a list of non-empty strings"
                    )));
                };
                out.push(item.to_string());
            }
            Ok(Some(out))
        }
        _ => Err(parse_error(format!(
            "Frontmatter field \"{field}\" in {file_path} must be a comma-separated string or a list of strings"
        ))),
    }
}

fn required_non_empty_string(
    value: Option<&Value>,
    field: &str,
    file_path: &str,
) -> Result<String, WireAsyncError> {
    if value.is_some_and(|value| !value.is_null() && !value.is_string()) {
        return Err(parse_error(format!(
            "Frontmatter field \"{field}\" in {file_path} must be a non-empty string"
        )));
    }
    non_empty_string(value).ok_or_else(|| {
        parse_error(format!(
            "Missing required frontmatter field \"{field}\" in {file_path}"
        ))
    })
}

fn derive_name_from_path(file_path: &str) -> Option<String> {
    let base = file_path.split(['/', '\\']).next_back().unwrap_or("");
    let name = base.rfind('.').map_or(base, |index| &base[..index]);
    (!name.is_empty()).then(|| name.to_string())
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(js_trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'..='\u{000d}'
                | ' '
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

fn parse_error(message: String) -> WireAsyncError {
    Box::new(AgentFileParseError::new(message, None))
}
