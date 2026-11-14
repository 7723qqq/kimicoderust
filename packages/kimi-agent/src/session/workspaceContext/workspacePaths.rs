use crate::environment::environment::EnvironmentPath;
use crate::mcp_core::errors::MCP_STARTUP_FAILED;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::fs::FS_PATH_ESCAPES;

use super::workspace_context::PathAccessOperation;

#[derive(Clone)]
pub struct WorkspacePathSemantics {
    pub is_absolute: fn(&str) -> bool,
    pub relative: fn(&str, &str) -> String,
    pub resolve: fn(&str, &str) -> String,
    pub resolve_single: fn(&str) -> String,
}

pub fn host_workspace_path_semantics() -> WorkspacePathSemantics {
    WorkspacePathSemantics {
        is_absolute: |path| std::path::Path::new(path).is_absolute(),
        relative: |base, target| {
            let base = std::path::Path::new(base);
            let target = std::path::Path::new(target);
            target
                .strip_prefix(base)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| target.to_string_lossy().into_owned())
        },
        resolve: |base, target| {
            let mut path = std::path::Path::new(base).to_path_buf();
            path.push(target);
            path.to_string_lossy().into_owned()
        },
        resolve_single: |path| {
            std::path::Path::new(path)
                .canonicalize()
                .unwrap_or_else(|_| std::path::Path::new(path).to_path_buf())
                .to_string_lossy()
                .into_owned()
        },
    }
}

pub fn resolve_workspace_path(
    path: &WorkspacePathSemantics,
    work_dir: &str,
    rel: &str,
) -> String {
    if (path.is_absolute)(rel) {
        (path.resolve_single)(rel)
    } else {
        (path.resolve)(work_dir, rel)
    }
}

pub fn is_within_workspace(
    path: &WorkspacePathSemantics,
    work_dir: &str,
    additional_dirs: &[String],
    abs_path: &str,
) -> bool {
    let target = (path.resolve_single)(abs_path);
    if target == work_dir {
        return true;
    }
    let rel = (path.relative)(work_dir, &target);
    if !rel.is_empty() && !rel.starts_with("..") && !(path.is_absolute)(&rel) {
        return true;
    }
    additional_dirs.iter().any(|dir| {
        let rel = (path.relative)(dir, &target);
        rel.is_empty() || (!rel.starts_with("..") && !(path.is_absolute)(&rel))
    })
}

pub fn assert_workspace_allowed(
    path: &WorkspacePathSemantics,
    work_dir: &str,
    additional_dirs: &[String],
    abs_path: &str,
    op: PathAccessOperation,
) -> Result<String, WireAsyncError> {
    let target = resolve_workspace_path(path, work_dir, abs_path);
    if !is_within_workspace(path, work_dir, additional_dirs, &target) {
        let mut details = serde_json::Map::new();
        details.insert("op".to_string(), op.as_str().into());
        details.insert("path".to_string(), target.clone().into());
        return Err(Box::new(Error2::with_options(
            FS_PATH_ESCAPES,
            format!("Path outside workspace ({}): {target}", op.as_str()),
            kimi_core::primitives::errors::Error2Options {
                details: Some(details),
                ..Default::default()
            },
        )));
    }
    Ok(target)
}

#[allow(dead_code)]
fn _environment_path_marker(_: &EnvironmentPath) {}
