use std::sync::LazyLock;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::environment::environment::{
    EnvironmentCapability, EnvironmentStatus, LOCAL_ENVIRONMENT_ID,
};
use crate::environment::environment_registry::EnvironmentGenerationSnapshot;

pub const ENVIRONMENT_ID_MAX_LENGTH: usize = 64;
pub const RESERVED_ENVIRONMENT_IDS: &[&str] = &["local", "default"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SshEnvironmentEntry {
    #[serde(rename = "type", deserialize_with = "require_ssh_type")]
    pub type_: String,
    #[serde(deserialize_with = "non_empty_string")]
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "non_empty_optional_string")]
    pub remote_bin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "non_empty_optional_string")]
    pub default_cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DockerEnvironmentEntry {
    #[serde(rename = "type", deserialize_with = "require_docker_type")]
    pub type_: String,
    #[serde(deserialize_with = "non_empty_string")]
    pub container: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "non_empty_optional_string")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "non_empty_optional_string")]
    pub remote_bin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "non_empty_optional_string")]
    pub default_cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandEnvironmentEntry {
    #[serde(deserialize_with = "non_empty_string")]
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "non_null_optional")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "non_null_optional")]
    pub env: Option<std::collections::HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "non_empty_optional_string")]
    pub remote_bin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "non_empty_optional_string")]
    pub default_cwd: Option<String>,
}

fn require_ssh_type<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    require_type(deserializer, "ssh")
}

fn require_docker_type<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    require_type(deserializer, "docker")
}

fn require_type<'de, D>(deserializer: D, expected: &str) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value != expected {
        return Err(serde::de::Error::custom(format!(
            "expected {expected}, got {value}"
        )));
    }
    Ok(value)
}

fn non_empty_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        return Err(serde::de::Error::custom("must not be empty"));
    }
    Ok(value)
}

fn non_empty_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    non_empty_string(deserializer).map(Some)
}

fn non_null_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Err(serde::de::Error::custom("must not be null"));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RemoteEnvironmentEntry {
    Ssh(SshEnvironmentEntry),
    Docker(DockerEnvironmentEntry),
    Command(CommandEnvironmentEntry),
}

impl RemoteEnvironmentEntry {
    pub fn default_cwd(&self) -> Option<&str> {
        match self {
            Self::Ssh(entry) => entry.default_cwd.as_deref(),
            Self::Docker(entry) => entry.default_cwd.as_deref(),
            Self::Command(entry) => entry.default_cwd.as_deref(),
        }
    }
}

pub static REMOTE_ENVIRONMENT_ENTRY_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "anyOf": [
            {
                "type": "object", "required": ["type", "host"], "additionalProperties": false,
                "properties": {
                    "type": {"const": "ssh"}, "host": {"type": "string", "minLength": 1},
                    "remoteBin": {"type": "string", "minLength": 1}, "defaultCwd": {"type": "string", "minLength": 1}
                }
            },
            {
                "type": "object", "required": ["type", "container"], "additionalProperties": false,
                "properties": {
                    "type": {"const": "docker"}, "container": {"type": "string", "minLength": 1},
                    "context": {"type": "string", "minLength": 1}, "remoteBin": {"type": "string", "minLength": 1},
                    "defaultCwd": {"type": "string", "minLength": 1}
                }
            },
            {
                "type": "object", "required": ["command"], "additionalProperties": false,
                "properties": {
                    "command": {"type": "string", "minLength": 1}, "args": {"type": "array", "items": {"type": "string"}},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}}, "remoteBin": {"type": "string", "minLength": 1},
                    "defaultCwd": {"type": "string", "minLength": 1}
                }
            }
        ]
    })
});

pub fn environment_id_problem(id: &str) -> Option<String> {
    if id.is_empty() {
        return Some("must not be empty".to_string());
    }
    if id != id.trim() {
        return Some("must not have leading or trailing whitespace".to_string());
    }
    if id.encode_utf16().count() > ENVIRONMENT_ID_MAX_LENGTH {
        return Some(format!("must be at most {ENVIRONMENT_ID_MAX_LENGTH} characters"));
    }
    if RESERVED_ENVIRONMENT_IDS.contains(&id) {
        return Some(format!("is reserved ({})", RESERVED_ENVIRONMENT_IDS.join(", ")));
    }
    None
}

pub static ENVIRONMENTS_SECTION_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {"default": {"type": "string"}},
        "additionalProperties": REMOTE_ENVIRONMENT_ENTRY_SCHEMA.clone()
    })
});

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvironmentsSection {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "non_null_optional")]
    pub default: Option<String>,
    #[serde(flatten)]
    pub entries: IndexMap<String, RemoteEnvironmentEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentSectionIssue {
    pub path: String,
    pub message: String,
}

pub fn validate_environments_section(section: &EnvironmentsSection) -> Vec<EnvironmentSectionIssue> {
    let mut issues = Vec::new();
    for id in section.entries.keys() {
        if let Some(problem) = environment_id_problem(id) {
            issues.push(EnvironmentSectionIssue {
                path: id.clone(),
                message: format!("environment id \"{id}\" {problem}"),
            });
        }
    }
    let Some(default_id) = &section.default else {
        return issues;
    };
    let Some(target) = section.entries.get(default_id) else {
        issues.push(EnvironmentSectionIssue {
            path: "default".to_string(),
            message: format!("environments.default references unconfigured environment \"{default_id}\""),
        });
        return issues;
    };
    if target.default_cwd().is_none() {
        issues.push(EnvironmentSectionIssue {
            path: "default".to_string(),
            message: format!("environments.default references environment \"{default_id}\" which does not set defaultCwd"),
        });
    }
    issues
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEnvironmentDeclaration {
    pub id: String,
    pub entry: RemoteEnvironmentEntry,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentDeclarationSet {
    pub entries: Vec<RemoteEnvironmentDeclaration>,
    pub default_id: Option<String>,
}

pub fn section_entries(section: Option<&EnvironmentsSection>) -> Vec<RemoteEnvironmentDeclaration> {
    section
        .map(|section| {
            section
                .entries
                .iter()
                .map(|(id, entry)| RemoteEnvironmentDeclaration {
                    id: id.clone(),
                    entry: entry.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentEntryType {
    Local,
    Ssh,
    Docker,
    Command,
}

#[derive(Clone)]
pub struct EnvironmentEntryInfo {
    pub environment_id: String,
    pub type_: EnvironmentEntryType,
    pub status: EnvironmentStatus,
    pub generation: String,
    pub capabilities: Vec<EnvironmentCapability>,
    pub default_cwd: Option<String>,
    pub connect_error: Option<String>,
}

pub fn environment_entry_info(
    environment: &EnvironmentGenerationSnapshot,
    entry: Option<&RemoteEnvironmentEntry>,
) -> EnvironmentEntryInfo {
    let type_ = if environment.environment_id == LOCAL_ENVIRONMENT_ID {
        EnvironmentEntryType::Local
    } else {
        match entry {
            None | Some(RemoteEnvironmentEntry::Command(_)) => EnvironmentEntryType::Command,
            Some(RemoteEnvironmentEntry::Ssh(_)) => EnvironmentEntryType::Ssh,
            Some(RemoteEnvironmentEntry::Docker(_)) => EnvironmentEntryType::Docker,
        }
    };
    EnvironmentEntryInfo {
        environment_id: environment.environment_id.clone(),
        type_,
        status: environment.status,
        generation: environment.generation.clone(),
        capabilities: environment.capabilities.clone(),
        default_cwd: entry.and_then(RemoteEnvironmentEntry::default_cwd).map(str::to_string),
        connect_error: environment.connect_error.clone(),
    }
}
