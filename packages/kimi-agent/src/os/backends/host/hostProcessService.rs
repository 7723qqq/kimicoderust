use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use parking_lot::Mutex;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::os::backends::host::host_timers_service::HostTimersService;
use crate::os::interface::host_process::{
    HostOutputStream, HostProcessError, HostProcessOptions, HostProcessShell, IHostProcess,
    IHostProcessService, OS_PROCESS_KILL_FAILED, OS_PROCESS_SPAWN_FAILED,
};
use crate::os::interface::host_timers::IHostTimers;
use crate::wire::record::WireAsyncError;

const TASKKILL_TIMEOUT_MS: u64 = 5_000;

#[derive(Clone)]
pub struct HostProcessService {
    pub timers: Arc<dyn IHostTimers>,
}

impl HostProcessService {
    pub fn new(timers: Option<Arc<dyn IHostTimers>>) -> Self {
        Self {
            timers: timers.unwrap_or_else(|| Arc::new(HostTimersService::new())),
        }
    }

    pub async fn spawn_with_options(
        &self,
        command: &str,
        args: &[String],
        options: HostProcessOptions,
    ) -> Result<Arc<dyn IHostProcess>, WireAsyncError> {
        if let Some(cwd) = &options.cwd {
            if !std::fs::metadata(cwd).is_ok_and(|stat| stat.is_dir()) {
                return Err(Box::new(HostProcessError::new(
                    OS_PROCESS_SPAWN_FAILED,
                    format!(
                        "Failed to spawn \"{command}\": cwd {cwd} does not exist or is not a directory"
                    ),
                    None,
                    Some(serde_json::json!({ "command": command, "args": args, "cwd": cwd })),
                )));
            }
        }
        let resolved = resolve_windows_spawn(command, args, &options);
        let mut process = Command::new(&resolved.command);
        #[cfg(windows)]
        if resolved.windows_verbatim_arguments {
            use std::os::windows::process::CommandExt;
            for arg in &resolved.args {
                process.raw_arg(arg);
            }
        } else {
            process.args(&resolved.args);
        }
        #[cfg(not(windows))]
        process.args(&resolved.args);
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &options.cwd {
            process.current_dir(cwd);
        }
        if let Some(env) = build_env(options.env.as_ref()) {
            process.envs(env);
        }
        #[cfg(unix)]
        if options.detached.unwrap_or(true) {
            use std::os::unix::process::CommandExt;
            process.process_group(0);
        }
        #[cfg(windows)]
        if options.windows_hide.unwrap_or(true) {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            process.creation_flags(CREATE_NO_WINDOW);
        }
        let child = process.spawn().map_err(|error| {
            Box::new(HostProcessError::with_cause(
                OS_PROCESS_SPAWN_FAILED,
                format!("Failed to spawn \"{command}\": {error}"),
                Some(serde_json::json!({
                    "command": command,
                    "args": args,
                    "cwd": options.cwd,
                    "errno": error.raw_os_error(),
                })),
                error,
            )) as WireAsyncError
        })?;
        Ok(Arc::new(HostProcess::new(
            child,
            options.merge_stderr.unwrap_or(false),
            options.timeout,
            self.timers.clone(),
        )))
    }
}

impl Default for HostProcessService {
    fn default() -> Self {
        Self::new(None)
    }
}

#[async_trait]
impl IHostProcessService for HostProcessService {
    async fn spawn(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: Option<&HashMap<String, String>>,
    ) -> Result<Arc<dyn IHostProcess>, WireAsyncError> {
        self.spawn_with_options(
            command,
            args,
            HostProcessOptions {
                cwd: cwd.map(str::to_string),
                env: env.cloned(),
                ..Default::default()
            },
        )
        .await
    }

    fn timers(&self) -> Option<Arc<dyn IHostTimers>> {
        Some(self.timers.clone())
    }
}

struct ResolvedSpawn {
    command: String,
    args: Vec<String>,
    windows_verbatim_arguments: bool,
}

fn resolve_windows_spawn(
    command: &str,
    args: &[String],
    options: &HostProcessOptions,
) -> ResolvedSpawn {
    if options.shell.as_ref().is_some_and(|shell| {
        !matches!(shell, HostProcessShell::Bool(false))
    }) {
        let joined = std::iter::once(command)
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        if cfg!(windows) {
            let host = match &options.shell {
                Some(HostProcessShell::Path(path)) => path.clone(),
                _ => std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string()),
            };
            return ResolvedSpawn {
                command: host,
                args: vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    format!("\"{joined}\""),
                ],
                windows_verbatim_arguments: true,
            };
        }
        let host = match &options.shell {
            Some(HostProcessShell::Path(path)) => path.clone(),
            _ => "/bin/sh".to_string(),
        };
        return ResolvedSpawn {
            command: host,
            args: vec!["-c".to_string(), joined],
            windows_verbatim_arguments: false,
        };
    }
    if !cfg!(windows) {
        return ResolvedSpawn {
            command: command.to_string(),
            args: args.to_vec(),
            windows_verbatim_arguments: false,
        };
    }
    let Some(script) = find_windows_script(command, options.env.as_ref()) else {
        return ResolvedSpawn {
            command: command.to_string(),
            args: args.to_vec(),
            windows_verbatim_arguments: false,
        };
    };
    let command_line = std::iter::once(escape_cmd_arg(&script))
        .chain(args.iter().map(|arg| escape_cmd_arg(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    ResolvedSpawn {
        command: std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string()),
        args: vec![
            "/d".to_string(),
            "/s".to_string(),
            "/c".to_string(),
            format!("\"{command_line}\""),
        ],
        windows_verbatim_arguments: true,
    }
}

fn find_windows_script(
    command: &str,
    env: Option<&HashMap<String, String>>,
) -> Option<String> {
    if command.to_lowercase().ends_with(".cmd") || command.to_lowercase().ends_with(".bat") {
        return Some(command.to_string());
    }
    if command.to_lowercase().ends_with(".exe") || command.to_lowercase().ends_with(".com") {
        return None;
    }
    let extensions = env
        .and_then(|env| env.get("PATHEXT"))
        .cloned()
        .or_else(|| std::env::var("PATHEXT").ok())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
    let extensions = extensions
        .split(';')
        .map(str::trim)
        .filter(|extension| !extension.is_empty())
        .collect::<Vec<_>>();
    let mut bases = Vec::new();
    if command.contains('/') || command.contains('\\') {
        bases.push(PathBuf::from(command));
    } else {
        bases.push(std::env::current_dir().unwrap_or_default().join(command));
        let path = env
            .and_then(|env| env.get("PATH"))
            .cloned()
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        for directory in path.split(';') {
            if !directory.is_empty() {
                bases.push(Path::new(directory).join(command));
            }
        }
    }
    for base in bases {
        for extension in &extensions {
            let candidate = PathBuf::from(format!("{}{}", base.to_string_lossy(), extension));
            if !candidate.is_file() {
                continue;
            }
            let name = candidate.to_string_lossy().to_lowercase();
            return (name.ends_with(".cmd") || name.ends_with(".bat"))
                .then_some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn escape_cmd_arg(arg: &str) -> String {
    if !arg.contains([' ', '\t', '"', '&', '|', '<', '>', '^', '(', ')', '%', '!']) {
        return arg.to_string();
    }
    let escaped = arg.replace('"', "\"\"");
    let trailing = escaped.chars().rev().take_while(|character| *character == '\\').count();
    format!("\"{escaped}{}\"", "\\".repeat(trailing))
}

fn build_env(overrides: Option<&HashMap<String, String>>) -> Option<HashMap<String, String>> {
    overrides.map(|overrides| {
        let mut env = std::env::vars().collect::<HashMap<_, _>>();
        env.extend(overrides.clone());
        env
    })
}

pub struct HostProcess {
    child: tokio::sync::Mutex<Child>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    stdout: Mutex<Option<ChildStdout>>,
    stderr: Mutex<Option<ChildStderr>>,
    pid: u32,
    exit_code: Arc<Mutex<Option<i32>>>,
    disposed: std::sync::atomic::AtomicBool,
}

impl HostProcess {
    fn new(
        mut child: Child,
        merge_stderr: bool,
        timeout: Option<u64>,
        timers: Arc<dyn IHostTimers>,
    ) -> Self {
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = if merge_stderr { None } else { child.stderr.take() };
        if merge_stderr {
            let _ = child.stderr.take();
        }
        let pid = child.id().unwrap_or(0);
        let exit_code = Arc::new(Mutex::new(None));
        if let Some(timeout) = timeout {
            let code = exit_code.clone();
            let timer = timers.clone();
            tokio::spawn(async move {
                let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let handle = timer.set_timeout(
                    Arc::new(move || {
                        fired.store(true, std::sync::atomic::Ordering::Relaxed);
                    }),
                    timeout,
                );
                let _ = handle;
                tokio::time::sleep(std::time::Duration::from_millis(timeout)).await;
                if code.lock().is_none() {
                    let _ = kill_process(pid, "SIGTERM").await;
                }
            });
        }
        Self {
            child: tokio::sync::Mutex::new(child),
            stdin: tokio::sync::Mutex::new(stdin),
            stdout: Mutex::new(stdout),
            stderr: Mutex::new(stderr),
            pid,
            exit_code,
            disposed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl IHostProcess for HostProcess {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock()
    }

    fn stdout(&self) -> HostOutputStream {
        let stream = self.stdout.lock().take();
        output_stream(stream.map(Output::Stdout))
    }

    fn stderr(&self) -> HostOutputStream {
        let stream = self.stderr.lock().take();
        output_stream(stream.map(Output::Stderr))
    }

    async fn write_stdin(&self, data: &[u8]) -> Result<(), WireAsyncError> {
        use tokio::io::AsyncWriteExt;
        let mut stdin = self.stdin.lock().await;
        let Some(stdin) = stdin.as_mut() else {
            return Ok(());
        };
        stdin.write_all(data).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn close_stdin(&self) -> Result<(), WireAsyncError> {
        self.stdin.lock().await.take();
        Ok(())
    }

    async fn wait(&self) -> Result<i32, WireAsyncError> {
        if let Some(code) = *self.exit_code.lock() {
            return Ok(code);
        }
        let status = self.child.lock().await.wait().await?;
        let code = status.code().unwrap_or(-1);
        *self.exit_code.lock() = Some(code);
        Ok(code)
    }

    async fn kill(&self, signal: &str) -> Result<(), WireAsyncError> {
        kill_process(self.pid, signal).await
    }

    async fn dispose(&self) {
        if self.disposed.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.stdin.lock().await.take();
        self.stdout.lock().take();
        self.stderr.lock().take();
    }
}

enum Output {
    Stdout(ChildStdout),
    Stderr(ChildStderr),
}

fn output_stream(stream: Option<Output>) -> HostOutputStream {
    Box::pin(futures_util::stream::try_unfold(stream, |stream| async move {
        let Some(mut stream) = stream else {
            return Ok(None);
        };
        let mut buffer = vec![0; 64 * 1024];
        let read = match &mut stream {
            Output::Stdout(stream) => {
                use tokio::io::AsyncReadExt;
                stream.read(&mut buffer).await
            }
            Output::Stderr(stream) => {
                use tokio::io::AsyncReadExt;
                stream.read(&mut buffer).await
            }
        }?;
        if read == 0 {
            return Ok(None);
        }
        buffer.truncate(read);
        Ok(Some((String::from_utf8_lossy(&buffer).into_owned(), Some(stream))))
    }))
}

#[cfg(unix)]
fn errno_name(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(libc::EPERM) => "EPERM".to_string(),
        Some(libc::ESRCH) => "ESRCH".to_string(),
        Some(libc::EINVAL) => "EINVAL".to_string(),
        Some(other) => other.to_string(),
        None => "UNKNOWN".to_string(),
    }
}

async fn kill_process(pid: u32, signal: &str) -> Result<(), WireAsyncError> {
    if pid == 0 {
        return Ok(());
    }
    #[cfg(windows)]
    {
        let mut killer = Command::new("taskkill");
        killer
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let Ok(mut killer) = killer.spawn() else {
            return Ok(());
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(TASKKILL_TIMEOUT_MS),
            killer.wait(),
        )
        .await;
        if result.is_err() {
            let _ = killer.kill().await;
        }
        let _ = signal;
        return Ok(());
    }
    #[cfg(unix)]
    {
        let signal_number = match signal {
            "SIGKILL" => libc::SIGKILL,
            "SIGINT" => libc::SIGINT,
            "SIGHUP" => libc::SIGHUP,
            _ => libc::SIGTERM,
        };
        let result = unsafe { libc::kill(-(pid as i32), signal_number) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        if error.raw_os_error() == Some(libc::EPERM) {
            if unsafe { libc::kill(pid as i32, signal_number) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return Ok(());
            }
            return Ok(());
        }
        Err(Box::new(HostProcessError::with_cause(
            OS_PROCESS_KILL_FAILED,
            format!("Failed to kill process {pid}: {error}"),
            Some(serde_json::json!({
                "pid": pid,
                "signal": signal,
                "errno": errno_name(&error),
            })),
            error,
        )))
    }
}
