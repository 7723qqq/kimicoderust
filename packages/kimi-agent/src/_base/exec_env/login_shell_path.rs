use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt, Shared};
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use super::environment_probe::{ExecFileText, exec_file_text};

#[derive(Clone)]
pub struct LoginShellPathDeps {
    pub platform: String,
    pub env: HashMap<String, String>,
    pub user_shell: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    pub exec_file_text: ExecFileText,
}

const LOGIN_SHELL_ENV_TIMEOUT_MS: u64 = 5_000;

pub async fn probe_login_shell_path(deps: &LoginShellPathDeps) -> Option<String> {
    if deps.platform == "win32" {
        return None;
    }
    let env_shell = deps.env.get("SHELL").map(|shell| shell.trim());
    let shell = match env_shell {
        Some(shell) if !shell.is_empty() => Some(shell.to_string()),
        _ => (deps.user_shell)(),
    }?;
    if shell.is_empty() {
        return None;
    }
    let stdout = (deps.exec_file_text)(
        shell,
        vec![
            "-l".to_string(),
            "-c".to_string(),
            "/usr/bin/env".to_string(),
        ],
        LOGIN_SHELL_ENV_TIMEOUT_MS,
    )
    .await?;
    let mut path = None;
    for line in stdout.split('\n') {
        if let Some(value) = line.strip_prefix("PATH=") {
            path = Some(value.trim().to_string());
        }
    }
    path.filter(|path| !path.is_empty())
}

pub fn merge_login_shell_path(current_path: Option<&str>, login_shell_path: &str) -> String {
    let current = current_path.unwrap_or("");
    let mut seen = current
        .split(':')
        .filter(|entry| !entry.is_empty())
        .collect::<HashSet<_>>();
    let mut additions = Vec::new();
    for entry in login_shell_path.split(':') {
        if !entry.starts_with('/') || seen.contains(entry) {
            continue;
        }
        seen.insert(entry);
        additions.push(entry);
    }
    if additions.is_empty() {
        return current.to_string();
    }
    if current_path.is_none() {
        additions.join(":")
    } else {
        format!("{current}:{}", additions.join(":"))
    }
}

pub async fn apply_login_shell_path(deps: &mut LoginShellPathDeps) {
    let Some(login_shell_path) = probe_login_shell_path(deps).await else {
        return;
    };
    let current_path = deps.env.get("PATH").cloned();
    let merged = merge_login_shell_path(current_path.as_deref(), &login_shell_path);
    if merged == current_path.unwrap_or_default() {
        return;
    }
    deps.env.insert("PATH".to_string(), merged);
}

fn user_shell_from_node() -> Option<String> {
    if let Ok(shell) = std::env::var("SHELL")
        && !shell.is_empty()
    {
        return Some(shell);
    }
    let username = whoami::username();
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? == username {
            let shell = fields.nth(5)?;
            (!shell.is_empty()).then(|| shell.to_string())
        } else {
            None
        }
    })
}

static APPLIED_LOGIN_SHELL_PATH: Lazy<Mutex<Option<Shared<BoxFuture<'static, ()>>>>> =
    Lazy::new(|| Mutex::new(None));

pub fn apply_login_shell_path_from_node() -> Shared<BoxFuture<'static, ()>> {
    if let Some(applied) = APPLIED_LOGIN_SHELL_PATH.lock().as_ref() {
        return applied.clone();
    }
    let applied = async {
        let mut deps = LoginShellPathDeps {
            platform: match std::env::consts::OS {
                "windows" => "win32".to_string(),
                "macos" => "darwin".to_string(),
                platform => platform.to_string(),
            },
            env: std::env::vars().collect(),
            user_shell: Arc::new(user_shell_from_node),
            exec_file_text: Arc::new(|file, args, timeout_ms| {
                async move { exec_file_text(&file, &args, timeout_ms).await }.boxed()
            }),
        };
        apply_login_shell_path(&mut deps).await;
        if let Some(path) = deps.env.get("PATH") {
            unsafe {
                std::env::set_var("PATH", path);
            }
        }
    }
    .boxed()
    .shared();
    *APPLIED_LOGIN_SHELL_PATH.lock() = Some(applied.clone());
    applied
}
