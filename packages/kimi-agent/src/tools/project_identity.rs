//! v2 `app/memory/projectIdentity.ts` + `_base/utils/workdir-slug.ts`:
//! project identity for the memory layout.
//!
//! v2 does not digest the cwd directly. It resolves an identity in three
//! tiers so that the same repository keeps its memory across a path change:
//!
//! 1. a normalized `origin` remote URL inside the discovered git dir;
//! 2. failing that, the git repository root;
//! 3. failing that, the cwd itself.
//!
//! Each tier's key is hashed to the first 12 hex chars of SHA-256, the same
//! length this engine's memory layout uses. This module is the pure port;
//! `memory_paths::project_id_from_cwd` remains the cwd-digest form until the
//! call sites are switched over one by one.

use std::path::PathBuf;

use regex::Regex;
use sha2::{Digest, Sha256};

/// The three kinds of identity v2 reports (`ProjectIdentityKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectIdentityKind {
    GitRemote,
    GitRepo,
    Path,
}

impl ProjectIdentityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitRemote => "git-remote",
            Self::GitRepo => "git-repo",
            Self::Path => "path",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    pub kind: ProjectIdentityKind,
    pub id: String,
    pub detail: String,
}

fn digest(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let bytes = hasher.finalize();
    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()[..12].to_string()
}

/// v2 `workspaceRootKey`: normalize separators and trailing slashes, and
/// lowercase Windows-shaped paths (drive letters and UNC) because Windows
/// filesystems are case-insensitive.
pub fn workspace_root_key(root: &str) -> String {
    let slashed = root.replace('\\', "/");
    let shaped = Regex::new(r"^(?:[A-Za-z]:[\\/]|\\\\|//)")
        .expect("static win-shaped regex")
        .is_match(&slashed);
    let normalized = slashed.trim_end_matches('/');
    if shaped {
        normalized.to_lowercase()
    } else {
        normalized.to_string()
    }
}

/// v2 `normalizeRemoteUrl` (`projectIdentity.ts:88-103`): strip credentials
/// and transport, fold SCP syntax, drop `.git`, and lowercase only the host
/// portion. Local paths and UNC paths are refused.
pub fn normalize_remote_url(raw: &str) -> Option<String> {
    let mut value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if Regex::new(r"^[A-Za-z]:[\\/]")
        .expect("static drive regex")
        .is_match(value)
        || value.starts_with("\\\\")
    {
        return None;
    }

    static SCP: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let scp = SCP
        .get_or_init(|| Regex::new(r"^(?:[^@/\\]+@)?([^:/\\]+):(.+)$").expect("static scp regex"));
    static SCHEME: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let scheme = SCHEME.get_or_init(|| {
        Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*://(?:[^@/]*@)?").expect("static scheme regex")
    });

    let mut owned;
    if let Some(caps) = scp.captures(value)
        && !value.contains("://")
        && let (Some(host), Some(path)) = (caps.get(1), caps.get(2))
    {
        owned = format!("{}/{}", host.as_str(), path.as_str());
    } else {
        owned = scheme.replace(value, "").into_owned();
    }
    value = &owned;
    owned = value
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string();
    if owned.ends_with(".git") {
        owned.truncate(owned.len() - 4);
    }
    if owned.is_empty() {
        return None;
    }
    if let Some(slash) = owned.find('/')
        && slash > 0
    {
        let (host, rest) = owned.split_at(slash);
        owned = format!("{}{}", host.to_lowercase(), rest);
    }
    Some(owned)
}

/// Read `remote "origin"`'s url out of a git config file. v2 scans section
/// headers and takes the first `url =` line inside that section.
fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line == r#"[remote "origin"]"#;
            continue;
        }
        if !in_origin {
            continue;
        }
        if let Some(rest) = line.strip_prefix("url") {
            let rest = rest.trim_start();
            if let Some(url) = rest.strip_prefix('=') {
                return Some(url.trim().to_string());
            }
        }
    }
    None
}

struct GitLocation {
    root: PathBuf,
    common_dir: PathBuf,
}

/// v2 `findGitLocation`: walk up from cwd, accept a `.git` directory or a
/// `.git` file whose `gitdir:` pointer resolves, and follow `commondir` for
/// worktrees.
fn find_git_location(start: &str) -> Option<GitLocation> {
    let mut dir = PathBuf::from(start);
    if !dir.is_absolute() {
        dir = std::env::current_dir().ok()?.join(dir);
    }
    loop {
        let marker = dir.join(".git");
        let git_dir = if marker.is_dir() {
            Some(marker.clone())
        } else if marker.is_file() {
            let content = std::fs::read_to_string(&marker).ok()?;
            let pointer = content
                .lines()
                .find_map(|line| line.trim().strip_prefix("gitdir:"))
                .map(str::trim)?;
            Some(marker.parent()?.join(pointer))
        } else {
            None
        };
        if let Some(git_dir) = git_dir {
            let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
                .ok()
                .map(|text| {
                    let value = text.trim();
                    if value.is_empty() {
                        git_dir.clone()
                    } else {
                        git_dir.join(value)
                    }
                })
                .unwrap_or_else(|| git_dir.clone());
            return Some(GitLocation {
                root: dir,
                common_dir,
            });
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// v2 `resolveProjectIdentity` (`projectIdentity.ts:105-118`).
pub fn resolve_project_identity(cwd: &str) -> ProjectIdentity {
    if let Some(location) = find_git_location(cwd) {
        let config = std::fs::read_to_string(location.common_dir.join("config")).ok();
        if let Some(url) = config.as_deref().and_then(origin_url)
            && let Some(normalized) = normalize_remote_url(&url)
        {
            let id = digest(&format!("git:{normalized}"));
            return ProjectIdentity {
                kind: ProjectIdentityKind::GitRemote,
                id,
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
        id: digest(&path_key),
        detail: path_key,
        kind: ProjectIdentityKind::Path,
    }
}

/// v2 `legacyProjectIdFromCwd`: the pre-identity digest of the workspace key.
pub fn legacy_project_id_from_cwd(cwd: &str) -> String {
    digest(&workspace_root_key(cwd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_root_key_normalizes_separators_and_windows_case() {
        assert_eq!(workspace_root_key("/home/User/Proj/"), "/home/User/Proj");
        assert_eq!(workspace_root_key("G:\\Kimi\\Repo\\"), "g:/kimi/repo");
        assert_eq!(workspace_root_key("\\\\SRV\\Share\\"), "//srv/share");
    }

    #[test]
    fn remote_urls_are_normalized_the_way_v2_does() {
        assert_eq!(
            normalize_remote_url("https://USER@github.com/MoonshotAI/kimi-code.git"),
            Some("github.com/MoonshotAI/kimi-code".to_string())
        );
        assert_eq!(
            normalize_remote_url("git@github.com:MoonshotAI/kimi-code.git"),
            Some("github.com/MoonshotAI/kimi-code".to_string())
        );
        assert_eq!(
            normalize_remote_url("ssh://git@EXAMPLE.com:2222/Org/Repo.git/"),
            Some("example.com:2222/Org/Repo".to_string())
        );
    }

    #[test]
    fn local_and_empty_remotes_are_refused() {
        assert_eq!(normalize_remote_url(r"C:\repo"), None);
        assert_eq!(normalize_remote_url(r"\\srv\share"), None);
        assert_eq!(normalize_remote_url("   "), None);
    }

    #[test]
    fn origin_url_only_reads_the_origin_section() {
        let config = r#"
[remote "upstream"]
    url = https://example.com/upstream.git
[remote "origin"]
    url = https://example.com/origin.git
"#;
        assert_eq!(
            origin_url(config).as_deref(),
            Some("https://example.com/origin.git")
        );
        assert_eq!(origin_url("[core]\n bare = false"), None);
    }

    #[test]
    fn a_plain_directory_uses_the_path_tier() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy();
        let identity = resolve_project_identity(&cwd);
        assert_eq!(identity.kind, ProjectIdentityKind::Path);
        assert_eq!(identity.id, digest(&workspace_root_key(&cwd)));
        assert_eq!(identity.detail, workspace_root_key(&cwd));
    }

    #[test]
    fn a_git_repo_without_origin_uses_its_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "[core]\n bare = false\n").unwrap();
        let identity = resolve_project_identity(&dir.path().to_string_lossy());
        assert_eq!(identity.kind, ProjectIdentityKind::GitRepo);
        let expected = digest(&format!(
            "repo:{}",
            workspace_root_key(&dir.path().to_string_lossy())
        ));
        assert_eq!(identity.id, expected);
    }

    #[test]
    fn a_git_repo_with_origin_uses_the_normalized_url() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".git/config"),
            "[remote \"origin\"]\n url = https://user@GitHub.com/Org/Repo.git\n",
        )
        .unwrap();
        let identity = resolve_project_identity(&dir.path().to_string_lossy());
        assert_eq!(identity.kind, ProjectIdentityKind::GitRemote);
        assert_eq!(identity.detail, "github.com/Org/Repo");
        assert_eq!(identity.id, digest("git:github.com/Org/Repo"));
    }

    #[test]
    fn the_legacy_id_is_the_bare_workspace_key_digest() {
        assert_eq!(
            legacy_project_id_from_cwd("/home/user/project/"),
            digest("/home/user/project")
        );
    }

    #[test]
    fn a_nested_directory_finds_the_ancestor_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".git/config"),
            "[remote \"origin\"]\n url = https://example.com/Org/Repo.git\n",
        )
        .unwrap();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let identity = resolve_project_identity(&nested.to_string_lossy());
        assert_eq!(identity.kind, ProjectIdentityKind::GitRemote);
        assert_eq!(identity.detail, "example.com/Org/Repo");
    }

    #[test]
    fn a_gitdir_file_and_commondir_are_followed() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join("worktree");
        let actual = dir.path().join("actual");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&actual).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", actual.to_string_lossy()),
        )
        .unwrap();
        std::fs::write(actual.join("commondir"), "../shared\n").unwrap();
        std::fs::create_dir_all(dir.path().join("shared")).unwrap();
        std::fs::write(
            dir.path().join("shared/config"),
            "[remote \"origin\"]\n url = git@github.com:Org/Repo.git\n",
        )
        .unwrap();
        let identity = resolve_project_identity(&worktree.to_string_lossy());
        assert_eq!(identity.kind, ProjectIdentityKind::GitRemote);
        assert_eq!(identity.detail, "github.com/Org/Repo");
    }
}
