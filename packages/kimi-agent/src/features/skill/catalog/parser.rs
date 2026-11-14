use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

use super::types::{SkillDefinition, SkillMetadata, SkillSource, is_supported_skill_type};

pub use crate::skills::{ParsedSkillMeta, parse_skill_frontmatter, parse_skill_metadata};

#[derive(Debug, Clone)]
pub struct SkillParseError {
    pub message: String,
    pub reason: Option<String>,
}

impl fmt::Display for SkillParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for SkillParseError {}

#[derive(Debug, Clone)]
pub struct UnsupportedSkillTypeError {
    pub skill_type: String,
}

impl fmt::Display for UnsupportedSkillTypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Skill type \"{}\" is not supported; only \"prompt\", \"inline\", and \"flow\" are supported.",
            self.skill_type
        )
    }
}

impl Error for UnsupportedSkillTypeError {}

#[derive(Debug, Clone)]
pub enum ParseSkillError {
    Parse(SkillParseError),
    UnsupportedType(UnsupportedSkillTypeError),
}

impl fmt::Display for ParseSkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => error.fmt(f),
            Self::UnsupportedType(error) => error.fmt(f),
        }
    }
}

impl Error for ParseSkillError {}

#[derive(Debug, Clone)]
pub struct ParseSkillTextOptions {
    pub skill_md_path: String,
    pub skill_dir_name: String,
    pub source: SkillSource,
    pub text: String,
}

pub fn parse_skill_text(options: &ParseSkillTextOptions) -> Result<SkillDefinition, ParseSkillError> {
    let is_directory_skill = Path::new(&options.skill_md_path)
        .file_name()
        .is_some_and(|name| name == "SKILL.md");
    if is_directory_skill && options.text.split('\n').next().map(trim_js) != Some("---") {
        return Err(ParseSkillError::Parse(SkillParseError {
            message: format!("Missing frontmatter in {}", options.skill_md_path),
            reason: None,
        }));
    }
    let parsed = crate::_base::text::frontmatter::parse_frontmatter(&options.text).map_err(|error| {
        ParseSkillError::Parse(SkillParseError {
            message: format!("Invalid frontmatter in {}: {error}", options.skill_md_path),
            reason: Some(error.to_string()),
        })
    })?;
    let empty_frontmatter = serde_json::Map::new();
    let frontmatter = match parsed.data.as_object() {
        Some(frontmatter) => frontmatter,
        None if parsed.data.is_null() => &empty_frontmatter,
        None => {
            return Err(ParseSkillError::Parse(SkillParseError {
                message: format!(
                    "Frontmatter in {} must be a mapping at the top level",
                    options.skill_md_path
                ),
                reason: None,
            }));
        }
    };
    let metadata = normalize_metadata(frontmatter);
    let skill_type = non_empty_string(metadata.get("type"));
    if !is_supported_skill_type(skill_type.as_deref()) {
        let raw_type = skill_type.unwrap_or_else(|| {
            metadata
                .get("type")
                .map(value_string)
                .unwrap_or_else(|| "undefined".to_string())
        });
        return Err(ParseSkillError::UnsupportedType(UnsupportedSkillTypeError {
            skill_type: raw_type,
        }));
    }
    let name = non_empty_string(metadata.get("name"));
    let description = non_empty_string(metadata.get("description"));
    if is_directory_skill && (name.is_none() || description.is_none()) {
        let field = if name.is_none() { "\"name\"" } else { "\"description\"" };
        return Err(ParseSkillError::Parse(SkillParseError {
            message: format!(
                "Missing required frontmatter field {field} in {}",
                options.skill_md_path
            ),
            reason: None,
        }));
    }
    let skill_path = resolve_path(&options.skill_md_path);
    let content = trim_js(&parsed.body).to_string();
    let dir = Path::new(&skill_path)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_string_lossy()
        .replace('\\', "/");
    Ok(SkillDefinition {
        name: name.unwrap_or_else(|| options.skill_dir_name.clone()),
        description: description.unwrap_or_else(|| description_from_body(&content)),
        path: skill_path,
        dir,
        content: content.clone(),
        metadata,
        source: options.source,
        workspace_dir: None,
        plugin: None,
        mermaid: parse_mermaid_flowchart(&content),
        d2: parse_d2_flowchart(&content),
        product_specific: None,
        scopes: None,
        experimental_flag: None,
    })
}

pub fn parse_mermaid_flowchart(markdown: &str) -> Option<String> {
    Regex::new(r"(?s)```mermaid\r?\n(.*?)\r?\n```")
        .ok()?
        .captures(markdown)?
        .get(1)
        .map(|value| value.as_str().to_string())
}

pub fn parse_d2_flowchart(markdown: &str) -> Option<String> {
    Regex::new(r"(?s)```d2\r?\n(.*?)\r?\n```")
        .ok()?
        .captures(markdown)?
        .get(1)
        .map(|value| value.as_str().to_string())
}

pub fn skill_argument_names(metadata: &SkillMetadata) -> Vec<String> {
    let valid = |name: &&str| !trim_js(name).is_empty() && !name.bytes().all(|byte| byte.is_ascii_digit());
    match metadata.get("arguments") {
        Some(Value::String(value)) => value.split(is_js_whitespace).filter(valid).map(str::to_owned).collect(),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .filter(valid)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn normalize_metadata(raw: &serde_json::Map<String, Value>) -> SkillMetadata {
    let mut out = SkillMetadata::new();
    for (raw_key, value) in raw {
        let key = match raw_key.as_str() {
            "when-to-use" | "when_to_use" => "whenToUse",
            "disable-model-invocation" | "disable_model_invocation" => "disableModelInvocation",
            key => key,
        };
        out.insert(key.to_string(), value.clone());
    }
    for key in ["type", "name", "description"] {
        if let Some(value) = non_empty_string(out.get(key)) {
            out.insert(key.to_string(), Value::String(value));
        }
    }
    out
}

fn description_from_body(body: &str) -> String {
    let Some(first) = body.split('\n').map(trim_js).find(|line| !line.is_empty()) else {
        return "No description provided.".to_string();
    };
    let units: Vec<u16> = first.encode_utf16().collect();
    if units.len() > 240 {
        format!("{}…", String::from_utf16_lossy(&units[..239]))
    } else {
        first.to_string()
    }
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(trim_js)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn value_string(value: &Value) -> String {
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| ctx.eval::<String, _>(format!("String({value})")))
            })
        })
        .unwrap_or_else(|_| match value {
            Value::String(value) => value.clone(),
            _ => value.to_string(),
        })
}

fn resolve_path(path: &str) -> String {
    let path = PathBuf::from(path);
    let resolved = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut normalized = PathBuf::new();
    for component in resolved.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized.to_string_lossy().replace('\\', "/")
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}
