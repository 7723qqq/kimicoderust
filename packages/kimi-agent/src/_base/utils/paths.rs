use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use once_cell::sync::Lazy;
use regex::Regex;

use super::workdir_slug::workspace_root_key;

pub fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

static WINDOWS_DRIVE_ABSOLUTE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z]:[\\/]").unwrap());
static WINDOWS_UNC_ABSOLUTE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[\\/]{2}[^\\/]+[\\/][^\\/]+").unwrap());

pub fn is_windows_absolute_path(value: &str) -> bool {
    WINDOWS_DRIVE_ABSOLUTE.is_match(value) || WINDOWS_UNC_ABSOLUTE.is_match(value)
}

pub fn resolve_path(base: &str, value: &str) -> String {
    if is_windows_absolute_path(base) {
        return win32_resolve(base, value);
    }
    if is_windows_absolute_path(value) {
        return win32_resolve(value, "");
    }
    if Path::new(value).is_absolute() {
        let trailing_separator = value.len() > 1 && value.ends_with('/');
        let normalized = normalize_posix(value);
        return if trailing_separator && normalized != "/" {
            format!("{normalized}/")
        } else {
            normalized
        };
    }
    let base = if Path::new(base).is_absolute() {
        PathBuf::from(base)
    } else {
        std::env::current_dir().unwrap_or_default().join(base)
    };
    normalize_posix(&normalize_slashes(&base.join(value).to_string_lossy()))
}

pub fn canonical_path(cwd: &str) -> String {
    if is_windows_absolute_path(cwd) {
        return win32_resolve(cwd, "");
    }
    if Path::new(cwd).is_absolute() {
        return normalize_posix(cwd);
    }
    normalize_posix(&normalize_slashes(
        &std::env::current_dir()
            .unwrap_or_default()
            .join(cwd)
            .to_string_lossy(),
    ))
}

pub fn canonical_workspace_root(cwd: &str) -> String {
    let resolved = canonical_path(cwd);
    let keyed = workspace_root_key(&resolved);
    if keyed.is_empty() { resolved } else { keyed }
}

fn normalize_posix(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|segment| *segment != "..") {
                    segments.pop();
                } else if !absolute {
                    segments.push("..");
                }
            }
            _ => segments.push(segment),
        }
    }
    let body = segments.join("/");
    if absolute {
        format!("/{body}")
    } else if body.is_empty() {
        ".".to_string()
    } else {
        body
    }
}

fn win32_resolve(base: &str, value: &str) -> String {
    let combined = if is_windows_absolute_path(value) {
        value.to_string()
    } else if value.is_empty() {
        base.to_string()
    } else if value.starts_with('/') || value.starts_with('\\') {
        let normalized_base = normalize_slashes(base);
        if normalized_base.starts_with("//") {
            let mut parts = normalized_base[2..].split('/');
            let server = parts.next().unwrap_or_default();
            let share = parts.next().unwrap_or_default();
            format!("//{server}/{share}{value}")
        } else if normalized_base.len() >= 2 && normalized_base.as_bytes()[1] == b':' {
            format!("{}{}", &normalized_base[..2], value)
        } else {
            value.to_string()
        }
    } else {
        format!("{base}/{value}")
    };
    let combined = normalize_slashes(&combined);
    let (prefix, rest) = if combined.starts_with("//") {
        let mut parts = combined[2..].splitn(3, '/');
        let server = parts.next().unwrap_or_default();
        let share = parts.next().unwrap_or_default();
        (format!("//{server}/{share}"), parts.next().unwrap_or_default().to_string())
    } else {
        let bytes = combined.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' {
            (combined[..2].to_string(), combined[2..].to_string())
        } else {
            (String::new(), combined.clone())
        }
    };
    let mut segments = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    let body = segments.join("/");
    if prefix.is_empty() {
        normalize_posix(&body)
    } else if body.is_empty() {
        format!("{prefix}/")
    } else {
        format!("{prefix}/{body}")
    }
}

pub trait UpwardRootPathApi: Send + Sync {
    fn resolve(&self, dir: &str) -> String;
    fn dirname(&self, dir: &str) -> String;
    fn join(&self, segments: &[String]) -> String;
}

pub struct NodePathApi;

impl UpwardRootPathApi for NodePathApi {
    fn resolve(&self, dir: &str) -> String {
        canonical_path(dir)
    }

    fn dirname(&self, dir: &str) -> String {
        Path::new(dir)
            .parent()
            .unwrap_or(Path::new(dir))
            .to_string_lossy()
            .into_owned()
    }

    fn join(&self, segments: &[String]) -> String {
        segments
            .iter()
            .fold(PathBuf::new(), |path, segment| path.join(segment))
            .to_string_lossy()
            .into_owned()
    }
}

pub fn find_upward_root(
    work_dir: &str,
    marker_name: &str,
    has_marker: Arc<dyn Fn(String) -> BoxFuture<'static, bool> + Send + Sync>,
    path_api: Arc<dyn UpwardRootPathApi>,
) -> BoxFuture<'static, String> {
    let start = path_api.resolve(work_dir);
    let marker_name = marker_name.to_string();
    async move {
        let mut current = start.clone();
        loop {
            if has_marker(path_api.join(&[current.clone(), marker_name.clone()])).await {
                return normalize_slashes(&current);
            }
            let parent = path_api.dirname(&current);
            if parent == current {
                return normalize_slashes(&start);
            }
            current = parent;
        }
    }
    .boxed()
}

#[derive(Clone, Default)]
pub struct SubtreeWatchFilterOptions {
    pub max_depth: Option<usize>,
    pub skip_entry: Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>,
    pub keep_entry_file: Option<String>,
    pub scanned_directories: Option<Vec<String>>,
}

pub fn subtree_watch_filter(
    root: &str,
    candidates: &[String],
    options: Option<SubtreeWatchFilterOptions>,
) -> Box<dyn Fn(&str) -> bool + Send + Sync> {
    let norm_root = normalize_slashes(root);
    let norm_candidates = candidates.iter().map(|path| normalize_slashes(path)).collect::<Vec<_>>();
    let norm_scanned_directories = options
        .as_ref()
        .and_then(|options| options.scanned_directories.as_ref())
        .map(|directories| {
            norm_candidates
                .iter()
                .cloned()
                .chain(directories.iter().map(|path| normalize_slashes(path)))
                .collect::<HashSet<_>>()
        });
    Box::new(move |path| {
        let norm = normalize_slashes(path);
        if norm == norm_root {
            return false;
        }
        for candidate in &norm_candidates {
            if norm == *candidate {
                return false;
            }
            if norm.starts_with(&format!("{candidate}/")) {
                return is_pruned_below_candidate(
                    &norm,
                    &norm[candidate.len() + 1..],
                    options.as_ref(),
                    norm_scanned_directories.as_ref(),
                );
            }
            if candidate.starts_with(&format!("{norm}/")) {
                return false;
            }
        }
        true
    })
}

fn is_pruned_below_candidate(
    norm_path: &str,
    rel: &str,
    options: Option<&SubtreeWatchFilterOptions>,
    scanned_directories: Option<&HashSet<String>>,
) -> bool {
    let Some(options) = options else {
        return false;
    };
    let segments = rel.split('/').collect::<Vec<_>>();
    if options
        .max_depth
        .is_some_and(|max_depth| segments.len() > max_depth)
    {
        return true;
    }
    if let Some(skip_entry) = &options.skip_entry
        && let Some(excluded_at) = segments.iter().position(|segment| skip_entry(segment))
    {
        if segments.len() <= excluded_at + 1 {
            return false;
        }
        return !(options.keep_entry_file.is_some()
            && segments.len() == excluded_at + 2
            && segments.last().copied() == options.keep_entry_file.as_deref());
    }
    if let Some(scanned_directories) = scanned_directories {
        return !is_scanner_visible_path(
            norm_path,
            scanned_directories,
            options.keep_entry_file.as_deref(),
        );
    }
    false
}

fn is_scanner_visible_path(
    norm_path: &str,
    scanned_directories: &HashSet<String>,
    keep_entry_file: Option<&str>,
) -> bool {
    if scanned_directories.contains(norm_path) {
        return true;
    }
    let Some(separator_at) = norm_path.rfind('/') else {
        return false;
    };
    let parent = &norm_path[..separator_at];
    if scanned_directories.contains(parent) {
        return true;
    }
    if keep_entry_file != Some(&norm_path[separator_at + 1..]) {
        return false;
    }
    let Some(parent_separator_at) = parent.rfind('/') else {
        return false;
    };
    scanned_directories.contains(&parent[..parent_separator_at])
}

use futures_util::FutureExt;

pub fn is_absolute(path: &str) -> bool {
    is_windows_absolute_path(path) || Path::new(path).is_absolute()
}

pub fn join<S: AsRef<str>>(base: &str, segments: &[S]) -> String {
    let mut path = PathBuf::from(base);
    for segment in segments {
        path.push(segment.as_ref());
    }
    normalize_slashes(&path.to_string_lossy())
}

pub fn resolve(base: &str, path: &str) -> String {
    resolve_path(base, path)
}

pub fn dirname(path: &str) -> String {
    match Path::new(path).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            normalize_slashes(&parent.to_string_lossy())
        }
        _ => ".".to_string(),
    }
}

pub fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn relative(from: &str, to: &str) -> String {
    let from_components: Vec<_> = Path::new(from).components().collect();
    let to_components: Vec<_> = Path::new(to).components().collect();
    let common = from_components
        .iter()
        .zip(to_components.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts: Vec<String> = Vec::new();
    for _ in common..from_components.len() {
        parts.push("..".to_string());
    }
    for component in &to_components[common..] {
        parts.push(component.as_os_str().to_string_lossy().into_owned());
    }
    parts.join("/")
}
