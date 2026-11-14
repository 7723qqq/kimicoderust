use std::fmt;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;

use super::shell_payload::{
    ShellPayloadKind, find_shell_payload_pin, resolve_shell_payload_home, shell_payload_exe_path,
};

pub const GIT_EXEC_PATH_TIMEOUT: Duration = Duration::from_secs(5);

pub const JS_RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

const MINGW_PREFIXES: [&str; 5] = ["mingw32", "mingw64", "ucrt64", "clang64", "clangarm64"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OsKind {
    MacOs,
    Linux,
    Windows,
    Other(String),
}

impl OsKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::MacOs => "macOS",
            Self::Linux => "Linux",
            Self::Windows => "Windows",
            Self::Other(value) => value,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellName {
    Bash,
    Sh,
}

impl ShellName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Sh => "sh",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    Posix,
    Win32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsRuntimeName {
    Bun,
    Node,
}

impl JsRuntimeName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Node => "node",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsRuntimeInfo {
    pub name: JsRuntimeName,
    pub version: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEnvironmentInfo {
    pub os_kind: OsKind,
    pub os_arch: String,
    pub os_version: String,
    pub shell_name: ShellName,
    pub shell_path: String,
    pub path_class: PathClass,
    pub home_dir: String,
    pub cwd: Option<String>,
    pub temp_dir: Option<String>,
    pub js_runtimes: Vec<JsRuntimeInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeShellNotFound {
    pub message: String,
    pub checked: Vec<String>,
}

impl ProbeShellNotFound {
    pub fn code(&self) -> &'static str {
        kimi_core::primitives::errors::domains::os_process::SHELL_GIT_BASH_NOT_FOUND
    }

    pub fn checked_paths(&self) -> &[String] {
        &self.checked
    }

    pub fn coded_message(&self) -> String {
        format!("[{}] {}", self.code(), self.message)
    }
}

impl fmt::Display for ProbeShellNotFound {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProbeShellNotFound {}

pub struct HostEnvironmentProbeDeps<'a> {
    pub platform: &'a str,
    pub arch: &'a str,
    pub release: &'a str,
    pub home_dir: &'a Path,
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub is_file: &'a dyn Fn(&Path) -> bool,
    pub exec_file_text: &'a dyn Fn(&str, &[&str], Duration) -> Option<String>,
}

pub fn probe_host_environment(
    deps: &HostEnvironmentProbeDeps<'_>,
) -> Result<HostEnvironmentInfo, ProbeShellNotFound> {
    let os_kind = resolve_os_kind(deps.platform);
    let mut info = probe_base_environment(deps, os_kind)?;
    let js_runtimes = probe_js_runtimes(deps);
    if !js_runtimes.is_empty() {
        info.js_runtimes = js_runtimes;
    }
    Ok(info)
}

pub fn probe_host_environment_from_process() -> Result<HostEnvironmentInfo, ProbeShellNotFound> {
    static CACHE: OnceLock<Result<HostEnvironmentInfo, ProbeShellNotFound>> = OnceLock::new();
    CACHE.get_or_init(probe_process_environment).clone()
}

fn probe_process_environment() -> Result<HostEnvironmentInfo, ProbeShellNotFound> {
    let home_dir = crate::default_workspace::user_home_dir().unwrap_or_default();
    let env = |name: &str| std::env::var(name).ok();
    let deps = HostEnvironmentProbeDeps {
        platform: node_platform(),
        arch: node_arch(),
        release: std::env::consts::OS,
        home_dir: &home_dir,
        env: &env,
        is_file: &|path| path.is_file(),
        exec_file_text: &exec_file_text,
    };
    probe_host_environment(&deps)
}

fn resolve_os_kind(platform: &str) -> OsKind {
    match platform {
        "darwin" => OsKind::MacOs,
        "linux" => OsKind::Linux,
        "win32" => OsKind::Windows,
        other => OsKind::Other(other.to_string()),
    }
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

fn probe_base_environment(
    deps: &HostEnvironmentProbeDeps<'_>,
    os_kind: OsKind,
) -> Result<HostEnvironmentInfo, ProbeShellNotFound> {
    let path_class = if deps.platform == "win32" {
        PathClass::Win32
    } else {
        PathClass::Posix
    };
    let home_dir = deps.home_dir.display().to_string();

    if deps.platform == "win32" {
        let shell_path = locate_windows_git_bash(deps)?;
        return Ok(HostEnvironmentInfo {
            os_kind,
            os_arch: deps.arch.to_string(),
            os_version: deps.release.to_string(),
            shell_name: ShellName::Bash,
            shell_path,
            path_class,
            home_dir,
            cwd: None,
            temp_dir: None,
            js_runtimes: Vec::new(),
        });
    }

    let candidates = ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"];
    for candidate in candidates {
        if (deps.is_file)(Path::new(candidate)) {
            return Ok(HostEnvironmentInfo {
                os_kind,
                os_arch: deps.arch.to_string(),
                os_version: deps.release.to_string(),
                shell_name: ShellName::Bash,
                shell_path: candidate.to_string(),
                path_class,
                home_dir,
                cwd: None,
                temp_dir: None,
                js_runtimes: Vec::new(),
            });
        }
    }
    Ok(HostEnvironmentInfo {
        os_kind,
        os_arch: deps.arch.to_string(),
        os_version: deps.release.to_string(),
        shell_name: ShellName::Sh,
        shell_path: "/bin/sh".to_string(),
        path_class,
        home_dir,
        cwd: None,
        temp_dir: None,
        js_runtimes: Vec::new(),
    })
}

pub fn locate_windows_git_bash(
    deps: &HostEnvironmentProbeDeps<'_>,
) -> Result<String, ProbeShellNotFound> {
    let mut checked: Vec<String> = Vec::new();

    let override_path = (deps.env)("KIMI_SHELL_PATH")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(override_path) = override_path {
        checked.push(override_path.clone());
        if (deps.is_file)(Path::new(&override_path)) {
            return Ok(override_path);
        }
    }

    for candidate in provisioned_bash_candidates(deps) {
        checked.push(candidate.clone());
        if (deps.is_file)(Path::new(&candidate)) {
            return Ok(candidate);
        }
    }

    let path_env = (deps.env)("PATH");
    let git_executables =
        find_executables_on_path("git.exe", path_env.as_deref(), deps.platform, deps.is_file);

    for git_exe in git_executables {
        if let Some(inferred) = git_bash_candidates_from_git_exe(&git_exe) {
            for candidate in inferred {
                checked.push(candidate.clone());
                if (deps.is_file)(Path::new(&candidate)) {
                    return Ok(candidate);
                }
            }
        }

        let Some(git_exec_path) = read_git_exec_path(deps, &git_exe) else {
            continue;
        };
        for candidate in git_bash_candidates_from_git_exec_path(&git_exec_path) {
            checked.push(candidate.clone());
            if (deps.is_file)(Path::new(&candidate)) {
                return Ok(candidate);
            }
        }
    }

    let mut candidates: Vec<String> = vec![
        "C:\\Program Files\\Git\\bin\\bash.exe".to_string(),
        "C:\\Program Files\\Git\\usr\\bin\\bash.exe".to_string(),
        "C:\\Program Files (x86)\\Git\\bin\\bash.exe".to_string(),
        "C:\\Program Files (x86)\\Git\\usr\\bin\\bash.exe".to_string(),
    ];
    let local_app_data = (deps.env)("LOCALAPPDATA")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(local_app_data) = local_app_data {
        candidates.push(win32_join(&[
            &local_app_data,
            "Programs",
            "Git",
            "bin",
            "bash.exe",
        ]));
        candidates.push(win32_join(&[
            &local_app_data,
            "Programs",
            "Git",
            "usr",
            "bin",
            "bash.exe",
        ]));
    }
    for candidate in candidates {
        checked.push(candidate.clone());
        if (deps.is_file)(Path::new(&candidate)) {
            return Ok(candidate);
        }
    }

    Err(ProbeShellNotFound {
        message: "Git Bash was not found on this Windows host. Run `kimi shells provision --bash` to install the bundled bash payload, install Git for Windows from https://gitforwindows.org/, or set KIMI_SHELL_PATH to a bash.exe.".to_string(),
        checked,
    })
}

fn provisioned_bash_candidates(deps: &HostEnvironmentProbeDeps<'_>) -> Vec<String> {
    let Some(pin) = find_shell_payload_pin(ShellPayloadKind::Bash) else {
        return Vec::new();
    };
    let kimi_code_home = (deps.env)("KIMI_CODE_HOME");
    let kimi_home = resolve_shell_payload_home(kimi_code_home.as_deref(), deps.home_dir);
    vec![
        shell_payload_exe_path(&kimi_home, pin)
            .display()
            .to_string(),
    ]
}

fn read_git_exec_path(deps: &HostEnvironmentProbeDeps<'_>, git_exe: &str) -> Option<String> {
    if deps.platform == "win32" && !is_absolute_windows_path(git_exe) {
        return None;
    }
    let stdout = (deps.exec_file_text)(git_exe, &["--exec-path"], GIT_EXEC_PATH_TIMEOUT)?;
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn git_bash_candidates_from_git_exe(git_exe: &str) -> Option<Vec<String>> {
    let normalized_git_exe = win32_normalize(git_exe);
    let git_dir = win32_dirname(&normalized_git_exe);
    let git_dir_name = win32_basename(&git_dir).to_ascii_lowercase();
    if git_dir_name != "cmd" && git_dir_name != "bin" {
        return None;
    }
    Some(git_bash_candidates_from_git_root(&win32_dirname(&git_dir)))
}

fn git_bash_candidates_from_git_exec_path(exec_path: &str) -> Vec<String> {
    let normalized = win32_normalize(exec_path);
    let parts: Vec<&str> = normalized.split('\\').collect();
    for index in (0..parts.len()).rev() {
        let segment = parts[index].to_ascii_lowercase();
        if MINGW_PREFIXES.contains(&segment.as_str()) {
            let root = parts[..index].join("\\");
            if !root.is_empty() {
                return git_bash_candidates_from_git_root(&root);
            }
        }
    }
    git_bash_candidates_from_git_root(&win32_join(&[&normalized, "..", ".."]))
}

fn git_bash_candidates_from_git_root(root: &str) -> Vec<String> {
    vec![
        win32_join(&[root, "bin", "bash.exe"]),
        win32_join(&[root, "usr", "bin", "bash.exe"]),
    ]
}

fn win32_join(parts: &[&str]) -> String {
    win32_normalize(&parts.join("\\"))
}

fn win32_normalize(path: &str) -> String {
    let path = path.replace('/', "\\");
    let (prefix, rest) = split_windows_root(&path);
    let mut segments: Vec<&str> = Vec::new();
    for segment in rest.split('\\') {
        match segment {
            "" | "." => {}
            ".." => {
                let can_pop = matches!(segments.last(), Some(last) if *last != "..");
                if can_pop {
                    segments.pop();
                } else if prefix.is_empty() {
                    segments.push("..");
                }
            }
            other => segments.push(other),
        }
    }
    let joined = segments.join("\\");
    if prefix.is_empty() {
        if joined.is_empty() {
            ".".to_string()
        } else {
            joined
        }
    } else if joined.is_empty() {
        prefix
    } else {
        format!("{prefix}{joined}")
    }
}

fn split_windows_root(path: &str) -> (String, &str) {
    let bytes = path.as_bytes();
    if path.starts_with("\\\\") {
        let mut separators = 0;
        for (index, byte) in path.char_indices() {
            if byte == '\\' {
                separators += 1;
                if separators == 4 {
                    return (path[..index + 1].to_string(), &path[index + 1..]);
                }
            }
        }
        return (path.to_string(), "");
    }
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
        return (path[..3].to_string(), &path[3..]);
    }
    if path.starts_with('\\') {
        return ("\\".to_string(), &path[1..]);
    }
    (String::new(), path)
}

fn win32_dirname(path: &str) -> String {
    let path = win32_normalize(path);
    let (prefix, rest) = split_windows_root(&path);
    let trimmed = rest.trim_end_matches('\\');
    match trimmed.rfind('\\') {
        Some(index) => {
            let head = &trimmed[..index];
            if head.is_empty() {
                prefix
            } else {
                format!("{prefix}{head}")
            }
        }
        None => {
            if prefix.is_empty() {
                ".".to_string()
            } else {
                prefix
            }
        }
    }
}

fn win32_basename(path: &str) -> String {
    let path = win32_normalize(path);
    let (_, rest) = split_windows_root(&path);
    let trimmed = rest.trim_end_matches('\\');
    match trimmed.rfind('\\') {
        Some(index) => trimmed[index + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

fn normalize_windows_path(path: &str) -> String {
    path.replace('/', "\\")
}

fn is_absolute_windows_path(path: &str) -> bool {
    let normalized = normalize_windows_path(path);
    let bytes = normalized.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
        return true;
    }
    normalized.starts_with('\\')
}

fn dedupe_windows_paths(paths: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    for path in paths {
        let key = normalize_windows_path(path).to_ascii_lowercase();
        if seen.insert(key) {
            deduped.push(path.clone());
        }
    }
    deduped
}

pub fn find_executables_on_path(
    name: &str,
    path_env: Option<&str>,
    platform: &str,
    is_file: &dyn Fn(&Path) -> bool,
) -> Vec<String> {
    let Some(path_env) = path_env else {
        return Vec::new();
    };
    if path_env.is_empty() {
        return Vec::new();
    }
    let list_separator = if platform == "win32" { ';' } else { ':' };
    let dir_separator = if platform == "win32" { '\\' } else { '/' };
    let mut paths = Vec::new();
    for raw_dir in path_env.split(list_separator) {
        let dir = raw_dir.trim();
        if dir.is_empty() {
            continue;
        }
        if platform == "win32" && !is_absolute_windows_path(dir) {
            continue;
        }
        let candidate = if dir.ends_with(dir_separator) {
            format!("{dir}{name}")
        } else {
            format!("{dir}{dir_separator}{name}")
        };
        if is_file(Path::new(&candidate)) {
            paths.push(candidate);
        }
    }
    if platform == "win32" {
        dedupe_windows_paths(&paths)
    } else {
        paths
    }
}

pub fn exec_file_text(file: &str, args: &[&str], timeout: Duration) -> Option<String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new(file)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let output = child.wait_with_output().ok()?;
                return Some(String::from_utf8_lossy(&output.stdout).to_string());
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

fn probe_js_runtimes(deps: &HostEnvironmentProbeDeps<'_>) -> Vec<JsRuntimeInfo> {
    let path_env = (deps.env)("PATH");
    let mut runtimes = Vec::new();
    for (name, binary) in [
        (
            JsRuntimeName::Bun,
            if deps.platform == "win32" {
                "bun.exe"
            } else {
                "bun"
            },
        ),
        (
            JsRuntimeName::Node,
            if deps.platform == "win32" {
                "node.exe"
            } else {
                "node"
            },
        ),
    ] {
        let Some(executable) =
            find_executables_on_path(binary, path_env.as_deref(), deps.platform, deps.is_file)
                .into_iter()
                .next()
        else {
            continue;
        };
        let stdout = (deps.exec_file_text)(&executable, &["--version"], JS_RUNTIME_PROBE_TIMEOUT);
        let Some(version) = stdout.and_then(|text| parse_runtime_version(&text)) else {
            continue;
        };
        runtimes.push(JsRuntimeInfo {
            name,
            version,
            path: executable,
        });
    }
    runtimes
}

static RUNTIME_VERSION_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^v?(\d+\.\d+\.\d+(?:[-+][^\s]+)?)").expect("valid runtime version pattern")
});

fn parse_runtime_version(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(found) = RUNTIME_VERSION_PATTERN.captures(trimmed) {
            return found.get(1).map(|capture| capture.as_str().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn deps<'a>(
        env: &'a dyn Fn(&str) -> Option<String>,
        is_file: &'a dyn Fn(&Path) -> bool,
        exec: &'a dyn Fn(&str, &[&str], Duration) -> Option<String>,
    ) -> HostEnvironmentProbeDeps<'a> {
        HostEnvironmentProbeDeps {
            platform: "win32",
            arch: "x64",
            release: "10.0.22631",
            home_dir: Path::new("C:\\Users\\tester"),
            env,
            is_file,
            exec_file_text: exec,
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn no_exec(_: &str, _: &[&str], _: Duration) -> Option<String> {
        None
    }

    #[test]
    fn non_windows_prefers_bin_bash_then_sh() {
        let is_file = |path: &Path| path == Path::new("/usr/bin/bash");
        let probe = |path: &Path| is_file(path);
        let result = probe_host_environment(&HostEnvironmentProbeDeps {
            platform: "linux",
            arch: "x64",
            release: "6.8.0",
            home_dir: Path::new("/home/tester"),
            env: &no_env,
            is_file: &probe,
            exec_file_text: &no_exec,
        })
        .unwrap();
        assert_eq!(result.shell_name, ShellName::Bash);
        assert_eq!(result.shell_path, "/usr/bin/bash");
        assert_eq!(result.path_class, PathClass::Posix);
        assert_eq!(result.os_kind, OsKind::Linux);

        let result = probe_host_environment(&HostEnvironmentProbeDeps {
            platform: "darwin",
            arch: "arm64",
            release: "24.0.0",
            home_dir: Path::new("/Users/tester"),
            env: &no_env,
            is_file: &|_| false,
            exec_file_text: &no_exec,
        })
        .unwrap();
        assert_eq!(result.os_kind, OsKind::MacOs);
        assert_eq!(result.shell_name, ShellName::Sh);
        assert_eq!(result.shell_path, "/bin/sh");
    }

    #[test]
    fn an_existing_override_wins() {
        let env =
            |name: &str| (name == "KIMI_SHELL_PATH").then(|| "C:\\custom\\bash.exe".to_string());
        let is_file = |path: &Path| path == Path::new("C:\\custom\\bash.exe");
        let found = locate_windows_git_bash(&deps(&env, &is_file, &no_exec)).unwrap();
        assert_eq!(found, "C:\\custom\\bash.exe");
    }

    #[test]
    fn a_missing_override_continues_to_the_fixed_candidates() {
        let env =
            |name: &str| (name == "KIMI_SHELL_PATH").then(|| "C:\\missing\\bash.exe".to_string());
        let is_file =
            |path: &Path| path.display().to_string() == "C:\\Program Files\\Git\\bin\\bash.exe";
        let found = locate_windows_git_bash(&deps(&env, &is_file, &no_exec)).unwrap();
        assert_eq!(found, "C:\\Program Files\\Git\\bin\\bash.exe");
    }

    #[test]
    fn a_missing_override_is_recorded_in_the_checked_list() {
        let env =
            |name: &str| (name == "KIMI_SHELL_PATH").then(|| "C:\\missing\\bash.exe".to_string());
        let error = locate_windows_git_bash(&deps(&env, &|_| false, &no_exec))
            .expect_err("nothing resolves");
        assert_eq!(
            error.checked.first().map(String::as_str),
            Some("C:\\missing\\bash.exe")
        );
        assert_eq!(error.code(), "shell.git_bash_not_found");
    }

    #[test]
    fn the_provisioned_payload_is_probed_after_the_override() {
        let env = |name: &str| match name {
            "KIMI_CODE_HOME" => Some("D:\\kimi-home".to_string()),
            _ => None,
        };
        let provisioned = shell_payload_exe_path(
            Path::new("D:\\kimi-home"),
            find_shell_payload_pin(ShellPayloadKind::Bash).unwrap(),
        )
        .display()
        .to_string();
        let is_file = |path: &Path| path.display().to_string() == provisioned;
        let found = locate_windows_git_bash(&deps(&env, &is_file, &no_exec)).unwrap();
        assert_eq!(found, provisioned);
    }

    #[test]
    fn a_git_exe_on_path_contributes_its_sibling_bash() {
        let env = |name: &str| match name {
            "PATH" => Some("C:\\Program Files\\Git\\cmd".to_string()),
            _ => None,
        };
        let is_file = |path: &Path| {
            let value = path.display().to_string();
            value == "C:\\Program Files\\Git\\cmd\\git.exe"
                || value == "C:\\Program Files\\Git\\bin\\bash.exe"
        };
        let found = locate_windows_git_bash(&deps(&env, &is_file, &no_exec)).unwrap();
        assert_eq!(found, "C:\\Program Files\\Git\\bin\\bash.exe");
    }

    #[test]
    fn git_exec_path_is_the_next_source() {
        let env = |name: &str| match name {
            "PATH" => Some("C:\\tools".to_string()),
            _ => None,
        };
        let is_file = |path: &Path| {
            let value = path.display().to_string();
            value == "C:\\tools\\git.exe" || value == "C:\\Program Files\\Git\\usr\\bin\\bash.exe"
        };
        let exec = |file: &str, args: &[&str], _: Duration| {
            (file == "C:\\tools\\git.exe" && args == ["--exec-path"])
                .then(|| "C:/Program Files/Git/mingw64/libexec/git-core\r\n".to_string())
        };
        let found = locate_windows_git_bash(&deps(&env, &is_file, &exec)).unwrap();
        assert_eq!(found, "C:\\Program Files\\Git\\usr\\bin\\bash.exe");
    }

    #[test]
    fn the_x86_install_and_local_app_data_are_the_last_resorts() {
        let env = |name: &str| match name {
            "LOCALAPPDATA" => Some("C:\\Users\\tester\\AppData\\Local".to_string()),
            _ => None,
        };
        let is_file = |path: &Path| {
            path.display().to_string()
                == "C:\\Users\\tester\\AppData\\Local\\Programs\\Git\\usr\\bin\\bash.exe"
        };
        let found = locate_windows_git_bash(&deps(&env, &is_file, &no_exec)).unwrap();
        assert_eq!(
            found,
            "C:\\Users\\tester\\AppData\\Local\\Programs\\Git\\usr\\bin\\bash.exe"
        );
    }

    #[test]
    fn a_missing_shell_reports_every_checked_path() {
        let error = locate_windows_git_bash(&deps(&no_env, &|_| false, &no_exec))
            .expect_err("nothing resolves");
        assert!(error.checked.len() >= 4);
        assert!(error.message.contains("kimi shells provision --bash"));
        assert!(
            error
                .coded_message()
                .starts_with("[shell.git_bash_not_found] ")
        );
    }

    #[test]
    fn a_relative_path_directory_is_skipped_on_windows() {
        let env = |name: &str| match name {
            "PATH" => Some("git\\cmd;C:\\tools".to_string()),
            _ => None,
        };
        let is_file = |path: &Path| path.display().to_string() == "git\\cmd\\git.exe";
        let found = find_executables_on_path("git.exe", env("PATH").as_deref(), "win32", &is_file);
        assert!(found.is_empty());
    }

    #[test]
    fn path_hits_are_deduped_case_insensitively() {
        let env = |name: &str| match name {
            "PATH" => Some("C:\\Tools;C:\\TOOLS".to_string()),
            _ => None,
        };
        let is_file = |_: &Path| true;
        let found = find_executables_on_path("git.exe", env("PATH").as_deref(), "win32", &is_file);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn the_version_parser_matches_the_node_regex() {
        assert_eq!(
            parse_runtime_version("v20.11.0\n"),
            Some("20.11.0".to_string())
        );
        assert_eq!(parse_runtime_version("1.4.4\n"), Some("1.4.4".to_string()));
        assert_eq!(
            parse_runtime_version("1.4.4-canary.1"),
            Some("1.4.4-canary.1".to_string())
        );
        assert_eq!(parse_runtime_version("not a version"), None);
        assert_eq!(parse_runtime_version(""), None);
    }

    #[test]
    fn win32_helpers_follow_node_semantics() {
        assert_eq!(
            win32_normalize("C:/Program Files/Git/mingw64/libexec/git-core/../../"),
            "C:\\Program Files\\Git\\mingw64"
        );
        assert_eq!(
            win32_dirname("C:\\Program Files\\Git\\cmd\\git.exe"),
            "C:\\Program Files\\Git\\cmd"
        );
        assert_eq!(win32_basename("C:\\Program Files\\Git\\cmd"), "cmd");
        assert_eq!(
            win32_join(&[
                "C:\\Program Files\\Git\\mingw64\\libexec\\git-core",
                "..",
                ".."
            ]),
            "C:\\Program Files\\Git\\mingw64"
        );
        assert!(is_absolute_windows_path("C:/Program Files/Git"));
        assert!(is_absolute_windows_path("\\\\server\\share"));
        assert!(!is_absolute_windows_path("git\\cmd"));
    }

    #[test]
    fn a_git_checkout_beside_bin_or_cmd_is_recognized() {
        let candidates =
            git_bash_candidates_from_git_exe("C:\\Program Files\\Git\\cmd\\git.exe").unwrap();
        assert_eq!(
            candidates,
            vec![
                "C:\\Program Files\\Git\\bin\\bash.exe".to_string(),
                "C:\\Program Files\\Git\\usr\\bin\\bash.exe".to_string(),
            ]
        );
        assert!(git_bash_candidates_from_git_exe("C:\\tools\\git.exe").is_none());
    }

    #[test]
    fn a_mingw_segment_anchors_the_exec_path_root() {
        let candidates =
            git_bash_candidates_from_git_exec_path("C:/Program Files/Git/mingw64/libexec/git-core");
        assert_eq!(
            candidates,
            vec![
                "C:\\Program Files\\Git\\bin\\bash.exe".to_string(),
                "C:\\Program Files\\Git\\usr\\bin\\bash.exe".to_string(),
            ]
        );
    }

    #[test]
    fn the_probe_reports_the_windows_shape() {
        let is_file =
            |path: &Path| path.display().to_string() == "C:\\Program Files\\Git\\bin\\bash.exe";
        let result = probe_host_environment(&deps(&no_env, &is_file, &no_exec)).unwrap();
        assert_eq!(result.os_kind, OsKind::Windows);
        assert_eq!(result.shell_name, ShellName::Bash);
        assert_eq!(result.shell_path, "C:\\Program Files\\Git\\bin\\bash.exe");
        assert_eq!(result.path_class, PathClass::Win32);
        assert_eq!(result.home_dir, "C:\\Users\\tester");
        assert!(result.js_runtimes.is_empty());
    }

    #[test]
    fn js_runtimes_are_probed_from_path_in_bun_then_node_order() {
        let env = |name: &str| match name {
            "PATH" => Some("C:\\tools".to_string()),
            _ => None,
        };
        let is_file = |path: &Path| {
            let value = path.display().to_string();
            value == "C:\\tools\\bun.exe"
                || value == "C:\\tools\\node.exe"
                || value == "C:\\Program Files\\Git\\bin\\bash.exe"
        };
        let exec = |file: &str, args: &[&str], _: Duration| {
            if args != ["--version"] {
                return None;
            }
            match file {
                "C:\\tools\\bun.exe" => Some("1.4.4\n".to_string()),
                "C:\\tools\\node.exe" => Some("v22.19.0\n".to_string()),
                _ => None,
            }
        };
        let result = probe_host_environment(&deps(&env, &is_file, &exec)).unwrap();
        assert_eq!(result.js_runtimes.len(), 2);
        assert_eq!(result.js_runtimes[0].name, JsRuntimeName::Bun);
        assert_eq!(result.js_runtimes[0].version, "1.4.4");
        assert_eq!(result.js_runtimes[1].name, JsRuntimeName::Node);
        assert_eq!(result.js_runtimes[1].version, "22.19.0");
    }

    #[test]
    fn a_failed_probe_keeps_the_checked_list_as_paths() {
        let error = probe_host_environment(&deps(&no_env, &|_| false, &no_exec))
            .expect_err("nothing resolves");
        assert!(error.checked_paths().iter().all(|path| !path.is_empty()));
        let _: PathBuf = PathBuf::from(&error.checked_paths()[0]);
    }
}
