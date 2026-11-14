#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    Posix,
    Win32,
}

impl PathClass {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "posix" => Some(Self::Posix),
            "win32" => Some(Self::Win32),
            _ => None,
        }
    }
}

pub const SENSITIVE_DOT_VARIANT_SUFFIXES: &[&str] = &[
    ".bak", ".backup", ".copy", ".disabled", ".key", ".old", ".orig", ".pem", ".save", ".tmp",
];

pub fn is_sensitive_file(path: &str) -> bool {
    const SENSITIVE_BASENAMES: &[&str] = &[".env", "id_rsa", "id_ed25519", "id_ecdsa", "credentials"];
    const ENV_EXEMPTIONS: &[&str] = &[".env.example", ".env.sample", ".env.template"];
    const PUBLIC_KEY_BASENAMES: &[&str] = &["id_rsa.pub", "id_ed25519.pub", "id_ecdsa.pub"];
    const SENSITIVE_BASENAME_PREFIXES: &[&str] = &["id_rsa", "id_ed25519", "id_ecdsa", "credentials"];

    let normalized = path.replace('\\', "/");
    let name = normalized
        .split('/')
        .rev()
        .find(|segment| !segment.is_empty())
        .unwrap_or_default();
    let comparable_name = name.to_lowercase();
    let comparable_path = path.to_lowercase();
    if ENV_EXEMPTIONS.contains(&comparable_name.as_str())
        || PUBLIC_KEY_BASENAMES.contains(&comparable_name.as_str())
    {
        return false;
    }
    if SENSITIVE_BASENAMES.contains(&comparable_name.as_str())
        || comparable_name.starts_with(".env.")
    {
        return true;
    }
    for prefix in SENSITIVE_BASENAME_PREFIXES {
        if comparable_name == *prefix {
            return true;
        }
        if comparable_name.len() > prefix.len() && comparable_name.starts_with(prefix) {
            let suffix = &comparable_name[prefix.len()..];
            if matches!(suffix.as_bytes().first(), Some(&b'-') | Some(&b'_'))
                || (suffix.starts_with('.') && SENSITIVE_DOT_VARIANT_SUFFIXES.contains(&suffix))
            {
                return true;
            }
        }
    }
    [".aws/credentials", ".gcp/credentials"]
        .iter()
        .any(|suffix| {
            comparable_path.ends_with(&format!("/{suffix}"))
                || comparable_path.contains(&format!("/{suffix}/"))
        })
}

pub fn is_project_local_config_path(path: &str) -> bool {
    path.replace('\\', "/")
        .to_lowercase()
        .ends_with("/.kimi-code/local.toml")
}

pub fn normalize_user_path(path: &str, path_class: PathClass) -> String {
    if path_class != PathClass::Win32 {
        return path.to_string();
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && (bytes.len() == 3 || matches!(bytes[3], b'/' | b'\\'))
    {
        return join_drive(bytes[1], &path[3..]);
    }
    if bytes.len() >= 11
        && path.starts_with("/cygdrive/")
        && bytes[10].is_ascii_alphabetic()
        && (bytes.len() == 11 || bytes[11] == b'/')
    {
        return join_drive(bytes[10], &path[11..]);
    }
    if bytes.len() >= 2
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && (bytes.len() == 2 || bytes[2] == b'/')
    {
        return join_drive(bytes[1], &path[2..]);
    }
    path.to_string()
}

fn join_drive(drive: u8, rest: &str) -> String {
    let drive = (drive as char).to_ascii_uppercase();
    let rest = rest.replace('\\', "/");
    if rest.is_empty() {
        format!("{drive}:/")
    } else {
        format!("{drive}:{rest}")
    }
}

pub fn expand_user_path(path: &str, home_dir: Option<&str>, path_class: PathClass) -> String {
    let Some(home) = home_dir else {
        return path.to_string();
    };
    if path == "~" {
        return home.to_string();
    }
    if path.starts_with("~/") || (path_class == PathClass::Win32 && path.starts_with("~\\")) {
        return join_paths(&[home, &path[2..]]);
    }
    path.to_string()
}

pub fn canonicalize_path(path: &str, cwd: &str, path_class: PathClass) -> Result<String, String> {
    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }
    let normalized_path = normalize_user_path(path, path_class);
    if path_class == PathClass::Win32 && is_win32_drive_relative(&normalized_path) {
        return Err(format!(
            "\"{path}\" is a drive-relative Windows path. \
             Use an absolute path like C:\\path or a path relative to the working directory."
        ));
    }
    if !is_absolute(&normalized_path) && !is_absolute(cwd) {
        return Err(format!(
            "Cannot resolve \"{path}\" against non-absolute cwd \"{cwd}\"."
        ));
    }
    let absolute = if is_absolute(&normalized_path) {
        normalized_path
    } else {
        resolve_path(cwd, &normalized_path)
    };
    Ok(normalize_path(&absolute, path_class))
}

fn is_win32_drive_relative(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || (bytes[2] != b'\\' && bytes[2] != b'/'))
}

fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
    {
        return true;
    }
    if !matches!(bytes.first(), Some(&b'/') | Some(&b'\\')) {
        return false;
    }
    if bytes.len() == 1 || !matches!(bytes[1], b'/' | b'\\') {
        return true;
    }
    bytes.get(2) != Some(&b'.')
}

fn normalize_windows_path(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    let bytes = normalized.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        let upper = (bytes[0] as char).to_ascii_uppercase();
        normalized.replace_range(0..1, &upper.to_string());
    }
    normalized
}

fn join_paths(segments: &[&str]) -> String {
    let mut path = String::new();
    for segment in segments.iter().copied().filter(|segment| !segment.is_empty()) {
        if path.is_empty() {
            path.push_str(segment);
            continue;
        }
        let trailing = path.ends_with('/');
        let leading = segment.starts_with('/');
        match (trailing, leading) {
            (true, true) => path.push_str(&segment[1..]),
            (false, false) => {
                path.push('/');
                path.push_str(segment);
            }
            _ => path.push_str(segment),
        }
    }
    normalize_path(&path, PathClass::Posix)
}

fn resolve_path(cwd: &str, path: &str) -> String {
    let cwd = normalize_windows_path(cwd);
    let path = normalize_windows_path(path);
    let combined = format!("{cwd}/{path}");
    let absolute = is_absolute(&cwd);
    let resolved = normalize_string(&combined, !absolute);
    if absolute && !is_absolute(&resolved) {
        format!("/{resolved}")
    } else if resolved.is_empty() {
        ".".to_string()
    } else {
        resolved
    }
}

fn normalize_string(path: &str, allow_above_root: bool) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|segment| *segment != "..") {
                    segments.pop();
                } else if allow_above_root {
                    segments.push("..");
                }
            }
            _ => segments.push(segment),
        }
    }
    segments.join("/")
}

pub(crate) fn normalize_path(path: &str, _path_class: PathClass) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let normalized = normalize_windows_path(path);
    let unc = normalized.starts_with("//");
    let absolute = is_absolute(&normalized);
    let trailing_separator = normalized.ends_with('/');
    let mut result = normalize_string(&normalized, !absolute);
    if result.is_empty() {
        return if absolute {
            "/".to_string()
        } else if trailing_separator {
            "./".to_string()
        } else {
            ".".to_string()
        };
    }
    if trailing_separator {
        result.push('/');
    }
    if result.len() == 2
        && result.as_bytes()[0].is_ascii_alphabetic()
        && result.as_bytes()[1] == b':'
    {
        result.push('/');
    }
    if unc {
        return if absolute {
            format!("//{result}")
        } else {
            format!("//./{result}")
        };
    }
    if absolute && !is_absolute(&result) {
        format!("/{result}")
    } else {
        result
    }
}

pub fn is_within_directory(candidate: &str, base: &str, path_class: PathClass) -> bool {
    let nc = normalize_path(candidate, path_class);
    let nb = normalize_path(base, path_class);
    let (comp_c, comp_b) = if path_class == PathClass::Win32 {
        (nc.to_lowercase(), nb.to_lowercase())
    } else {
        (nc, nb)
    };
    if comp_c == comp_b {
        return true;
    }
    let prefix = if comp_b.ends_with('/') {
        comp_b.clone()
    } else {
        format!("{comp_b}/")
    };
    comp_c.starts_with(&prefix)
}

pub fn is_within_workspace(candidate: &str, roots: &[String], path_class: PathClass) -> bool {
    for root in roots {
        if is_within_directory(candidate, root, path_class) {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub workspace_dir: String,
    pub additional_dirs: Vec<String>,
}

pub fn extend_workspace_with_skill_roots(
    workspace: &WorkspaceConfig,
    skill_roots: &[String],
    path_class: PathClass,
) -> WorkspaceConfig {
    let mut additional_dirs = workspace.additional_dirs.clone();
    for root in skill_roots {
        if is_within_directory(root, &workspace.workspace_dir, path_class)
            || additional_dirs
                .iter()
                .any(|dir| is_within_directory(root, dir, path_class))
        {
            continue;
        }
        additional_dirs.push(root.clone());
    }
    if additional_dirs.len() == workspace.additional_dirs.len() {
        return workspace.clone();
    }
    WorkspaceConfig {
        workspace_dir: workspace.workspace_dir.clone(),
        additional_dirs,
    }
}

pub async fn is_within_directory_resolved<F, Fut, E>(
    candidate: &str,
    base: &str,
    mut resolver: F,
    path_class: PathClass,
) -> Option<bool>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, E>>,
{
    if !is_within_directory(candidate, base, path_class) {
        return Some(false);
    }
    let resolved_base = resolve_existing(&mut resolver, normalize_path(base, path_class)).await?;
    let resolved_candidate =
        resolve_deepest_existing(&mut resolver, normalize_path(candidate, path_class)).await?;
    Some(is_within_directory(
        &resolved_candidate,
        &resolved_base,
        path_class,
    ))
}

async fn resolve_existing<F, Fut, E>(resolver: &mut F, path: String) -> Option<String>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, E>>,
{
    resolver(path).await.ok()
}

async fn resolve_deepest_existing<F, Fut, E>(resolver: &mut F, path: String) -> Option<String>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, E>>,
{
    const MAX_RESOLVE_DEPTH: usize = 64;
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        if let Some(resolved) = resolve_existing(resolver, current.clone()).await {
            if missing.is_empty() {
                return Some(resolved);
            }
            let mut segments = vec![resolved.as_str()];
            segments.extend(missing.iter().rev().map(String::as_str));
            return Some(join_paths(&segments));
        }
        let parent = dirname(&current);
        if parent == current {
            return None;
        }
        missing.push(basename(&current));
        if missing.len() > MAX_RESOLVE_DEPTH {
            return None;
        }
        current = parent;
    }
}

fn dirname(path: &str) -> String {
    let normalized = normalize_windows_path(path);
    let trimmed = normalized.strip_suffix('/').unwrap_or(&normalized);
    let Some(index) = trimmed.rfind('/') else {
        return (if is_absolute(path) { "/" } else { "." }).to_string();
    };
    let parent = &trimmed[..index];
    if parent.is_empty() {
        return (if is_absolute(path) { "/" } else { "." }).to_string();
    }
    if parent.len() == 2
        && parent.as_bytes()[0].is_ascii_alphabetic()
        && parent.as_bytes()[1] == b':'
    {
        format!("{parent}/")
    } else {
        parent.to_string()
    }
}

fn basename(path: &str) -> String {
    normalize_windows_path(path)
        .split('/')
        .rev()
        .find(|segment| !segment.is_empty())
        .unwrap_or_default()
        .to_string()
}

pub fn is_within_workspace_posix(
    candidate: &str,
    workspace_dir: &str,
    additional_dirs: &[String],
) -> bool {
    let absolute = if candidate.starts_with('/') {
        candidate.to_string()
    } else {
        format!("{}/{}", workspace_dir.trim_end_matches('/'), candidate)
    };
    is_within_directory_posix(&absolute, workspace_dir)
        || additional_dirs
            .iter()
            .any(|dir| is_within_directory_posix(&absolute, dir))
}

pub fn is_within_directory_posix(candidate: &str, base: &str) -> bool {
    let candidate_parts = normalized_posix_parts(candidate);
    let base_parts = normalized_posix_parts(base);
    !base_parts.is_empty()
        && candidate_parts.len() >= base_parts.len()
        && candidate_parts[..base_parts.len()] == base_parts[..]
}

fn normalized_posix_parts(path: &str) -> Vec<String> {
    let unified = path.replace('\\', "/");
    let mut parts: Vec<String> = Vec::new();
    for segment in unified.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_string()),
        }
    }
    if unified.starts_with('/') {
        parts.insert(0, "/".to_string());
    }
    parts
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMode {
    AbsoluteOutsideAllowed,

    #[allow(dead_code)]
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceAccessPolicy {
    pub guard_mode: GuardMode,
    pub check_sensitive: bool,
}

pub const DEFAULT_WORKSPACE_ACCESS_POLICY: WorkspaceAccessPolicy = WorkspaceAccessPolicy {
    guard_mode: GuardMode::AbsoluteOutsideAllowed,
    check_sensitive: true,
};

impl Default for WorkspaceAccessPolicy {
    fn default() -> Self {
        DEFAULT_WORKSPACE_ACCESS_POLICY
    }
}

impl WorkspaceAccessPolicy {
    pub const SEARCH: Self = Self {
        guard_mode: GuardMode::AbsoluteOutsideAllowed,
        check_sensitive: false,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccessOperation {
    Read,
    Write,

    Search,
}

impl PathAccessOperation {
    fn verb(self) -> &'static str {
        match self {
            Self::Read => "read a file",
            Self::Write => "write or edit a file",
            Self::Search => "search",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSecurityCode {
    OutsideWorkspace,

    Sensitive,

    Invalid,

    SymlinkEscape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSecurityError {
    pub code: PathSecurityCode,
    pub raw_path: String,
    pub canonical_path: String,
    pub message: String,
}

impl PathSecurityError {
    fn new(code: PathSecurityCode, raw_path: &str, canonical_path: &str, message: String) -> Self {
        Self {
            code,
            raw_path: raw_path.to_string(),
            canonical_path: canonical_path.to_string(),
            message,
        }
    }

    pub fn symlink_escape(raw_path: &str, canonical_path: &str, message: String) -> Self {
        Self::new(
            PathSecurityCode::SymlinkEscape,
            raw_path,
            canonical_path,
            message,
        )
    }

    pub fn symlink_sensitive(raw_path: &str, resolved_path: &str) -> Self {
        Self::new(
            PathSecurityCode::Sensitive,
            raw_path,
            resolved_path,
            format!(
                "\"{raw_path}\" resolves to \"{resolved_path}\" through a symbolic link, \
                 which matches a sensitive-file pattern (env / credential / SSH key). \
                 Access is blocked to protect secrets."
            ),
        )
    }
}

pub fn enforce_path_access(
    raw_path: &str,
    canonical: &str,
    raw_is_absolute: bool,
    roots: &[String],
    path_class: PathClass,
    operation: PathAccessOperation,
    policy: &WorkspaceAccessPolicy,
) -> Result<bool, PathSecurityError> {
    let outside_workspace = !is_within_workspace(canonical, roots, path_class);

    if policy.check_sensitive && is_sensitive_file(canonical) {
        return Err(PathSecurityError {
            code: PathSecurityCode::Sensitive,
            raw_path: raw_path.to_string(),
            canonical_path: canonical.to_string(),
            message: format!(
                "\"{raw_path}\" matches a sensitive-file pattern \
                 (env / credential / SSH key). Access is blocked to protect secrets."
            ),
        });
    }

    if outside_workspace
        && policy.guard_mode == GuardMode::AbsoluteOutsideAllowed
        && !raw_is_absolute
    {
        return Err(PathSecurityError {
            code: PathSecurityCode::OutsideWorkspace,
            raw_path: raw_path.to_string(),
            canonical_path: canonical.to_string(),
            message: format!(
                "\"{raw_path}\" is not an absolute path. \
                 You must provide an absolute path to {} outside the working directory.",
                operation.verb()
            ),
        });
    }

    Ok(outside_workspace)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathAccess {
    pub path: String,
    pub outside_workspace: bool,
}

pub fn resolve_path_access(
    raw_path: &str,
    cwd: &str,
    roots: &[String],
    home_dir: Option<&str>,
    path_class: PathClass,
    operation: PathAccessOperation,
    policy: &WorkspaceAccessPolicy,
) -> Result<PathAccess, PathSecurityError> {
    resolve_path_access_inner(
        raw_path,
        cwd,
        roots,
        home_dir,
        path_class,
        operation,
        policy,
        None,
    )
}

fn resolve_path_access_inner(
    raw_path: &str,
    cwd: &str,
    roots: &[String],
    home_dir: Option<&str>,
    path_class: PathClass,
    operation: PathAccessOperation,
    policy: &WorkspaceAccessPolicy,
    shell_path_bridge: Option<&dyn Fn(&str) -> String>,
) -> Result<PathAccess, PathSecurityError> {
    let normalized = match shell_path_bridge {
        Some(from_shell_path) => from_shell_path(raw_path),
        None => normalize_user_path(raw_path, path_class),
    };
    let expanded = expand_user_path(&normalized, home_dir, path_class);
    let raw_is_absolute = is_absolute(&expanded);
    let canonical_path = normalize_user_path(&expanded, path_class);
    let canonical =
        canonicalize_path(&expanded, cwd, path_class).map_err(|message| PathSecurityError {
            code: PathSecurityCode::Invalid,
            raw_path: expanded.clone(),
            canonical_path,
            message,
        })?;
    let outside_workspace = enforce_path_access(
        raw_path,
        &canonical,
        raw_is_absolute,
        roots,
        path_class,
        operation,
        policy,
    )?;
    Ok(PathAccess {
        path: canonical,
        outside_workspace,
    })
}

pub struct ResolvePathAccessPathOptions<'a> {
    pub path_class: PathClass,
    pub home_dir: Option<&'a str>,
    pub shell_path_bridge: Option<&'a dyn Fn(&str) -> String>,
    pub workspace: &'a WorkspaceConfig,
    pub operation: PathAccessOperation,
    pub policy: Option<&'a WorkspaceAccessPolicy>,
    pub expand_home: Option<bool>,
}

pub fn resolve_path_access_path(
    path: &str,
    options: &ResolvePathAccessPathOptions<'_>,
) -> Result<String, PathSecurityError> {
    let roots = std::iter::once(options.workspace.workspace_dir.clone())
        .chain(options.workspace.additional_dirs.iter().cloned())
        .collect::<Vec<_>>();
    resolve_path_access_inner(
        path,
        &options.workspace.workspace_dir,
        &roots,
        if options.expand_home.unwrap_or(true) {
            options.home_dir
        } else {
            None
        },
        options.path_class,
        options.operation,
        options.policy.unwrap_or(&DEFAULT_WORKSPACE_ACCESS_POLICY),
        options.shell_path_bridge,
    )
    .map(|access| access.path)
}

pub fn assert_path_allowed(
    path: &str,
    cwd: &str,
    workspace: &WorkspaceConfig,
    mode: PathAccessOperation,
    check_sensitive: Option<bool>,
    path_class: PathClass,
) -> Result<String, PathSecurityError> {
    let roots = std::iter::once(workspace.workspace_dir.clone())
        .chain(workspace.additional_dirs.iter().cloned())
        .collect::<Vec<_>>();
    let policy = WorkspaceAccessPolicy {
        guard_mode: GuardMode::AbsoluteOutsideAllowed,
        check_sensitive: check_sensitive
            .unwrap_or(DEFAULT_WORKSPACE_ACCESS_POLICY.check_sensitive),
    };
    resolve_path_access(
        path,
        cwd,
        &roots,
        None,
        path_class,
        mode,
        &policy,
    )
    .map(|access| access.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_user_path_posix() {
        assert_eq!(
            normalize_user_path("/foo/bar", PathClass::Posix),
            "/foo/bar"
        );
        assert_eq!(
            normalize_user_path("relative", PathClass::Posix),
            "relative"
        );
    }

    #[test]
    fn test_normalize_user_path_win32_cygdrive() {
        assert_eq!(
            normalize_user_path("/cygdrive/c/path", PathClass::Win32),
            "C:/path"
        );
        assert_eq!(normalize_user_path("/cygdrive/z", PathClass::Win32), "Z:/");
    }

    #[test]
    fn test_normalize_user_path_win32_drive() {
        assert_eq!(normalize_user_path("/c/path", PathClass::Win32), "C:/path");
        assert_eq!(normalize_user_path("/z", PathClass::Win32), "Z:/");
    }

    #[test]
    fn test_normalize_user_path_win32_bare_root() {
        assert_eq!(normalize_user_path("/", PathClass::Win32), "/");
        assert_eq!(
            normalize_user_path("//server", PathClass::Win32),
            "//server"
        );
    }

    #[test]
    fn test_expand_user_path_posix() {
        assert_eq!(
            expand_user_path("~/foo", Some("/home/user"), PathClass::Posix),
            "/home/user/foo"
        );
        assert_eq!(
            expand_user_path("~", Some("/home/user"), PathClass::Posix),
            "/home/user"
        );
        assert_eq!(
            expand_user_path("/abs", Some("/home/user"), PathClass::Posix),
            "/abs"
        );
    }

    #[test]
    fn test_expand_user_path_win32() {
        assert_eq!(
            expand_user_path("~\\foo", Some("C:\\User"), PathClass::Win32),
            "C:/User/foo"
        );
    }

    #[test]
    fn test_canonicalize_empty() {
        assert!(canonicalize_path("", "/cwd", PathClass::Posix).is_err());
    }

    #[test]
    fn test_canonicalize_drive_relative_win32() {
        assert!(canonicalize_path("C:path", "C:\\cwd", PathClass::Win32).is_err());
    }

    #[test]
    fn test_canonicalize_relative() {
        assert_eq!(
            canonicalize_path("foo/bar", "/cwd", PathClass::Posix).unwrap(),
            "/cwd/foo/bar"
        );
        assert_eq!(
            canonicalize_path("./foo", "/cwd", PathClass::Posix).unwrap(),
            "/cwd/foo"
        );
    }

    #[test]
    fn test_canonicalize_dotdot() {
        assert_eq!(
            canonicalize_path("foo/../bar", "/cwd", PathClass::Posix).unwrap(),
            "/cwd/bar"
        );
        assert_eq!(
            canonicalize_path("../bar", "/cwd/sub", PathClass::Posix).unwrap(),
            "/cwd/bar"
        );
    }

    #[test]
    fn test_canonicalize_already_absolute() {
        assert_eq!(
            canonicalize_path("/foo/bar", "/cwd", PathClass::Posix).unwrap(),
            "/foo/bar"
        );
    }

    #[test]
    fn test_is_absolute_non_ascii_win32() {
        assert!(!is_absolute("中文路径/文件"));
        assert!(!is_absolute("日本語"));
        assert!(is_absolute("C:/foo"));
        assert!(is_absolute("/foo"));
        assert!(!is_absolute("foo"));
    }

    #[test]
    fn test_is_within_directory_exact() {
        assert!(is_within_directory(
            "/workspace/file",
            "/workspace",
            PathClass::Posix
        ));
    }

    #[test]
    fn test_is_within_directory_descendant() {
        assert!(is_within_directory(
            "/workspace/sub/file",
            "/workspace",
            PathClass::Posix
        ));
    }

    #[test]
    fn test_is_within_directory_shared_prefix_escape() {
        assert!(!is_within_directory(
            "/workspace-evil",
            "/workspace",
            PathClass::Posix
        ));
        assert!(!is_within_directory(
            "/workspace/sub/../../../etc/passwd",
            "/workspace",
            PathClass::Posix
        ));
    }

    #[test]
    fn test_is_within_directory_win32_case() {
        assert!(is_within_directory(
            "C:/Workspace/File",
            "c:/workspace",
            PathClass::Win32
        ));
    }

    #[test]
    fn test_canonicalize_win32_unc_preserved() {
        assert_eq!(
            canonicalize_path("\\\\server\\share\\file", "/cwd", PathClass::Win32).unwrap(),
            "//server/share/file"
        );
        assert_eq!(
            canonicalize_path("//server/share/file", "/cwd", PathClass::Win32).unwrap(),
            "//server/share/file"
        );

        assert_eq!(
            canonicalize_path("\\\\?\\C:\\very\\long\\path", "/cwd", PathClass::Win32).unwrap(),
            "//?/C:/very/long/path"
        );

        assert_eq!(
            canonicalize_path("C:/workspace/./a/../b", "/cwd", PathClass::Win32).unwrap(),
            "C:/workspace/b"
        );
    }

    #[test]
    fn test_is_within_directory_win32_unc() {
        let candidate = "\\\\server\\share\\work\\file";
        let base = "\\\\server\\share\\work";
        assert!(is_within_directory(candidate, base, PathClass::Win32));
        assert!(!is_within_directory(
            "\\\\server\\shared\\file",
            base,
            PathClass::Win32
        ));
    }

    #[test]
    fn test_canonicalize_untouched_pass() {
        assert_eq!(
            canonicalize_path("//server", "/cwd", PathClass::Win32).unwrap(),
            "//server"
        );
        assert_eq!(
            canonicalize_path("/cwd", "/cwd", PathClass::Posix).unwrap(),
            "/cwd"
        );
    }

    #[test]
    fn test_canonicalize_for_glob_match() {
        assert_eq!(
            canonicalize_path("./src/**", "/workspace", PathClass::Posix).unwrap(),
            "/workspace/src/**"
        );
        assert_eq!(
            canonicalize_path("/workspace/src/a.ts", "/workspace", PathClass::Posix).unwrap(),
            "/workspace/src/a.ts"
        );
    }

    #[test]
    fn test_is_within_workspace_multi_root() {
        let roots = vec!["/primary".to_string(), "/secondary".to_string()];
        assert!(is_within_workspace(
            "/primary/file",
            &roots,
            PathClass::Posix
        ));
        assert!(is_within_workspace(
            "/secondary/file",
            &roots,
            PathClass::Posix
        ));
        assert!(!is_within_workspace(
            "/other/file",
            &roots,
            PathClass::Posix
        ));
    }

    fn ws() -> Vec<String> {
        vec!["/workspace".to_string()]
    }

    #[test]
    fn test_gate_refuses_sensitive_inside_workspace() {
        let err = enforce_path_access(
            "/workspace/.env",
            "/workspace/.env",
            true,
            &ws(),
            PathClass::Posix,
            PathAccessOperation::Read,
            &WorkspaceAccessPolicy::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, PathSecurityCode::Sensitive);
        assert_eq!(
            err.message,
            "\"/workspace/.env\" matches a sensitive-file pattern \
             (env / credential / SSH key). Access is blocked to protect secrets."
        );
    }

    #[test]
    fn test_gate_sensitive_matches_v2_shapes() {
        for path in [
            "/workspace/.env",
            "/workspace/.env.local",
            "/workspace/.ssh/id_rsa",
            "/workspace/.aws/credentials",
        ] {
            let err = enforce_path_access(
                path,
                path,
                true,
                &ws(),
                PathClass::Posix,
                PathAccessOperation::Read,
                &WorkspaceAccessPolicy::default(),
            )
            .unwrap_err();
            assert_eq!(err.code, PathSecurityCode::Sensitive, "{path}");
        }
    }

    #[test]
    fn test_gate_honours_sensitive_exemptions() {
        for path in [
            "/workspace/.env.example",
            "/workspace/.env.template",
            "/workspace/.ssh/id_rsa.pub",
        ] {
            let outside = enforce_path_access(
                path,
                path,
                true,
                &ws(),
                PathClass::Posix,
                PathAccessOperation::Read,
                &WorkspaceAccessPolicy::default(),
            )
            .unwrap_or_else(|e| panic!("{path} must not be refused: {}", e.message));
            assert!(!outside, "{path}");
        }
    }

    #[test]
    fn test_gate_refuses_relative_path_outside_workspace() {
        let err = enforce_path_access(
            "../../etc/passwd",
            "/etc/passwd",
            false,
            &ws(),
            PathClass::Posix,
            PathAccessOperation::Read,
            &WorkspaceAccessPolicy::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, PathSecurityCode::OutsideWorkspace);
        assert_eq!(
            err.message,
            "\"../../etc/passwd\" is not an absolute path. \
             You must provide an absolute path to read a file outside the working directory."
        );
    }

    #[test]
    fn test_gate_absolute_path_outside_workspace_is_allowed() {
        let outside = enforce_path_access(
            "/etc/passwd",
            "/etc/passwd",
            true,
            &ws(),
            PathClass::Posix,
            PathAccessOperation::Read,
            &WorkspaceAccessPolicy::default(),
        )
        .expect("absolute outside path is allowed by the gate");
        assert!(outside);
    }

    #[test]
    fn a_bridged_shell_path_resolves_and_reports_outside_workspace() {
        let access = resolve_path_access(
            "C:/Temp/notes.txt",
            "C:\\workspace",
            &["C:\\workspace".to_string()],
            None,
            PathClass::Win32,
            PathAccessOperation::Read,
            &WorkspaceAccessPolicy::default(),
        )
        .expect("an absolute path outside the workspace is allowed by the gate");

        assert_eq!(access.path, r"C:/Temp/notes.txt");
        assert!(
            access.outside_workspace,
            "a path outside every root is reported, not silently treated as inside"
        );
    }

    #[test]
    fn test_gate_write_and_search_verbs() {
        let err = |op| {
            enforce_path_access(
                "../x",
                "/x",
                false,
                &ws(),
                PathClass::Posix,
                op,
                &WorkspaceAccessPolicy::default(),
            )
            .unwrap_err()
            .message
        };
        assert!(err(PathAccessOperation::Write).ends_with(
            "You must provide an absolute path to write or edit a file outside the working directory."
        ));
        assert!(err(PathAccessOperation::Search).ends_with(
            "You must provide an absolute path to search outside the working directory."
        ));
    }

    #[test]
    fn test_gate_additional_dirs_count_as_inside() {
        let roots = vec!["/workspace".to_string(), "/srv/data".to_string()];
        let outside = enforce_path_access(
            "../srv/data/file.txt",
            "/srv/data/file.txt",
            false,
            &roots,
            PathClass::Posix,
            PathAccessOperation::Read,
            &WorkspaceAccessPolicy::default(),
        )
        .expect("additionalDirs root is inside");
        assert!(!outside);
    }

    #[test]
    fn test_gate_search_policy_does_not_refuse_sensitive() {
        let outside = enforce_path_access(
            "/workspace/.env",
            "/workspace/.env",
            true,
            &ws(),
            PathClass::Posix,
            PathAccessOperation::Search,
            &WorkspaceAccessPolicy::SEARCH,
        )
        .expect("search policy leaves the sensitive check to result filtering");
        assert!(!outside);
    }

    #[test]
    fn test_gate_disabled_mode_skips_containment() {
        let policy = WorkspaceAccessPolicy {
            guard_mode: GuardMode::Disabled,
            check_sensitive: true,
        };
        let outside = enforce_path_access(
            "../x",
            "/x",
            false,
            &ws(),
            PathClass::Posix,
            PathAccessOperation::Read,
            &policy,
        )
        .expect("disabled guard mode refuses nothing");
        assert!(outside);
    }

    #[test]
    fn test_gate_sensitive_precedes_outside_workspace() {
        let err = enforce_path_access(
            "../.env",
            "/other/.env",
            false,
            &ws(),
            PathClass::Posix,
            PathAccessOperation::Read,
            &WorkspaceAccessPolicy::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, PathSecurityCode::Sensitive);
    }

#[test]
    fn test_workspace_containment_is_component_wise() {
        assert!(is_within_directory_posix("/repo/src/lib.rs", "/repo"));
        assert!(is_within_directory_posix("/repo", "/repo"));
        assert!(!is_within_directory_posix("/repo2/src/lib.rs", "/repo"));
        assert!(!is_within_directory_posix("/other/repo/x", "/repo"));
        assert!(is_within_directory_posix("/repo/a/../b/x.rs", "/repo/b"));
        assert!(!is_within_directory_posix("/repo/../elsewhere/x.rs", "/repo"));
        assert!(
            !is_within_directory_posix("/repo/x", "repo"),
            "the leading `/` is its own component, so an absolute path is never \
             inside the relative base of the same name"
        );
        assert!(
            !is_within_directory_posix("repo/src/lib.rs", "/repo"),
            "a relative candidate is not the absolute path it resolves to"
        );
        assert!(
            !is_within_directory_posix("/repo/x", ""),
            "an empty base has no components, so nothing is inside it"
        );

        let extras = vec!["/granted".to_string()];
        assert!(is_within_workspace_posix("/repo/x.rs", "/repo", &extras));
        assert!(
            is_within_workspace_posix("/granted/x.rs", "/repo", &extras),
            "an /add-dir root is inside the boundary"
        );
        assert!(is_within_workspace_posix("/granted/sub/x.rs", "/repo", &extras));
        assert!(!is_within_workspace_posix("/granted2/x.rs", "/repo", &extras));
        assert!(
            is_within_workspace_posix("src/lib.rs", "/repo", &extras),
            "a relative target resolves against the workspace cwd"
        );
        assert!(!is_within_workspace_posix("/elsewhere/x.rs", "/repo", &extras));
    }

#[test]
    fn test_workspace_containment_refuses_escape_shapes() {
        assert!(!is_within_directory_posix("~/.ssh/config", "/"));
        assert!(!is_within_directory_posix("/repo/~/x", "/repo/real"));
        assert!(is_within_directory_posix("/repo/x.rs", "/repo/"));
        assert!(is_within_directory_posix("/repo/", "/repo"));
        assert!(!is_within_directory_posix("/repo-evil/x.rs", "/repo/"));
        assert!(is_within_directory_posix("/repo//src//x.rs", "/repo"));
        assert!(!is_within_directory_posix("/../etc/passwd", "/repo"));
        assert!(is_within_directory_posix("\\repo\\src\\x.rs", "/repo"));
        assert!(!is_within_directory_posix("\\repo2\\x.rs", "/repo"));
    }
}