use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;

use super::environment_probe::ShellName;

#[derive(Clone)]
pub struct ShellPathBridgeEnv {
    pub os_kind: String,
    pub shell_name: ShellName,
    pub shell_path: String,
}

pub type ExecFileSync = Arc<dyn Fn(String, Vec<String>) -> Result<String, String> + Send + Sync>;

#[derive(Clone)]
pub struct ShellPathBridgeDeps {
    pub exec_file_sync: ExecFileSync,
    pub is_file: Arc<dyn Fn(String) -> bool + Send + Sync>,
}

/// v2 `interface ShellPathBridge`: `toShellPath(nativePath) -> string` and
/// `fromShellPath(path) -> string`. Both the local cygpath bridge and the
/// remote MSYS bridge implement it; callers hold `Arc<dyn ShellPathBridge>`.
pub trait ShellPathBridge: Send + Sync {
    fn to_shell_path(&self, native_path: &str) -> String;
    fn from_shell_path(&self, path: &str) -> String;
}

#[derive(Clone)]
pub struct LocalShellPathBridge {
    env: ShellPathBridgeEnv,
    deps: ShellPathBridgeDeps,
    enabled: bool,
    cygpath_exe: Arc<Mutex<Option<Option<String>>>>,
    segment_cache: Arc<Mutex<HashMap<String, String>>>,
}

const CYGPATH_TIMEOUT_MS: u64 = 5_000;
const VIRTUAL_FS_PREFIXES: &[&str] = &["/dev/", "/proc/", "/sys/"];

static DRIVE_COLON_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^/([a-zA-Z]):(?:[\\/]|$)").unwrap());
static CYGDRIVE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^/cygdrive/([a-zA-Z])(?:/|$)").unwrap());
static DRIVE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^/([a-zA-Z])(?:/|$)").unwrap());
static WIN32_DRIVE_ABSOLUTE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z]:[\\/]").unwrap());

fn join_drive(letter: &str, rest: &str) -> String {
    let normalized_rest = rest.replace('\\', "/");
    if normalized_rest.is_empty() {
        format!("{}:/", letter.to_uppercase())
    } else {
        format!("{}:{normalized_rest}", letter.to_uppercase())
    }
}

pub fn translate_shell_drive_path(path: &str) -> String {
    if let Some(captures) = DRIVE_COLON_RE.captures(path) {
        return join_drive(captures.get(1).unwrap().as_str(), &path[3..]);
    }
    if let Some(captures) = CYGDRIVE_RE.captures(path) {
        let letter = captures.get(1).unwrap().as_str();
        return join_drive(letter, &path["/cygdrive/".len() + letter.len()..]);
    }
    if let Some(captures) = DRIVE_RE.captures(path) {
        return join_drive(captures.get(1).unwrap().as_str(), &path[2..]);
    }
    path.to_string()
}

impl LocalShellPathBridge {
    pub fn new(env: ShellPathBridgeEnv, deps: ShellPathBridgeDeps) -> Self {
        let enabled = env.os_kind == "Windows" && env.shell_name == ShellName::Bash;
        Self {
            env,
            deps,
            enabled,
            cygpath_exe: Arc::new(Mutex::new(None)),
            segment_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn locate_cygpath(&self) -> Option<String> {
        if let Some(cached) = self.cygpath_exe.lock().as_ref() {
            return cached.clone();
        }
        let shell_dir = win_dirname(&self.env.shell_path);
        let mut candidates = vec![win_join(&[&shell_dir, "cygpath.exe"])];
        if win_basename(&shell_dir).eq_ignore_ascii_case("bin") {
            candidates.push(win_join(&[&shell_dir, "..", "usr", "bin", "cygpath.exe"]));
        }
        let resolved = candidates
            .into_iter()
            .find(|candidate| (self.deps.is_file)(candidate.clone()));
        *self.cygpath_exe.lock() = Some(resolved.clone());
        resolved
    }

    fn resolve_root_segment(&self, first_segment: &str) -> Option<String> {
        if let Some(cached) = self.segment_cache.lock().get(first_segment) {
            return Some(cached.clone());
        }
        let exe = self.locate_cygpath()?;
        let output = (self.deps.exec_file_sync)(
            exe,
            vec![
                "-w".to_string(),
                "-C".to_string(),
                "UTF8".to_string(),
                "--".to_string(),
                format!("/{first_segment}"),
            ],
        )
        .ok()?;
        let trimmed = output
            .strip_suffix("\r\n")
            .or_else(|| output.strip_suffix('\n'))
            .unwrap_or(&output);
        if !WIN32_DRIVE_ABSOLUTE_RE.is_match(trimmed) && !trimmed.starts_with("\\\\") {
            return None;
        }
        let resolved = if trimmed.ends_with('/') || trimmed.ends_with('\\') {
            trimmed[..trimmed.len() - 1].to_string()
        } else {
            trimmed.to_string()
        };
        self.segment_cache
            .lock()
            .insert(first_segment.to_string(), resolved.clone());
        Some(resolved)
    }

    pub fn from_shell_path(&self, path: &str) -> String {
        if !self.enabled || path.starts_with("//") {
            return path.to_string();
        }
        if !path.starts_with('/') {
            return path.to_string();
        }
        let normalized = posix_normalize(path);
        let lexical = translate_shell_drive_path(&normalized);
        if lexical != normalized {
            return lexical;
        }
        if normalized == "/" || VIRTUAL_FS_PREFIXES.iter().any(|prefix| normalized.starts_with(prefix)) {
            return normalized;
        }
        let first_segment = normalized[1..].split('/').next().unwrap();
        let Some(prefix) = self.resolve_root_segment(first_segment) else {
            return normalized;
        };
        let remainder = &normalized[first_segment.len() + 1..];
        let joined = format!("{prefix}{remainder}").replace('\\', "/");
        if Regex::new(r"^[A-Za-z]:$").unwrap().is_match(&joined) {
            format!("{joined}/")
        } else {
            joined
        }
    }

    pub fn to_shell_path(&self, native_path: &str) -> String {
        if self.enabled {
            to_msys_path(native_path)
        } else {
            native_path.to_string()
        }
    }
}

impl ShellPathBridge for LocalShellPathBridge {
    fn to_shell_path(&self, native_path: &str) -> String {
        LocalShellPathBridge::to_shell_path(self, native_path)
    }

    fn from_shell_path(&self, path: &str) -> String {
        LocalShellPathBridge::from_shell_path(self, path)
    }
}

pub fn create_shell_path_bridge(
    env: ShellPathBridgeEnv,
    deps: ShellPathBridgeDeps,
) -> Arc<dyn ShellPathBridge> {
    Arc::new(LocalShellPathBridge::new(env, deps))
}

fn to_msys_path(native_path: &str) -> String {
    if native_path.starts_with("\\\\") {
        return native_path.replace('\\', "/");
    }
    let regex = Regex::new(r"^([A-Za-z]):(?:[\\/]|$)").unwrap();
    if let Some(captures) = regex.captures(native_path) {
        let drive = captures.get(1).unwrap().as_str().to_lowercase();
        let rest = native_path[2..].replace('\\', "/");
        return format!(
            "/{drive}{}",
            if rest.starts_with('/') {
                rest
            } else {
                format!("/{rest}")
            }
        );
    }
    native_path.replace('\\', "/")
}

#[derive(Clone)]
pub struct RemoteShellPathBridge;

impl ShellPathBridge for RemoteShellPathBridge {
    fn to_shell_path(&self, native_path: &str) -> String {
        to_msys_path(native_path)
    }

    /// v2 `REMOTE_SHELL_PATH_BRIDGE.fromShellPath` **throws** when the POSIX path
    /// cannot be resolved to a drive path on the target; the Rust equivalent is a
    /// panic carrying the same message.
    fn from_shell_path(&self, path: &str) -> String {
        if !path.starts_with('/') {
            return path.to_string();
        }
        let normalized = posix_normalize(path);
        let lexical = translate_shell_drive_path(&normalized);
        if lexical != normalized {
            return lexical;
        }
        panic!(
            "Windows remote path {normalized} cannot be resolved on the target; use an absolute drive path such as C:\\work instead"
        );
    }
}

pub static REMOTE_SHELL_PATH_BRIDGE: std::sync::LazyLock<Arc<dyn ShellPathBridge>> =
    std::sync::LazyLock::new(|| Arc::new(RemoteShellPathBridge));

fn posix_normalize(path: &str) -> String {
    let trailing_separator = path.len() > 1 && path.ends_with('/');
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    if segments.is_empty() {
        "/".to_string()
    } else if trailing_separator {
        format!("/{}/", segments.join("/"))
    } else {
        format!("/{}", segments.join("/"))
    }
}

fn win_dirname(path: &str) -> String {
    let path = path.replace('/', "\\");
    let trimmed = path.trim_end_matches('\\');
    trimmed
        .rfind('\\')
        .map(|index| trimmed[..index].to_string())
        .unwrap_or_else(|| trimmed.to_string())
}

fn win_basename(path: &str) -> String {
    path.trim_end_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or(path)
        .to_string()
}

fn win_join(segments: &[&str]) -> String {
    segments.join("\\").replace('/', "\\")
}

static BRIDGE_CACHE: Lazy<Mutex<HashMap<String, Arc<dyn ShellPathBridge>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn get_shell_path_bridge(env: ShellPathBridgeEnv) -> Arc<dyn ShellPathBridge> {
    let key = format!("{} {:?} {}", env.os_kind, env.shell_name, env.shell_path);
    if let Some(bridge) = BRIDGE_CACHE.lock().get(&key) {
        return bridge.clone();
    }
    let bridge = create_shell_path_bridge(
        env,
        ShellPathBridgeDeps {
            exec_file_sync: Arc::new(|file, args| exec_file_sync(&file, &args)),
            is_file: Arc::new(|path| std::path::Path::new(&path).exists()),
        },
    );
    BRIDGE_CACHE.lock().insert(key, bridge.clone());
    bridge
}

fn exec_file_sync(file: &str, args: &[String]) -> Result<String, String> {
    let mut command = std::process::Command::new(file);
    command
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let deadline = Instant::now() + Duration::from_millis(CYGPATH_TIMEOUT_MS);
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            let output = child.wait_with_output().map_err(|error| error.to_string())?;
            if !status.success() {
                return Err("spawn failed".to_string());
            }
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        if Instant::now() >= deadline {
            child.kill().map_err(|error| error.to_string())?;
            let _ = child.wait();
            return Err("spawn failed".to_string());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
