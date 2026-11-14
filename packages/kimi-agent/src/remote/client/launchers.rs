use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use base64::Engine;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::environment::program_path::resolve_program_path;
use crate::remote::declaration::remote_environment_declaration::RemoteEnvironmentEntry;

pub const DEFAULT_REMOTE_BIN: &str = "~/.kimi-code/bin/kimi";
pub const EXEC_SERVER_SUBCOMMAND: &str = "exec-server";
pub const EXEC_SERVER_ARGV: &[&str] = &[EXEC_SERVER_SUBCOMMAND, "--listen", "stdio"];
pub const SSH_CONFIG_OPTIONS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    "StrictHostKeyChecking=accept-new",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LauncherSpec {
    Ssh {
        host: String,
        #[serde(rename = "remoteBin", default, skip_serializing_if = "Option::is_none")]
        remote_bin: Option<String>,
    },
    Docker {
        container: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(rename = "remoteBin", default, skip_serializing_if = "Option::is_none")]
        remote_bin: Option<String>,
    },
    Command {
        program: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<HashMap<String, String>>,
        #[serde(rename = "remoteBin", default, skip_serializing_if = "Option::is_none")]
        remote_bin: Option<String>,
    },
}

impl LauncherSpec {
    pub fn remote_bin(&self) -> Option<&str> {
        match self {
            Self::Ssh { remote_bin, .. }
            | Self::Docker { remote_bin, .. }
            | Self::Command { remote_bin, .. } => remote_bin.as_deref(),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Ssh { .. } => "ssh",
            Self::Docker { .. } => "docker",
            Self::Command { .. } => "command",
        }
    }
}

pub fn docker_base_args(context: Option<&str>) -> Vec<String> {
    context
        .map(|context| vec!["--context".to_string(), context.to_string()])
        .unwrap_or_default()
}

pub fn to_launcher_spec(entry: &RemoteEnvironmentEntry) -> LauncherSpec {
    match entry {
        RemoteEnvironmentEntry::Command(entry) => LauncherSpec::Command {
            program: entry.command.clone(),
            args: entry.args.clone(),
            env: entry.env.clone(),
            remote_bin: entry.remote_bin.clone(),
        },
        RemoteEnvironmentEntry::Ssh(entry) => LauncherSpec::Ssh {
            host: entry.host.clone(),
            remote_bin: entry.remote_bin.clone(),
        },
        RemoteEnvironmentEntry::Docker(entry) => LauncherSpec::Docker {
            container: entry.container.clone(),
            context: entry.context.clone(),
            remote_bin: entry.remote_bin.clone(),
        },
    }
}

pub fn declaration_fingerprint(entry: &RemoteEnvironmentEntry) -> String {
    let mut launcher = to_launcher_spec(entry);
    match &mut launcher {
        LauncherSpec::Ssh { remote_bin, .. }
        | LauncherSpec::Docker { remote_bin, .. }
        | LauncherSpec::Command { remote_bin, .. } => {
            *remote_bin = Some(
                remote_bin
                    .clone()
                    .unwrap_or_else(|| DEFAULT_REMOTE_BIN.to_string()),
            );
        }
    }
    if let LauncherSpec::Command {
        program,
        args,
        env,
        remote_bin,
    } = &launcher
    {
        let sorted = env
            .as_ref()
            .map(|env| env.iter().map(|(key, value)| (key.clone(), value.clone())).collect::<BTreeMap<_, _>>())
            .unwrap_or_default();
        let sorted = sorted.into_iter().collect::<indexmap::IndexMap<_, _>>();
        return format!(
            "{{\"type\":\"command\",\"program\":{},\"args\":{},\"env\":{},\"remoteBin\":{}}}",
            serde_json::to_string(program).unwrap(),
            serde_json::to_string(args.as_ref().unwrap_or(&Vec::new())).unwrap(),
            serde_json::to_string(&sorted).unwrap(),
            serde_json::to_string(remote_bin).unwrap(),
        );
    }
    serde_json::to_string(&launcher).unwrap()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLauncher {
    pub program: String,
    pub args: Vec<String>,
    pub env: Option<HashMap<String, String>>,
}

pub fn assert_launcher_operand(kind: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{kind} must not be empty"));
    }
    if value.starts_with('-') {
        return Err(format!("{kind} must not start with '-': {value}"));
    }
    Ok(())
}

pub fn launcher_remote_bin(spec: &LauncherSpec) -> Option<String> {
    static EXEC_SERVER_WORD: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(^|[\s=])exec-server(\s|$)").unwrap());
    if let LauncherSpec::Command { args, .. } = spec {
        if args
            .as_ref()
            .is_some_and(|args| args.iter().any(|arg| EXEC_SERVER_WORD.is_match(arg)))
        {
            return None;
        }
    }
    Some(
        spec.remote_bin()
            .unwrap_or(DEFAULT_REMOTE_BIN)
            .to_string(),
    )
}

pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

static TILDE_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^~[a-zA-Z0-9._-]*").unwrap());
static BARE_REMOTE_BIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+$").unwrap());
static SHELL_RESERVED_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "case", "coproc", "do", "done", "elif", "else", "esac", "fi", "for", "function",
        "if", "in", "select", "then", "time", "until", "while",
    ])
});

pub fn is_bare_remote_bin(remote_bin: &str) -> bool {
    BARE_REMOTE_BIN.is_match(remote_bin) && !SHELL_RESERVED_WORDS.contains(remote_bin)
}

fn exec_server_shell_command(remote_bin: &str) -> String {
    if is_bare_remote_bin(remote_bin) {
        return std::iter::once(remote_bin.to_string())
            .chain(EXEC_SERVER_ARGV.iter().map(|arg| arg.to_string()))
            .collect::<Vec<_>>()
            .join(" ");
    }
    let tilde = TILDE_PREFIX.find(remote_bin).map(|match_| match_.as_str());
    let Some(prefix) = tilde else {
        return std::iter::once(sh_quote(remote_bin))
            .chain(EXEC_SERVER_ARGV.iter().map(|arg| sh_quote(arg)))
            .collect::<Vec<_>>()
            .join(" ");
    };
    let rest = &remote_bin[prefix.len()..];
    let bin = if prefix == "~" {
        if rest.is_empty() {
            "\"$HOME\"".to_string()
        } else {
            format!("\"$HOME\"{}", sh_quote(rest))
        }
    } else if rest.is_empty() {
        prefix.to_string()
    } else if rest == "/" {
        format!("{prefix}/")
    } else if rest.starts_with('/') {
        format!("{prefix}/{}", sh_quote(&rest[1..]))
    } else {
        sh_quote(remote_bin)
    };
    std::iter::once(bin)
        .chain(EXEC_SERVER_ARGV.iter().map(|arg| sh_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

static COMMAND_LAUNCHER_ENV_KEYS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "PATH", "HOME", "LANG", "TZ", "TMPDIR", "TEMP", "TMP", "SHELL", "USER", "LOGNAME",
        "SystemRoot", "SystemDrive", "windir", "ComSpec", "PATHEXT",
    ])
});

pub fn command_launcher_env(
    spec_env: Option<&HashMap<String, String>>,
    base_env: &HashMap<String, Option<String>>,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    for (key, value) in base_env {
        let Some(value) = value else {
            continue;
        };
        if COMMAND_LAUNCHER_ENV_KEYS.contains(key.as_str()) || key.starts_with("LC_") {
            env.insert(key.clone(), value.clone());
        }
    }
    if let Some(spec_env) = spec_env {
        env.extend(spec_env.clone());
    }
    env
}

pub fn power_shell_command(script: &str) -> String {
    let utf16 = script.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
    format!(
        "powershell -NoLogo -NoProfile -NonInteractive -EncodedCommand {}",
        base64::engine::general_purpose::STANDARD.encode(utf16)
    )
}

fn exec_server_power_shell_command(remote_bin: &str) -> String {
    let path = if remote_bin == DEFAULT_REMOTE_BIN {
        "~/.kimi-code/bin/kimi.exe"
    } else {
        remote_bin
    };
    let literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let executable = if path.starts_with("~/") || path.starts_with("~\\") {
        format!("($env:USERPROFILE + {})", literal(&path[1..]))
    } else {
        literal(path)
    };
    power_shell_command(&format!(
        "$ErrorActionPreference = 'Stop'; & {executable} {}; exit $LASTEXITCODE",
        EXEC_SERVER_ARGV.join(" ")
    ))
}

pub fn resolve_ssh_command(host: &str, command: &str, interactive_ssh_auth: bool) -> Result<ResolvedLauncher, String> {
    assert_launcher_operand("ssh host", host)?;
    let auth_options: &[&str] = if interactive_ssh_auth {
        &["-o", "BatchMode=no", "-o", "NumberOfPasswordPrompts=1"]
    } else {
        &["-o", "BatchMode=yes"]
    };
    Ok(ResolvedLauncher {
        program: "ssh".to_string(),
        args: std::iter::once("-T".to_string())
            .chain(auth_options.iter().map(|arg| arg.to_string()))
            .chain(SSH_CONFIG_OPTIONS[2..].iter().map(|arg| arg.to_string()))
            .chain([host.to_string(), command.to_string()])
            .collect(),
        env: None,
    })
}

pub fn resolve_launcher(
    spec: &LauncherSpec,
    interactive_ssh_auth: bool,
    ssh_platform: &str,
) -> Result<ResolvedLauncher, String> {
    match spec {
        LauncherSpec::Ssh { host, remote_bin } => {
            let remote_bin = remote_bin.clone().unwrap_or_else(|| DEFAULT_REMOTE_BIN.to_string());
            resolve_ssh_command(
                host,
                &if ssh_platform == "windows" && !is_bare_remote_bin(&remote_bin) {
                    exec_server_power_shell_command(&remote_bin)
                } else {
                    exec_server_shell_command(&remote_bin)
                },
                interactive_ssh_auth,
            )
        }
        LauncherSpec::Docker { container, context, .. } => {
            assert_launcher_operand("docker container", container)?;
            Ok(ResolvedLauncher {
                program: "docker".to_string(),
                args: docker_base_args(context.as_deref())
                    .into_iter()
                    .chain([
                        "exec".to_string(),
                        "-i".to_string(),
                        container.clone(),
                        "sh".to_string(),
                        "-c".to_string(),
                        format!(
                            "exec {}",
                            exec_server_shell_command(
                                spec.remote_bin().unwrap_or(DEFAULT_REMOTE_BIN)
                            )
                        ),
                    ])
                    .collect(),
                env: None,
            })
        }
        LauncherSpec::Command { program, args, env, .. } => {
            let remote_bin = launcher_remote_bin(spec);
            if let Some(remote_bin) = &remote_bin {
                assert_launcher_operand("remote bin", remote_bin)?;
            }
            let path_env = env.as_ref().and_then(|env| env.get("PATH")).map(String::as_str);
            let mut resolved_args = args.clone().unwrap_or_default();
            if let Some(remote_bin) = remote_bin {
                resolved_args.push(remote_bin);
                resolved_args.push(EXEC_SERVER_SUBCOMMAND.to_string());
            }
            Ok(ResolvedLauncher {
                program: resolve_program_path(program, path_env),
                args: resolved_args,
                env: Some(command_launcher_env(env.as_ref(), &std::env::vars().map(|(key, value)| (key, Some(value))).collect())),
            })
        }
    }
}
