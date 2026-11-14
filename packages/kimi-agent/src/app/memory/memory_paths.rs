use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

pub use super::project_identity::{ProjectIdentity, ProjectIdentityKind, legacy_project_id_from_cwd, resolve_project_identity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    Global,
    Project,
    Session,
}

impl MemoryScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    Note,
    Decision,
    Pattern,
    Lesson,
    Reference,
}

impl MemoryType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Decision => "decision",
            Self::Pattern => "pattern",
            Self::Lesson => "lesson",
            Self::Reference => "reference",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "note" => Some(Self::Note),
            "decision" => Some(Self::Decision),
            "pattern" => Some(Self::Pattern),
            "lesson" => Some(Self::Lesson),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntry {
    pub path: String,
    pub scope: MemoryScope,
    pub scope_id: String,
    #[serde(rename = "type")]
    pub type_: MemoryType,
    pub title: String,
    pub body: String,
    pub fingerprint: String,
    pub updated_at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applies_to: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchResult {
    pub path: String,
    pub scope: MemoryScope,
    pub scope_id: String,
    #[serde(rename = "type")]
    pub type_: MemoryType,
    pub title: String,
    pub snippet: String,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMemoryPath {
    pub scope: MemoryScope,
    pub scope_id: String,
    pub file_name: String,
}

pub fn project_id_from_cwd(cwd: &str) -> String {
    resolve_project_identity(cwd).id
}

pub fn memory_dir(home_dir: &Path) -> PathBuf {
    home_dir.join("memory")
}

pub fn scope_dir(base: &Path, scope: MemoryScope, scope_id: &str) -> PathBuf {
    match scope {
        MemoryScope::Global => base.join("global"),
        MemoryScope::Project => base.join("projects").join(scope_id),
        MemoryScope::Session => base.join("sessions").join(scope_id),
    }
}

pub fn parse_memory_path(rel_path: &str) -> Option<ParsedMemoryPath> {
    let parts = rel_path.split('/').collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }
    match parts[0] {
        "global" => Some(ParsedMemoryPath {
            scope: MemoryScope::Global,
            scope_id: String::new(),
            file_name: parts[1..].join("/"),
        }),
        "projects" if parts.len() >= 3 => Some(ParsedMemoryPath {
            scope: MemoryScope::Project,
            scope_id: parts[1].to_string(),
            file_name: parts[2..].join("/"),
        }),
        "sessions" if parts.len() >= 3 => Some(ParsedMemoryPath {
            scope: MemoryScope::Session,
            scope_id: parts[1].to_string(),
            file_name: parts[2..].join("/"),
        }),
        _ => None,
    }
}

pub fn extract_title(body: &str, file_name: &str) -> String {
    static H1: OnceLock<Regex> = OnceLock::new();
    if let Some(captures) = H1.get_or_init(|| Regex::new(r"(?m)^#\s+([^\r\n\u{2028}\u{2029}]+)\r?$").expect("H1 regex")).captures(body) {
        return captures.get(1).expect("title capture").as_str().trim().to_string();
    }
    if file_name.len() >= 3 && file_name[file_name.len() - 3..].eq_ignore_ascii_case(".md") {
        file_name[..file_name.len() - 3].to_string()
    } else {
        file_name.to_string()
    }
}

pub fn detect_type(body: &str) -> MemoryType {
    static FRONT_MATTER: OnceLock<Regex> = OnceLock::new();
    if let Some(captures) = FRONT_MATTER.get_or_init(|| Regex::new(r"(?m)^---[\s\S]*?type:\s*(\w+)").expect("frontmatter regex")).captures(body)
        && let Some(type_) = captures.get(1).and_then(|value| MemoryType::parse(value.as_str()))
    {
        return type_;
    }
    static HEADING: OnceLock<Regex> = OnceLock::new();
    if let Some(captures) = HEADING.get_or_init(|| Regex::new(r"(?im)^##\s+(decision|pattern|lesson|reference)").expect("heading regex")).captures(body)
        && let Some(type_) = captures.get(1).and_then(|value| MemoryType::parse(&value.as_str().to_lowercase()))
    {
        return type_;
    }
    MemoryType::Note
}

pub fn parse_applies_to(body: &str) -> Vec<String> {
    static FRONT: OnceLock<Regex> = OnceLock::new();
    let Some(front) = FRONT.get_or_init(|| Regex::new(r"\A---[\s\S]*?\n---").expect("frontmatter block regex")).find(body) else {
        return Vec::new();
    };
    let block = front.as_str();
    static INLINE: OnceLock<Regex> = OnceLock::new();
    if let Some(captures) = INLINE.get_or_init(|| Regex::new(r"applies_to:\s*\[([^\]]*)\]").expect("inline applies_to regex")).captures(block) {
        return captures.get(1).into_iter().flat_map(|value| value.as_str().split(',')).filter_map(unquote).collect();
    }
    static LIST: OnceLock<Regex> = OnceLock::new();
    let Some(captures) = LIST.get_or_init(|| Regex::new(r"applies_to:\s*\n((?:\s*-\s*[^\n]+\n?)+)").expect("list applies_to regex")).captures(block) else {
        return Vec::new();
    };
    captures.get(1).into_iter().flat_map(|value| value.as_str().split('\n')).map(str::trim).map(|line| line.strip_prefix('-').map(|rest| rest.trim_start()).unwrap_or(line)).filter_map(unquote).collect()
}

fn unquote(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let stripped = trimmed.strip_prefix('\'').and_then(|value| value.strip_suffix('\'')).or_else(|| trimmed.strip_prefix('"').and_then(|value| value.strip_suffix('"'))).unwrap_or(trimmed);
    let cleaned = stripped.trim_matches('\'').trim_matches('"').trim();
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

pub fn build_snippet(body: &str, query: &str) -> String {
    build_snippet_with_max_len(body, query, 200)
}

pub fn build_snippet_with_max_len(body: &str, query: &str, max_len: usize) -> String {
    let lower_body = body.to_lowercase();
    let lower_query = query.to_lowercase();
    let Some(index) = lower_body.find(&lower_query) else {
        return body.chars().take(max_len).collect::<String>().trim().to_string();
    };
    let start = floor_char_boundary(body, index.saturating_sub(40).min(body.len()));
    let end = floor_char_boundary(body, (index + lower_query.len() + 80).min(body.len()));
    let start = start.min(end);
    let mut snippet = body[start..end].trim().to_string();
    if start > 0 {
        snippet.insert_str(0, "...");
    }
    if end < body.len() {
        snippet.push_str("...");
    }
    snippet
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}
