use std::error::Error;
use std::fmt;

use serde_json::Value;

const FENCE: &str = "---";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterError {
    message: String,
    pub cause: Option<String>,
}

impl FrontmatterError {
    pub fn new(message: impl Into<String>, cause: Option<String>) -> Self {
        Self {
            message: message.into(),
            cause,
        }
    }

    pub fn name(&self) -> &'static str {
        "FrontmatterError"
    }
}

impl fmt::Display for FrontmatterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for FrontmatterError {}

#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    pub data: Value,
    pub body: String,
}

pub type ParsedFrontmatter = Frontmatter;

pub fn parse_frontmatter(text: &str) -> Result<Frontmatter, FrontmatterError> {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.first().map(|line| line.trim()) != Some(FENCE) {
        return Ok(Frontmatter {
            data: Value::Null,
            body: text.to_string(),
        });
    }
    let close = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| (line.trim() == FENCE).then_some(index))
        .ok_or_else(|| FrontmatterError::new("Missing closing frontmatter fence", None))?;
    let yaml_text = lines[1..close].join("\n").trim().to_string();
    let body = lines[close + 1..].join("\n");
    if yaml_text.is_empty() {
        return Ok(Frontmatter {
            data: Value::Object(serde_json::Map::new()),
            body,
        });
    }
    let parsed: Value = serde_yaml::from_str(&yaml_text)
        .map_err(|error| FrontmatterError::new(error.to_string(), Some(error.to_string())))?;
    if parsed.is_null() {
        return Ok(Frontmatter {
            data: Value::Object(serde_json::Map::new()),
            body,
        });
    }
    Ok(Frontmatter { data: parsed, body })
}
