use std::collections::HashMap;
use std::sync::LazyLock;

use crate::environment::environment::{
    EnvironmentPath, EnvironmentWorkspaceMapper, EnvironmentWorkspaceRoots,
};
use crate::environment::environment_registry::EnvironmentError;
use crate::os::interface::host_environment::HostEnvironmentInfo;
use crate::wire::record::WireAsyncError;

pub fn normalize_remote_cwd(cwd: &str) -> Result<String, WireAsyncError> {
    let path_class = if is_windows_drive_absolute(cwd) {
        "win32"
    } else {
        "posix"
    };
    if cwd.contains('\0') || !is_absolute(path_class, cwd) {
        return Err(Box::new(EnvironmentError::new(
            "environment.invalid_cwd",
            "remote cwd must be an absolute POSIX or Windows path; relative paths and ~ are not expanded",
            None,
        )));
    }
    Ok(resolve(path_class, cwd, ""))
}

pub static DEFAULT_ENVIRONMENT_HOST: LazyLock<HostEnvironmentInfo> =
    LazyLock::new(|| HostEnvironmentInfo {
        os_kind: "unknown".to_string(),
        os_arch: "unknown".to_string(),
        os_version: String::new(),
        shell_name: "sh".to_string(),
        shell_path: "/bin/sh".to_string(),
        path_class: "posix".to_string(),
        home_dir: "/".to_string(),
        cwd: None,
        temp_dir: None,
        js_runtimes: None,
    });

pub static ENVIRONMENT_PATHS: LazyLock<HashMap<String, EnvironmentPath>> = LazyLock::new(|| {
    HashMap::from([
        ("posix".to_string(), EnvironmentPath::new("posix")),
        ("win32".to_string(), EnvironmentPath::new("win32")),
    ])
});

pub static ENVIRONMENT_WORKSPACES: LazyLock<HashMap<String, EnvironmentWorkspaceMapper>> =
    LazyLock::new(|| {
        HashMap::from([
            (
                "posix".to_string(),
                EnvironmentWorkspaceMapper::new("posix"),
            ),
            (
                "win32".to_string(),
                EnvironmentWorkspaceMapper::new("win32"),
            ),
        ])
    });

pub static POSIX_ENVIRONMENT_PATH: LazyLock<EnvironmentPath> =
    LazyLock::new(|| EnvironmentPath::new("posix"));
pub static POSIX_ENVIRONMENT_WORKSPACE: LazyLock<EnvironmentWorkspaceMapper> =
    LazyLock::new(|| EnvironmentWorkspaceMapper::new("posix"));

pub(crate) fn separator(path_class: &str) -> &'static str {
    if path_class == "win32" { "\\" } else { "/" }
}

pub(crate) fn is_absolute(path_class: &str, value: &str) -> bool {
    if path_class == "win32" {
        is_windows_drive_absolute(value)
            || value.starts_with(['\\', '/'])
            || is_unc_absolute(value)
    } else {
        value.starts_with('/')
    }
}

pub(crate) fn join(path_class: &str, base: &str, parts: &[&str]) -> String {
    let mut value = base.to_string();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        if !value.is_empty() && !value.ends_with(['\\', '/']) {
            value.push_str(separator(path_class));
        }
        value.push_str(part);
    }
    normalize(path_class, &value)
}

pub(crate) fn resolve(path_class: &str, base: &str, other: &str) -> String {
    let resolved = if !other.is_empty() && is_absolute(path_class, other) {
        normalize(path_class, other)
    } else if is_absolute(path_class, base) {
        join(path_class, base, &[other])
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        let cwd = cwd.to_string_lossy().into_owned();
        join(path_class, &join(path_class, &cwd, &[base]), &[other])
    };
    let parsed = path_parts(path_class, &resolved);
    if parsed.rooted && parsed.components.is_empty() {
        resolved
    } else {
        resolved.trim_end_matches(['\\', '/']).to_string()
    }
}

pub(crate) fn relative(path_class: &str, from: &str, to: &str) -> String {
    let from = resolve(path_class, from, "");
    let to = resolve(path_class, to, "");
    let from_parts = path_parts(path_class, &from);
    let to_parts = path_parts(path_class, &to);
    if from_parts.prefix != to_parts.prefix
        && (path_class != "win32"
            || !from_parts.prefix.eq_ignore_ascii_case(&to_parts.prefix))
    {
        return to;
    }
    let mut common = 0usize;
    while common < from_parts.components.len()
        && common < to_parts.components.len()
        && component_eq(
            path_class,
            &from_parts.components[common],
            &to_parts.components[common],
        )
    {
        common += 1;
    }
    let mut output = Vec::new();
    for _ in common..from_parts.components.len() {
        output.push("..".to_string());
    }
    output.extend(to_parts.components[common..].iter().cloned());
    output.join(separator(path_class))
}

pub(crate) fn basename(path_class: &str, value: &str) -> String {
    path_parts(path_class, value)
        .components
        .last()
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn dirname(path_class: &str, value: &str) -> String {
    let normalized = normalize(path_class, value);
    let parsed = path_parts(path_class, &normalized);
    if parsed.components.is_empty() {
        return normalized;
    }
    if parsed.components.len() == 1 && parsed.prefix.is_empty() && !parsed.rooted {
        return ".".to_string();
    }
    let mut parent = parsed.clone();
    parent.components.pop();
    render(path_class, &parent)
}

pub(crate) fn map_roots(
    path_class: &str,
    roots: &EnvironmentWorkspaceRoots,
) -> EnvironmentWorkspaceRoots {
    EnvironmentWorkspaceRoots {
        work_dir: resolve(path_class, &roots.work_dir, ""),
        primary_dir: roots
            .primary_dir
            .as_deref()
            .map(|dir| resolve(path_class, dir, "")),
        additional_dirs: roots.additional_dirs.as_ref().map(|dirs| {
            dirs.iter()
                .map(|dir| resolve(path_class, dir, ""))
                .collect()
        }),
        follows_primary: roots.follows_primary,
    }
}

#[derive(Clone)]
struct ParsedPath {
    prefix: String,
    rooted: bool,
    components: Vec<String>,
}

fn path_parts(path_class: &str, value: &str) -> ParsedPath {
    let unified = if path_class == "win32" {
        value.replace('/', "\\")
    } else {
        value.replace('\\', "/")
    };
    let separator = separator(path_class);
    let mut prefix = String::new();
    let mut rooted = false;
    let mut rest = unified.as_str();
    if path_class == "win32" {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            prefix = rest[..2].to_string();
            rest = &rest[2..];
            if rest.starts_with('\\') {
                rooted = true;
                rest = rest.trim_start_matches('\\');
            }
        } else if rest.starts_with("\\\\") {
            prefix = "\\\\".to_string();
            rooted = true;
            rest = rest.trim_start_matches('\\');
        } else if rest.starts_with('\\') {
            rooted = true;
            rest = rest.trim_start_matches('\\');
        }
    } else if rest.starts_with('/') {
        rooted = true;
        rest = rest.trim_start_matches('/');
    }
    let mut components = Vec::new();
    for component in rest.split(separator) {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            if components.last().is_some_and(|part| part != "..") {
                components.pop();
            } else if !rooted {
                components.push(component.to_string());
            }
        } else {
            components.push(component.to_string());
        }
    }
    ParsedPath {
        prefix,
        rooted,
        components,
    }
}

fn render(path_class: &str, parsed: &ParsedPath) -> String {
    let separator = separator(path_class);
    let mut output = parsed.prefix.clone();
    if parsed.rooted && !output.ends_with(['\\', '/']) {
        output.push_str(separator);
    }
    output.push_str(&parsed.components.join(separator));
    if output.is_empty() {
        if parsed.rooted {
            separator.to_string()
        } else {
            ".".to_string()
        }
    } else {
        output
    }
}

fn normalize(path_class: &str, value: &str) -> String {
    if value.is_empty() {
        return ".".to_string();
    }
    let parsed = path_parts(path_class, value);
    let rendered = render(path_class, &parsed);
    if value.ends_with(['\\', '/']) && rendered != separator(path_class) {
        format!("{rendered}{}", separator(path_class))
    } else {
        rendered
    }
}

fn component_eq(path_class: &str, left: &str, right: &str) -> bool {
    if path_class == "win32" {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn is_windows_drive_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

fn is_unc_absolute(value: &str) -> bool {
    (value.starts_with("\\\\") || value.starts_with("//")) && value.len() > 2
}
