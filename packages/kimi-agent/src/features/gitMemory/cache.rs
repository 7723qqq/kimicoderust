use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::types::{CommitRecord, GIT_MEMORY_INDEX_VERSION, GitMemoryIndex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexReadResult {
    Hit(GitMemoryIndex),
    Miss,
    Corrupt,
}

pub fn cache_path(store_dir: &Path, repo_root: &str) -> PathBuf {
    let normalized = repo_root.replace('\\', "/");
    let digest = Sha256::digest(normalized.as_bytes());
    let digest = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    store_dir.join("git-memory").join(format!("{digest}.json"))
}

pub fn build_index(
    repo_root: String,
    head_sha: String,
    window: usize,
    commits: Vec<CommitRecord>,
    computed_at: String,
    dirs: Vec<String>,
) -> GitMemoryIndex {
    let mut by_path = IndexMap::<String, Vec<usize>>::new();
    for (index, commit) in commits.iter().enumerate() {
        for file in &commit.files {
            by_path.entry(file.clone()).or_default().push(index);
        }
    }
    GitMemoryIndex {
        version: GIT_MEMORY_INDEX_VERSION,
        repo_root,
        head_sha,
        window,
        computed_at,
        commits,
        by_path,
        dirs,
    }
}

fn stable_stringify(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).unwrap(),
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(stable_stringify).collect::<Vec<_>>().join(",")
        ),
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| {
                left.encode_utf16().cmp(right.encode_utf16())
            });
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| {
                        format!("{}:{}", serde_json::to_string(key).unwrap(), stable_stringify(value))
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

pub async fn read_index(path: &Path, head_sha: &str, window: usize) -> IndexReadResult {
    let Ok(raw) = tokio::fs::read(path).await else {
        return IndexReadResult::Miss;
    };
    let raw = String::from_utf8_lossy(&raw);
    let Ok(mut parsed) = serde_json::from_str::<Value>(&raw) else {
        return IndexReadResult::Corrupt;
    };
    if !parsed.is_object() {
        return IndexReadResult::Corrupt;
    }
    if parsed.get("version").and_then(Value::as_f64) != Some(f64::from(GIT_MEMORY_INDEX_VERSION))
        || parsed.get("headSha").and_then(Value::as_str) != Some(head_sha)
        || parsed.get("window").and_then(Value::as_f64) != Some(window as f64)
    {
        return IndexReadResult::Miss;
    }
    normalize_index_numbers(&mut parsed);
    match serde_json::from_value(parsed) {
        Ok(index) => IndexReadResult::Hit(index),
        Err(_) => IndexReadResult::Corrupt,
    }
}

pub async fn read_any_index(path: &Path) -> Option<GitMemoryIndex> {
    let raw = tokio::fs::read(path).await.ok()?;
    let raw = String::from_utf8_lossy(&raw);
    let mut parsed = serde_json::from_str::<Value>(&raw).ok()?;
    if !parsed.is_object()
        || parsed.get("version").and_then(Value::as_f64)
            != Some(f64::from(GIT_MEMORY_INDEX_VERSION))
    {
        return None;
    }
    normalize_index_numbers(&mut parsed);
    serde_json::from_value(parsed).ok()
}

fn normalize_index_numbers(parsed: &mut Value) {
    let Some(index) = parsed.as_object_mut() else {
        return;
    };
    normalize_integer(index.get_mut("version"));
    normalize_integer(index.get_mut("window"));
    let Some(by_path) = index.get_mut("byPath").and_then(Value::as_object_mut) else {
        return;
    };
    for indices in by_path.values_mut().filter_map(Value::as_array_mut) {
        for index in indices {
            normalize_integer(Some(index));
        }
    }
}

fn normalize_integer(value: Option<&mut Value>) {
    let Some(value) = value else {
        return;
    };
    let Some(number) = value.as_f64().filter(|number| {
        number.is_finite() && *number >= 0.0 && number.fract() == 0.0
    }) else {
        return;
    };
    *value = Value::from(number as u64);
}

pub fn legacy_cache_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".kimi-code").join("git-memory.json")
}

pub async fn remove_legacy_index(repo_root: &Path) -> bool {
    let path = legacy_cache_path(repo_root);
    if read_any_index(&path).await.is_none() {
        return false;
    }
    match tokio::fs::remove_file(path).await {
        Ok(()) => true,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

pub async fn write_index(path: &Path, index: &GitMemoryIndex) -> bool {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if tokio::fs::create_dir_all(parent).await.is_err() {
        return false;
    }
    let Ok(value) = serde_json::to_value(index) else {
        return false;
    };
    tokio::fs::write(path, format!("{}\n", stable_stringify(&value)))
        .await
        .is_ok()
}
