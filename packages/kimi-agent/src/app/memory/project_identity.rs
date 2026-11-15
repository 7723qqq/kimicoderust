use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::_base::utils::workdir_slug::workspace_root_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectIdentityKind {
    #[serde(rename = "git-remote")]
    GitRemote,
    #[serde(rename = "git-repo")]
    GitRepo,
    #[serde(rename = "path")]
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectIdentity {
    pub kind: ProjectIdentityKind,
    pub id: String,
    pub detail: String,
}

struct GitLocation {
    root: PathBuf,
    common_dir: PathBuf,
}

fn digest(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())[..12].to_string()
}

fn read_text_if_present(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn resolve(base: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn resolve_git_dir(marker: &Path) -> Option<PathBuf> {
    if marker.is_dir() {
        return Some(marker.to_path_buf());
    }
    let content = read_text_if_present(marker)?;
    static GIT_DIR: OnceLock<Regex> = OnceLock::new();
    let captures = GIT_DIR.get_or_init(|| Regex::new(r"(?m)^gitdir:\s*(.+)$").expect("gitdir regex")).captures(&content)?;
    let value = captures.get(1)?.as_str().trim();
    Some(resolve(marker.parent()?, value))
}

fn common_dir_of(git_dir: &Path) -> PathBuf {
    let Some(content) = read_text_if_present(&git_dir.join("commondir")) else {
        return git_dir.to_path_buf();
    };
    let value = content.trim();
    if value.is_empty() {
        git_dir.to_path_buf()
    } else {
        resolve(git_dir, value)
    }
}

fn absolute(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn find_git_location(start: &str) -> Option<GitLocation> {
    let mut dir = absolute(start);
    loop {
        let marker = dir.join(".git");
        if marker.exists()
            && let Some(git_dir) = resolve_git_dir(&marker)
        {
            return Some(GitLocation {
                root: dir,
                common_dir: common_dir_of(&git_dir),
            });
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn origin_url(common_dir: &Path) -> Option<String> {
    let config = read_text_if_present(&common_dir.join("config"))?;
    let mut in_origin = false;
    for raw_line in config.split('\n') {
        let line = raw_line.trim();
        if line.starts_with('[') {
            in_origin = line == r#"[remote "origin"]"#;
            continue;
        }
        if !in_origin {
            continue;
        }
        static URL: OnceLock<Regex> = OnceLock::new();
        if let Some(captures) = URL.get_or_init(|| Regex::new(r"^url\s*=\s*(.+)$").expect("URL regex")).captures(line) {
            return captures.get(1).map(|value| value.as_str().trim().to_string());
        }
    }
    None
}

pub fn normalize_remote_url(raw: &str) -> Option<String> {
    let mut value = raw.trim().to_string();
    if value.is_empty() {
        return None;
    }
    static WINDOWS_PATH: OnceLock<Regex> = OnceLock::new();
    if WINDOWS_PATH.get_or_init(|| Regex::new(r"^[A-Za-z]:[\\/]").expect("Windows path regex")).is_match(&value) || value.starts_with("\\\\") {
        return None;
    }
    static SCP: OnceLock<Regex> = OnceLock::new();
    let scp = SCP.get_or_init(|| Regex::new(r"^(?:[^@/\\]+@)?([^:/\\]+):(.+)$").expect("scp regex")).captures(&value);
    if let Some(captures) = scp.filter(|_| !value.contains("://")) {
        value = format!("{}/{}", captures.get(1)?.as_str(), captures.get(2)?.as_str());
    } else {
        static SCHEME: OnceLock<Regex> = OnceLock::new();
        value = SCHEME.get_or_init(|| Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*://(?:[^@/]*@)?").expect("scheme regex")).replace(&value, "").into_owned();
    }
    value = value.trim_start_matches('/').trim_end_matches('/').to_string();
    if value.len() >= 4 && value[value.len() - 4..].eq_ignore_ascii_case(".git") {
        value.truncate(value.len() - 4);
    }
    if value.is_empty() {
        return None;
    }
    if let Some(slash) = value.find('/')
        && slash > 0
    {
        value = format!("{}{}", value[..slash].to_lowercase(), &value[slash..]);
    }
    Some(value)
}

pub fn resolve_project_identity(cwd: &str) -> ProjectIdentity {
    if let Some(location) = find_git_location(cwd) {
        let normalized = origin_url(&location.common_dir).as_deref().and_then(normalize_remote_url);
        if let Some(normalized) = normalized {
            return ProjectIdentity {
                kind: ProjectIdentityKind::GitRemote,
                id: digest(&format!("git:{normalized}")),
                detail: normalized,
            };
        }
        let repo_key = workspace_root_key(&location.root.to_string_lossy());
        return ProjectIdentity {
            kind: ProjectIdentityKind::GitRepo,
            id: digest(&format!("repo:{repo_key}")),
            detail: repo_key,
        };
    }
    let path_key = workspace_root_key(cwd);
    ProjectIdentity {
        kind: ProjectIdentityKind::Path,
        id: digest(&path_key),
        detail: path_key,
    }
}

pub fn legacy_project_id_from_cwd(cwd: &str) -> String {
    digest(&workspace_root_key(cwd))
}
