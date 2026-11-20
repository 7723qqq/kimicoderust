use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::os::backends::remote::remote_file_system::RemoteFileSystem;
use crate::os::backends::remote::remote_process::RemoteProcessService;
use crate::os::interface::host_environment::{HostEnvironmentInfo, PathClass, ShellName};
use crate::wire::record::WireAsyncError;

use super::connection::{
    DEFAULT_INITIALIZE_TIMEOUT_MS, ConnectOptions, HandshakeError, HandshakeErrorDetails,
    HandshakeErrorKind, RemoteExecConnection,
};
use super::exec_bridge::{BytePipe, ExecBridge, ExecBridgeExit, ExecBridgeOptions};
use super::launchers::{
    LauncherSpec, is_bare_remote_bin, launcher_remote_bin, power_shell_command, resolve_launcher,
    resolve_ssh_command,
};
use super::ssh_askpass::{SshAuthOptions, create_ssh_askpass};

pub struct RemoteEnvironmentProbe {
    pub os_kind: String,
    pub os_arch: String,
    pub os_version: String,
    pub shell_name: ShellName,
    pub shell_path: String,
    pub path_class: PathClass,
    pub home_dir: String,
    pub cwd: String,
    pub temp_dir: String,
}

impl RemoteEnvironmentProbe {
    pub fn host_environment_info(&self) -> HostEnvironmentInfo {
        HostEnvironmentInfo {
            os_kind: self.os_kind.clone(),
            os_arch: self.os_arch.clone(),
            os_version: self.os_version.clone(),
            shell_name: self.shell_name.clone(),
            shell_path: self.shell_path.clone(),
            path_class: self.path_class.clone(),
            home_dir: self.home_dir.clone(),
            cwd: None,
            temp_dir: None,
            js_runtimes: None,
        }
    }
}

#[derive(Clone)]
pub struct RemoteSessionOptions {
    pub environment_id: String,
    pub launcher: LauncherSpec,
    pub client_version: Option<String>,
    pub min_executor_version: Option<String>,
    pub initialize_timeout_ms: Option<u64>,
    pub ssh_auth: Option<SshAuthOptions>,
}

pub struct RemoteSession {
    pub host: RemoteEnvironmentProbe,
    pub fs: Arc<RemoteFileSystem>,
    pub process: Arc<RemoteProcessService>,
    pub executor_version: String,
    pub connection: Arc<RemoteExecConnection>,
    bridge: Arc<ExecBridge>,
    disposed: std::sync::atomic::AtomicBool,
}

impl RemoteSession {
    pub async fn connect(options: RemoteSessionOptions) -> Result<Arc<Self>, WireAsyncError> {
        let askpass = match &options.ssh_auth {
            Some(auth) => Some(create_ssh_askpass(SshAuthOptions {
                password: auth.password.clone(),
                command: auth.command.clone(),
            }).await?),
            None => None,
        };
        let result = Self::connect_inner(&options, askpass.as_ref()).await;
        if let Some(askpass) = askpass {
            askpass.close().await;
        }
        result
    }

    async fn connect_inner(
        options: &RemoteSessionOptions,
        askpass: Option<&super::ssh_askpass::SshAskpass>,
    ) -> Result<Arc<Self>, WireAsyncError> {
        let ssh_env = askpass.map(|askpass| {
            let mut env = std::env::vars().collect::<HashMap<_, _>>();
            env.extend(askpass.env.clone());
            env
        });
        let remote_bin = match &options.launcher {
            LauncherSpec::Ssh { .. } => launcher_remote_bin(&options.launcher),
            _ => None,
        };
        let ssh_platform = if matches!(options.launcher, LauncherSpec::Ssh { .. })
            && remote_bin.as_deref().is_some_and(|remote_bin| !is_bare_remote_bin(remote_bin))
        {
            detect_ssh_platform(
                match &options.launcher {
                    LauncherSpec::Ssh { host, .. } => host,
                    _ => unreachable!(),
                },
                ssh_env.clone(),
                options.initialize_timeout_ms.unwrap_or(DEFAULT_INITIALIZE_TIMEOUT_MS),
            )
            .await?
        } else {
            "posix".to_string()
        };
        let resolved = resolve_launcher(
            &options.launcher,
            options.ssh_auth.is_some(),
            &ssh_platform,
        )?;
        let env = if let Some(askpass) = askpass {
            let mut env = std::env::vars().collect::<HashMap<_, _>>();
            if let Some(resolved) = &resolved.env {
                env.extend(resolved.clone());
            }
            env.extend(askpass.env.clone());
            Some(env)
        } else {
            resolved.env.clone()
        };
        let bridge = ExecBridge::spawn(ExecBridgeOptions {
            program: resolved.program,
            args: resolved.args,
            env,
        })?;
        let exit_watch = bridge.exited();
        let connection = RemoteExecConnection::connect(
            bridge.clone(),
            ConnectOptions {
                client_name: "kimi-code".to_string(),
                client_version: options.client_version.clone().unwrap_or_else(|| "0.0.0".to_string()),
                min_executor_version: options.min_executor_version.clone(),
                initialize_timeout_ms: options.initialize_timeout_ms,
                request_call_timeout_ms: None,
                liveness_interval_ms: None,
                liveness_timeout_ms: None,
                handshake_timers: None,
            },
        )
        .await;
        let connection = match connection {
            Ok(connection) => connection,
            Err(error) => {
                bridge.close();
                let exit = tokio::time::timeout(Duration::from_millis(100), exit_watch)
                    .await
                    .ok();
                if let Some(exit) = exit {
                    if error.downcast_ref::<HandshakeError>().is_none() {
                        let stderr = bridge.get_stderr_tail().trim().to_string();
                        let detail = if stderr.is_empty() {
                            exit.error.map(|error| error.to_string())
                        } else {
                            Some(stderr.clone())
                        };
                        let authentication_failed = matches!(options.launcher, LauncherSpec::Ssh { .. })
                            && exit.code == Some(255)
                            && Regex::new(r"(?i)Permission denied \([^)]*(?:password|keyboard-interactive)[^)]*\)")
                                .unwrap()
                                .is_match(&stderr);
                        return Err(Box::new(HandshakeError::new(
                            format!(
                                "executor process exited before the handshake completed (code {}, signal {}){}",
                                exit.code.map_or("null".to_string(), |code| code.to_string()),
                                exit.signal.clone().unwrap_or_else(|| "null".to_string()),
                                detail.filter(|detail| !detail.is_empty()).map(|detail| format!(": {detail}")).unwrap_or_default()
                            ),
                            HandshakeErrorDetails {
                                kind: Some(if authentication_failed {
                                    HandshakeErrorKind::Authentication
                                } else {
                                    HandshakeErrorKind::ExecutorExit
                                }),
                                exit_code: exit.code,
                                cause: Some(error),
                                ..Default::default()
                            },
                        )));
                    }
                }
                if error.downcast_ref::<HandshakeError>().is_some_and(|error| {
                    error.kind == Some(HandshakeErrorKind::Timeout)
                }) {
                    let stderr = bridge.get_stderr_tail().trim().to_string();
                    if !stderr.is_empty() {
                        return Err(Box::new(HandshakeError::new(
                            format!("{error}; executor stderr: {stderr}"),
                            HandshakeErrorDetails {
                                kind: Some(HandshakeErrorKind::Timeout),
                                cause: Some(error),
                                ..Default::default()
                            },
                        )));
                    }
                }
                return Err(error);
            }
        };
        let environment = connection.environment();
        let host = RemoteEnvironmentProbe {
            os_kind: environment.os_kind,
            os_arch: environment.os_arch,
            os_version: environment.os_version,
            shell_name: environment.shell_name,
            shell_path: environment.shell_path.clone(),
            path_class: environment.path_class,
            home_dir: environment.home_dir,
            cwd: environment.cwd.clone(),
            temp_dir: environment.temp_dir,
        };
        let executor_version = connection.executor_version();
        Ok(Arc::new(Self {
            fs: Arc::new(RemoteFileSystem::new(connection.clone())),
            process: RemoteProcessService::new(
                connection.clone(),
                environment.cwd,
                environment.shell_path,
            ),
            host,
            executor_version,
            connection,
            bridge,
            disposed: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    pub async fn dispose(&self) {
        if self.disposed.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.connection.close();
        self.bridge.close();
        let _ = self.bridge.exited().await;
    }
}

async fn detect_ssh_platform(
    host: &str,
    env: Option<HashMap<String, String>>,
    timeout_ms: u64,
) -> Result<String, WireAsyncError> {
    let probes = ["uname -s".to_string(), power_shell_command("[Console]::WriteLine('KIMI_WINDOWS')")];
    for (index, command) in probes.iter().enumerate() {
        let launcher = resolve_ssh_command(host, command, env.is_some())?;
        let result = run_ssh_probe(&launcher.program, &launcher.args, env.clone(), timeout_ms).await?;
        let output = result
            .stdout
            .trim()
            .lines()
            .next_back()
            .unwrap_or("")
            .to_string();
        if result.code == Some(255) {
            let authentication_failed = Regex::new(
                r"(?i)Permission denied \([^)]*(?:password|keyboard-interactive)[^)]*\)",
            )
            .unwrap()
            .is_match(&result.stderr);
            return Err(Box::new(HandshakeError::new(
                format!(
                    "SSH platform probe failed: {}",
                    if result.stderr.trim().is_empty() {
                        "ssh exited with code 255".to_string()
                    } else {
                        result.stderr.trim().to_string()
                    }
                ),
                HandshakeErrorDetails {
                    kind: Some(if authentication_failed {
                        HandshakeErrorKind::Authentication
                    } else {
                        HandshakeErrorKind::ExecutorExit
                    }),
                    exit_code: Some(255),
                    ..Default::default()
                },
            )));
        }
        if result.code == Some(0) && !output.is_empty() {
            if index == 0 {
                return Ok(if Regex::new(r"(?i)^(?:MINGW|MSYS|CYGWIN)").unwrap().is_match(&output) {
                    "windows".to_string()
                } else {
                    "posix".to_string()
                });
            }
            if output == "KIMI_WINDOWS" {
                return Ok("windows".to_string());
            }
        }
        if index == probes.len() - 1 {
            let detail = if !result.stderr.trim().is_empty() {
                result.stderr.trim().to_string()
            } else if !result.stdout.trim().is_empty() {
                result.stdout.trim().to_string()
            } else {
                format!("probe exited with code {}", result.code.map_or("null".to_string(), |code| code.to_string()))
            };
            return Err(Box::new(HandshakeError::new(
                format!("Could not detect the SSH target platform: {detail}"),
                HandshakeErrorDetails {
                    kind: Some(HandshakeErrorKind::ExecutorExit),
                    exit_code: result.code,
                    ..Default::default()
                },
            )));
        }
    }
    Err("SSH platform probes exhausted".into())
}

struct SshProbeResult {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

async fn run_ssh_probe(
    program: &str,
    args: &[String],
    env: Option<HashMap<String, String>>,
    timeout_ms: u64,
) -> Result<SshProbeResult, WireAsyncError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(env) = env {
        command.envs(env);
    }
    let mut child = command.spawn()?;
    child.stdin.take().unwrap().shutdown().await?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let read_out = async {
            let mut buffer = [0u8; 4096];
            loop {
                let read = stdout.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                out.extend_from_slice(&buffer[..read]);
                if out.len() > 64 * 1024 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "stdout maxBuffer length exceeded",
                    ));
                }
            }
            Ok::<_, std::io::Error>(())
        };
        let read_err = async {
            let mut buffer = [0u8; 4096];
            loop {
                let read = stderr.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                err.extend_from_slice(&buffer[..read]);
                if err.len() > 64 * 1024 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "stderr maxBuffer length exceeded",
                    ));
                }
            }
            Ok::<_, std::io::Error>(())
        };
        let output = async {
            tokio::try_join!(read_out, read_err)?;
            Ok::<(), std::io::Error>(())
        };
        let (_, status) = tokio::try_join!(output, child.wait())?;
        Ok::<_, std::io::Error>(status)
    })
    .await;
    match result {
        Ok(Ok(status)) => Ok(SshProbeResult {
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            code: status.code(),
        }),
        Ok(Err(error)) => Err(Box::new(HandshakeError::new(
            format!("SSH platform probe failed: {error}"),
            HandshakeErrorDetails {
                kind: Some(HandshakeErrorKind::ExecutorExit),
                cause: Some(error.into()),
                ..Default::default()
            },
        ))),
        Err(_) => {
            let stderr = String::from_utf8_lossy(&err);
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            };
            Err(Box::new(HandshakeError::new(
                format!("SSH platform probe timed out after {timeout_ms}ms{detail}"),
                HandshakeErrorDetails {
                    kind: Some(HandshakeErrorKind::Timeout),
                    ..Default::default()
                },
            )))
        }
    }
}
