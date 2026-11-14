use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt, Shared, join};
use once_cell::sync::Lazy;
use regex::Regex;

use super::shell_payload::{
    ShellPayloadKind, find_shell_payload_pin, resolve_shell_payload_home, shell_payload_exe_path,
};

pub type OsKind = String;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellName {
    Bash,
    Sh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathClass {
    Posix,
    Win32,
}

#[derive(Clone, Debug)]
pub struct ProbeShellNotFoundError {
    pub message: String,
    pub checked: Vec<String>,
}

impl fmt::Display for ProbeShellNotFoundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProbeShellNotFoundError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsRuntimeName {
    Bun,
    Node,
}

#[derive(Clone, Debug)]
pub struct JsRuntimeInfo {
    pub name: JsRuntimeName,
    pub version: String,
    pub path: String,
}

#[derive(Clone, Debug)]
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
    pub js_runtimes: Option<Vec<JsRuntimeInfo>>,
}

pub type IsFile = Arc<dyn Fn(String) -> BoxFuture<'static, bool> + Send + Sync>;
pub type ExecFileText = Arc<
    dyn Fn(String, Vec<String>, u64) -> BoxFuture<'static, Option<String>> + Send + Sync,
>;

#[derive(Clone)]
pub struct HostEnvironmentProbeDeps {
    pub platform: String,
    pub arch: String,
    pub release: String,
    pub home_dir: String,
    pub env: HashMap<String, String>,
    pub is_file: IsFile,
    pub exec_file_text: ExecFileText,
}

const GIT_EXEC_PATH_TIMEOUT_MS: u64 = 5_000;
const JS_RUNTIME_PROBE_TIMEOUT_MS: u64 = 3_000;

fn resolve_os_kind(platform: &str) -> OsKind {
    match platform {
        "darwin" => "macOS".to_string(),
        "linux" => "Linux".to_string(),
        "win32" => "Windows".to_string(),
        _ => platform.to_string(),
    }
}

pub async fn probe_host_environment(
    deps: &HostEnvironmentProbeDeps,
) -> Result<HostEnvironmentInfo, ProbeShellNotFoundError> {
    let os_kind = resolve_os_kind(&deps.platform);
    let (base, js_runtimes) = join(probe_base_environment(deps, os_kind), probe_js_runtimes(deps)).await;
    let mut base = base?;
    if !js_runtimes.is_empty() {
        base.js_runtimes = Some(js_runtimes);
    }
    Ok(base)
}

async fn probe_base_environment(
    deps: &HostEnvironmentProbeDeps,
    os_kind: OsKind,
) -> Result<HostEnvironmentInfo, ProbeShellNotFoundError> {
    let path_class = if deps.platform == "win32" {
        PathClass::Win32
    } else {
        PathClass::Posix
    };
    let base = |shell_name, shell_path| HostEnvironmentInfo {
        os_kind: os_kind.clone(),
        os_arch: deps.arch.clone(),
        os_version: deps.release.clone(),
        shell_name,
        shell_path,
        path_class,
        home_dir: deps.home_dir.clone(),
        cwd: None,
        temp_dir: None,
        js_runtimes: None,
    };
    if deps.platform == "win32" {
        let shell_path = locate_windows_git_bash(deps).await?;
        return Ok(base(ShellName::Bash, shell_path));
    }
    for candidate in ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"] {
        if (deps.is_file)(candidate.to_string()).await {
            return Ok(base(ShellName::Bash, candidate.to_string()));
        }
    }
    Ok(base(ShellName::Sh, "/bin/sh".to_string()))
}

async fn locate_windows_git_bash(
    deps: &HostEnvironmentProbeDeps,
) -> Result<String, ProbeShellNotFoundError> {
    let mut checked = Vec::new();
    if let Some(override_path) = deps.env.get("KIMI_SHELL_PATH")
        && !override_path.trim().is_empty()
    {
        let override_path = override_path.trim().to_string();
        checked.push(override_path.clone());
        if (deps.is_file)(override_path.clone()).await {
            return Ok(override_path);
        }
    }
    if let Some(pin) = find_shell_payload_pin(ShellPayloadKind::Bash) {
        let kimi_home = resolve_shell_payload_home(&deps.env, &deps.home_dir);
        let candidate = shell_payload_exe_path(&kimi_home, pin);
        checked.push(candidate.clone());
        if (deps.is_file)(candidate.clone()).await {
            return Ok(candidate);
        }
    }
    let git_executables = find_executables_on_path(
        "git.exe",
        deps.env.get("PATH").map(String::as_str),
        &deps.platform,
        &deps.is_file,
    )
    .await;
    for git_exe in git_executables {
        if let Some(inferred) = git_bash_candidates_from_git_exe(&git_exe) {
            for candidate in inferred {
                checked.push(candidate.clone());
                if (deps.is_file)(candidate.clone()).await {
                    return Ok(candidate);
                }
            }
        }
        let Some(git_exec_path) = read_git_exec_path(deps, &git_exe).await else {
            continue;
        };
        for candidate in git_bash_candidates_from_git_exec_path(&git_exec_path) {
            checked.push(candidate.clone());
            if (deps.is_file)(candidate.clone()).await {
                return Ok(candidate);
            }
        }
    }
    let mut candidates = vec![
        "C:\\Program Files\\Git\\bin\\bash.exe".to_string(),
        "C:\\Program Files\\Git\\usr\\bin\\bash.exe".to_string(),
        "C:\\Program Files (x86)\\Git\\bin\\bash.exe".to_string(),
        "C:\\Program Files (x86)\\Git\\usr\\bin\\bash.exe".to_string(),
    ];
    if let Some(local_app_data) = deps.env.get("LOCALAPPDATA")
        && !local_app_data.trim().is_empty()
    {
        let local_app_data = local_app_data.trim();
        candidates.push(format!("{local_app_data}\\Programs\\Git\\bin\\bash.exe"));
        candidates.push(format!("{local_app_data}\\Programs\\Git\\usr\\bin\\bash.exe"));
    }
    for candidate in candidates {
        checked.push(candidate.clone());
        if (deps.is_file)(candidate.clone()).await {
            return Ok(candidate);
        }
    }
    Err(ProbeShellNotFoundError {
        message: "Git Bash was not found on this Windows host. Run `kimi shells provision --bash` to install the bundled bash payload, install Git for Windows from https://gitforwindows.org/, or set KIMI_SHELL_PATH to a bash.exe.".to_string(),
        checked,
    })
}

async fn read_git_exec_path(deps: &HostEnvironmentProbeDeps, git_exe: &str) -> Option<String> {
    if deps.platform == "win32" && !is_absolute_windows_path(git_exe) {
        return None;
    }
    let stdout = (deps.exec_file_text)(
        git_exe.to_string(),
        vec!["--exec-path".to_string()],
        GIT_EXEC_PATH_TIMEOUT_MS,
    )
    .await?;
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn git_bash_candidates_from_git_exe(git_exe: &str) -> Option<Vec<String>> {
    let normalized = win_normalize(&normalize_windows_path(git_exe));
    let git_dir = win_dirname(&normalized);
    let git_dir_name = win_basename(&git_dir).to_lowercase();
    if git_dir_name != "cmd" && git_dir_name != "bin" {
        return None;
    }
    Some(git_bash_candidates_from_git_root(&win_dirname(&git_dir)))
}

fn git_bash_candidates_from_git_exec_path(exec_path: &str) -> Vec<String> {
    let normalized = win_normalize(&normalize_windows_path(exec_path));
    let parts = normalized.split('\\').collect::<Vec<_>>();
    for index in (0..parts.len()).rev() {
        if matches!(
            parts[index].to_lowercase().as_str(),
            "mingw32" | "mingw64" | "ucrt64" | "clang64" | "clangarm64"
        ) {
            let root = parts[..index].join("\\");
            if !root.is_empty() {
                return git_bash_candidates_from_git_root(&root);
            }
        }
    }
    git_bash_candidates_from_git_root(&win_normalize(&format!("{normalized}\\..\\..")))
}

fn git_bash_candidates_from_git_root(root: &str) -> Vec<String> {
    vec![
        win_normalize(&format!("{root}\\bin\\bash.exe")),
        win_normalize(&format!("{root}\\usr\\bin\\bash.exe")),
    ]
}

fn normalize_windows_path(path: &str) -> String {
    path.replace('/', "\\")
}

static WINDOWS_ABSOLUTE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?:[A-Za-z]:[\\/]|[\\/]{2})").unwrap());

fn is_absolute_windows_path(path: &str) -> bool {
    WINDOWS_ABSOLUTE.is_match(path)
}

fn win_normalize(path: &str) -> String {
    let path = normalize_windows_path(path);
    let prefix = if path.starts_with("\\\\") {
        "\\\\".to_string()
    } else if path.len() >= 2 && path.as_bytes()[1] == b':' {
        path[..2].to_string()
    } else {
        String::new()
    };
    let rest = if prefix == "\\\\" {
        path[2..].to_string()
    } else if prefix.is_empty() {
        path.clone()
    } else {
        path[2..].to_string()
    };
    let mut parts = Vec::new();
    for part in rest.split('\\') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    if prefix == "\\\\" {
        format!("\\\\{}", parts.join("\\"))
    } else if prefix.is_empty() {
        parts.join("\\")
    } else {
        format!("{}\\{}", prefix, parts.join("\\"))
    }
}

fn win_dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches('\\');
    trimmed
        .rfind('\\')
        .map(|index| {
            if index == 0 {
                "\\".to_string()
            } else {
                trimmed[..index].to_string()
            }
        })
        .unwrap_or_else(|| trimmed.to_string())
}

fn win_basename(path: &str) -> String {
    path.trim_end_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or(path)
        .to_string()
}

fn dedupe_windows_paths(paths: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for path in paths {
        if seen.insert(normalize_windows_path(&path).to_lowercase()) {
            deduped.push(path);
        }
    }
    deduped
}

async fn find_executables_on_path(
    name: &str,
    path_env: Option<&str>,
    platform: &str,
    is_file: &IsFile,
) -> Vec<String> {
    let Some(path_env) = path_env.filter(|path| !path.is_empty()) else {
        return Vec::new();
    };
    let list_separator = if platform == "win32" { ';' } else { ':' };
    let directory_separator = if platform == "win32" { "\\" } else { "/" };
    let mut paths = Vec::new();
    for raw_dir in path_env.split(list_separator) {
        let dir = raw_dir.trim();
        if dir.is_empty() || (platform == "win32" && !is_absolute_windows_path(dir)) {
            continue;
        }
        let candidate = if dir.ends_with(directory_separator) {
            format!("{dir}{name}")
        } else {
            format!("{dir}{directory_separator}{name}")
        };
        if is_file(candidate.clone()).await {
            paths.push(candidate);
        }
    }
    if platform == "win32" {
        dedupe_windows_paths(paths)
    } else {
        paths
    }
}

async fn probe_js_runtimes(deps: &HostEnvironmentProbeDeps) -> Vec<JsRuntimeInfo> {
    let specifications = [
        (JsRuntimeName::Bun, if deps.platform == "win32" { "bun.exe" } else { "bun" }),
        (JsRuntimeName::Node, if deps.platform == "win32" { "node.exe" } else { "node" }),
    ];
    let (bun, node) = join(
        probe_js_runtime(deps, specifications[0].0, specifications[0].1),
        probe_js_runtime(deps, specifications[1].0, specifications[1].1),
    )
    .await;
    [bun, node].into_iter().flatten().collect()
}

async fn probe_js_runtime(
    deps: &HostEnvironmentProbeDeps,
    name: JsRuntimeName,
    binary: &str,
) -> Option<JsRuntimeInfo> {
    let executable = find_executables_on_path(
        binary,
        deps.env.get("PATH").map(String::as_str),
        &deps.platform,
        &deps.is_file,
    )
    .await
    .into_iter()
    .next()?;
    let version = parse_runtime_version(
        &(deps.exec_file_text)(
            executable.clone(),
            vec!["--version".to_string()],
            JS_RUNTIME_PROBE_TIMEOUT_MS,
        )
        .await,
    )?;
    Some(JsRuntimeInfo {
        name,
        version,
        path: executable,
    })
}

fn parse_runtime_version(stdout: &Option<String>) -> Option<String> {
    let stdout = stdout.as_ref()?;
    let regex = Regex::new(r"^v?(\d+\.\d+\.\d+(?:[-+][^\s]+)?)").unwrap();
    stdout
        .lines()
        .filter_map(|line| regex.captures(line.trim()))
        .find_map(|captures| captures.get(1).map(|version| version.as_str().to_string()))
}

type ProbeFuture = Shared<BoxFuture<'static, Result<HostEnvironmentInfo, ProbeShellNotFoundError>>>;
static CACHED_PROBE: Lazy<std::sync::Mutex<Option<ProbeFuture>>> =
    Lazy::new(|| std::sync::Mutex::new(None));

pub fn probe_host_environment_from_node() -> ProbeFuture {
    if let Some(probe) = CACHED_PROBE.lock().unwrap().as_ref() {
        return probe.clone();
    }
    let platform = match std::env::consts::OS {
        "windows" => "win32".to_string(),
        "macos" => "darwin".to_string(),
        platform => platform.to_string(),
    };
    let env = std::env::vars().collect::<HashMap<_, _>>();
    let deps = HostEnvironmentProbeDeps {
        platform: platform.clone(),
        arch: node_arch(),
        release: os_release(),
        home_dir: home_dir(),
        env,
        is_file: Arc::new(|path| {
            async move { tokio::fs::metadata(path).await.is_ok() }.boxed()
        }),
        exec_file_text: Arc::new(|file, args, timeout_ms| {
            async move { exec_file_text(&file, &args, timeout_ms).await }.boxed()
        }),
    };
    let probe = async move { probe_host_environment(&deps).await }
        .boxed()
        .shared();
    *CACHED_PROBE.lock().unwrap() = Some(probe.clone());
    probe
}

fn node_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x64".to_string(),
        "aarch64" => "arm64".to_string(),
        "x86" => "ia32".to_string(),
        arch => arch.to_string(),
    }
}

fn home_dir() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default()
}

fn os_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|release| release.trim().to_string())
        .unwrap_or_else(|_| std::env::consts::OS.to_string())
}

pub async fn exec_file_text(file: &str, args: &[String], timeout_ms: u64) -> Option<String> {
    let mut command = tokio::process::Command::new(file);
    command
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let child = command.spawn().ok()?;
    let output = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        child.wait_with_output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}
