use std::path::{Component, Path, PathBuf};

use crate::os::interface::host_file_system::IHostFileSystem;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorkTree {
    pub root: String,
    pub dot_git_path: String,
    pub control_dir_path: String,
}

pub async fn find_git_work_tree(
    fs: &dyn IHostFileSystem,
    cwd: &str,
) -> Result<Option<GitWorkTree>, WireAsyncError> {
    if cwd.is_empty() || !Path::new(cwd).is_absolute() {
        return Ok(None);
    }
    let mut current = normalize_path(Path::new(cwd));
    loop {
        let dot_git_path = current.join(".git");
        if let Some(control_dir_path) = probe_git_control_dir(fs, &dot_git_path, &current).await {
            return Ok(Some(GitWorkTree {
                root: path_string(&current),
                dot_git_path: path_string(&dot_git_path),
                control_dir_path: path_string(&control_dir_path),
            }));
        }
        let Some(parent) = current.parent() else {
            return Ok(None);
        };
        if parent == current {
            return Ok(None);
        }
        current = parent.to_path_buf();
    }
}

pub fn find_local_git_work_tree(cwd: &str) -> Option<PathBuf> {
    let start = Path::new(cwd);
    if cwd.is_empty() || !start.is_absolute() {
        return None;
    }
    let mut current = start;
    loop {
        let dot_git = current.join(".git");
        if let Ok(metadata) = std::fs::metadata(&dot_git) {
            if metadata.is_dir() {
                return Some(current.to_path_buf());
            }
            if metadata.is_file()
                && std::fs::read_to_string(&dot_git)
                    .ok()
                    .is_some_and(|content| has_git_dir_pointer(&content))
            {
                return Some(current.to_path_buf());
            }
        }
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent,
            _ => return None,
        }
    }
}

fn has_git_dir_pointer(content: &str) -> bool {
    let stripped = content.strip_prefix('\u{feff}').unwrap_or(content);
    stripped
        .lines()
        .next()
        .map(str::trim)
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .is_some_and(|target| !target.is_empty())
}

async fn probe_git_control_dir(
    fs: &dyn IHostFileSystem,
    dot_git_path: &Path,
    marker_parent: &Path,
) -> Option<PathBuf> {
    let stat = fs.stat(&path_string(dot_git_path)).await.ok()?;
    if stat.is_directory {
        return Some(dot_git_path.to_path_buf());
    }
    if !stat.is_file {
        return None;
    }
    let content = fs.read_text(&path_string(dot_git_path), None).await.ok()?;
    parse_git_dir_pointer(&content, marker_parent)
}

fn parse_git_dir_pointer(content: &str, marker_parent: &Path) -> Option<PathBuf> {
    let stripped = content.strip_prefix('\u{feff}').unwrap_or(content);
    let line = stripped.trim_start().lines().next()?.trim();
    let raw_path = line.strip_prefix("gitdir:")?.trim();
    if raw_path.is_empty() {
        return None;
    }
    let raw_path = Path::new(raw_path);
    Some(normalize_path(if raw_path.is_absolute() {
        raw_path
    } else {
        &marker_parent.join(raw_path)
    }))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    output.push(component.as_os_str());
                }
            }
            Component::Normal(part) => output.push(part),
        }
    }
    output
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::find_local_git_work_tree;
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

    #[test]
    fn test_find_git_work_tree() {
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(
            find_local_git_work_tree(&plain.path().to_string_lossy()),
            None,
            "a stray temp dir is not a work tree"
        );

        let checkout = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(checkout.path().join(".git")).unwrap();
        let nested = checkout.path().join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            find_local_git_work_tree(&nested.to_string_lossy()),
            Some(checkout.path().to_path_buf()),
            "the walk starts at the cwd and finds the checkout root above it"
        );

        let linked = tempfile::tempdir().unwrap();
        std::fs::write(
            linked.path().join(".git"),
            "gitdir: /elsewhere/.git/worktrees/linked\n",
        )
        .unwrap();
        assert_eq!(
            find_local_git_work_tree(&linked.path().to_string_lossy()),
            Some(linked.path().to_path_buf()),
            "a `gitdir:` pointer file is a work tree too"
        );

        let broken = tempfile::tempdir().unwrap();
        std::fs::write(broken.path().join(".git"), "not a pointer\n").unwrap();
        assert_eq!(find_local_git_work_tree(&broken.path().to_string_lossy()), None);

        assert_eq!(find_local_git_work_tree(""), None, "no cwd, no work tree");
        assert_eq!(
            find_local_git_work_tree("relative/dir"),
            None,
            "a cwd that is not absolute has nowhere to walk up from"
        );
    }
}
