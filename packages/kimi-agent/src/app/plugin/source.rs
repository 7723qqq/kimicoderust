use std::path::Path;

use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::core::VALIDATION_FAILED;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use url::Url;

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GithubRefKind {
    Branch,
    Tag,
    Sha,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubRef {
    pub kind: GithubRefKind,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ResolvedSource {
    LocalPath { path: String },
    ZipUrl { path: String },
    Github {
        owner: String,
        repo: String,
        #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
        ref_: Option<GithubRef>,
    },
}

pub type InstallSource = ResolvedSource;

pub fn resolve_install_source(source: &str) -> Result<ResolvedSource, WireAsyncError> {
    let trimmed = source.trim();
    if let Some(github) = parse_github_url(trimmed) {
        return Ok(github);
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(ResolvedSource::ZipUrl {
            path: trimmed.to_string(),
        });
    }
    if !Path::new(trimmed).is_absolute() {
        let mut details = Map::new();
        details.insert("source".to_string(), Value::String(source.to_string()));
        return Err(Box::new(
            Error2::new(
                VALIDATION_FAILED,
                format!("Plugin root must be an absolute path (got \"{source}\")"),
            )
            .with_details(details),
        ));
    }
    Ok(ResolvedSource::LocalPath {
        path: trimmed.to_string(),
    })
}

fn parse_github_url(raw: &str) -> Option<ResolvedSource> {
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    if !matches!(url.host_str()?, "github.com" | "www.github.com") {
        return None;
    }
    let segments = url
        .path()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let owner = (*segments.first()?).to_string();
    let repo_raw = *segments.get(1)?;
    let repo = repo_raw.strip_suffix(".git").unwrap_or(repo_raw).to_string();
    let rest = &segments[2..];
    if rest.is_empty() {
        return Some(ResolvedSource::Github {
            owner,
            repo,
            ref_: None,
        });
    }
    let head = rest[0];
    let second = rest.get(1).copied();
    let ref_ = if head == "tree" && rest.len() >= 2 {
        let value = decode_ref_segments(&rest[1..]);
        GithubRef {
            kind: if is_sha(&value) {
                GithubRefKind::Sha
            } else {
                GithubRefKind::Branch
            },
            value,
        }
    } else if head == "releases" && second == Some("tag") && rest.len() >= 3 {
        GithubRef {
            kind: GithubRefKind::Tag,
            value: decode_ref_segments(&rest[2..]),
        }
    } else if head == "commit" && rest.len() >= 2 {
        GithubRef {
            kind: GithubRefKind::Sha,
            value: decode_ref_segments(&rest[1..]),
        }
    } else {
        return None;
    };
    Some(ResolvedSource::Github {
        owner,
        repo,
        ref_: Some(ref_),
    })
}

fn is_sha(value: &str) -> bool {
    (7..=40).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decode_ref_segments(segments: &[&str]) -> String {
    segments
        .iter()
        .map(|segment| decode_uri_component(segment).unwrap_or_else(|| (*segment).to_string()))
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn error_details(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

pub(crate) fn json_value(value: impl Serialize) -> Value {
    json!(value)
}
