use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::types::{HookAction, HookResult};

pub const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
pub const KILL_GRACE_MS: u64 = 100;
pub const MAX_HOOK_OUTPUT_CHARS: usize = 64 * 1024;
pub const OUTPUT_TRUNCATED_MARKER: &str = "\n[hook output truncated]";

#[derive(Debug, Clone)]
pub struct RunHookOptions {
    pub timeout: u64,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub signal: Option<CancellationToken>,
    pub fail_closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSpawnOptions {
    pub shell: bool,
    pub cwd: Option<String>,
    pub detached: bool,
    pub windows_hide: bool,
    pub env: Option<HashMap<String, String>>,
}

pub fn build_hook_spawn_options(
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
) -> HookSpawnOptions {
    let env = env.map(|env| {
        let mut merged: HashMap<String, String> = std::env::vars().collect();
        merged.extend(env);
        merged
    });
    HookSpawnOptions {
        shell: true,
        cwd,
        detached: !cfg!(windows),
        windows_hide: true,
        env,
    }
}

pub fn append_capped(current: &str, chunk: &str) -> (String, bool) {
    let current_len = current.encode_utf16().count();
    let remaining = MAX_HOOK_OUTPUT_CHARS.saturating_sub(current_len);
    if remaining == 0 {
        if chunk.is_empty() {
            return (current.to_string(), false);
        }
        if current.ends_with(OUTPUT_TRUNCATED_MARKER) {
            return (current.to_string(), true);
        }
        return (
            format!("{current}{OUTPUT_TRUNCATED_MARKER}"),
            true,
        );
    }
    if chunk.encode_utf16().count() <= remaining {
        return (format!("{current}{chunk}"), false);
    }
    let prefix = String::from_utf16_lossy(
        &chunk.encode_utf16().take(remaining).collect::<Vec<_>>(),
    );
    (format!("{current}{prefix}{OUTPUT_TRUNCATED_MARKER}"), true)
}

#[derive(Default)]
struct OutputCapture {
    text: String,
    truncated: bool,
}

pub async fn run_hook(
    command: &str,
    input: &Map<String, Value>,
    options: RunHookOptions,
) -> HookResult {
    let spawn = build_hook_spawn_options(options.cwd.clone(), options.env.clone());
    let mut builder = if cfg!(windows) {
        let mut builder = Command::new("cmd");
        builder.arg("/C").arg(command);
        builder
    } else {
        let mut builder = Command::new("sh");
        builder.arg("-c").arg(command);
        builder
    };
    if let Some(cwd) = spawn.cwd.filter(|cwd| !cwd.is_empty()) {
        builder.current_dir(cwd);
    }
    if let Some(env) = spawn.env {
        builder.envs(env);
    }
    #[cfg(windows)]
    {
        builder.creation_flags(0x08000000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        builder.process_group(0);
    }
    builder
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut process = match builder.spawn() {
        Ok(process) => process,
        Err(error) => {
            let message = error.to_string();
            return allow_result(HookResult {
                stderr: Some(message.clone()),
                errored: Some(true),
                error: Some(message),
                ..HookResult::default()
            });
        }
    };

    let stdin = process.stdin.take();
    let stdout_output = Arc::new(Mutex::new(OutputCapture::default()));
    let stderr_output = Arc::new(Mutex::new(OutputCapture::default()));
    let stdout_task = tokio::spawn(read_capped(
        process.stdout.take(),
        stdout_output.clone(),
    ));
    let stderr_task = tokio::spawn(read_capped(
        process.stderr.take(),
        stderr_output.clone(),
    ));
    let timeout = Duration::from_secs(timeout_seconds(options.timeout));
    let already_cancelled = options
        .signal
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled);
    if !already_cancelled
        && let Some(mut stdin) = stdin
    {
        let payload = serde_json::to_vec(input).unwrap_or_else(|_| b"{}".to_vec());
        tokio::spawn(async move {
            let _ = stdin.write_all(&payload).await;
        });
    }
    let cancelled = async {
        match &options.signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };

    enum Completion {
        Exit(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Cancelled,
    }

    let completion = tokio::select! {
        biased;
        () = cancelled => Completion::Cancelled,
        status = process.wait() => Completion::Exit(status),
        () = tokio::time::sleep(timeout) => Completion::TimedOut,
    };

    match completion {
        Completion::Exit(Ok(status)) => {
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            let (stdout, stdout_truncated) = output_snapshot(&stdout_output);
            let (stderr, stderr_truncated) = output_snapshot(&stderr_output);
            result_from_exit_code(
                status.code().unwrap_or(-1),
                stdout,
                stderr,
                stdout_truncated || stderr_truncated,
            )
        }
        Completion::Exit(Err(error)) => {
            stdout_task.abort();
            stderr_task.abort();
            let (stdout, _) = output_snapshot(&stdout_output);
            let (stderr, _) = output_snapshot(&stderr_output);
            let error = error.to_string();
            allow_result(HookResult {
                stdout: Some(stdout),
                stderr: Some(format!("{stderr}{error}")),
                errored: Some(true),
                error: Some(error),
                ..HookResult::default()
            })
        }
        Completion::TimedOut => {
            kill_process(process);
            let (stdout, stdout_truncated) = output_snapshot(&stdout_output);
            let (stderr, stderr_truncated) = output_snapshot(&stderr_output);
            let output_truncated = stdout_truncated || stderr_truncated;
            if options.fail_closed {
                return HookResult {
                    action: HookAction::Block,
                    message: Some("Permission hook timed out".to_string()),
                    reason: Some("Permission hook timed out".to_string()),
                    stderr: Some(format!("{stderr} (timed out)")),
                    output_truncated: Some(output_truncated),
                    ..HookResult::default()
                };
            }
            allow_result(HookResult {
                stdout: Some(stdout),
                stderr: Some(stderr),
                timed_out: Some(true),
                output_truncated: Some(output_truncated),
                ..HookResult::default()
            })
        }
        Completion::Cancelled => {
            kill_process(process);
            let (stdout, stdout_truncated) = output_snapshot(&stdout_output);
            let (stderr, stderr_truncated) = output_snapshot(&stderr_output);
            allow_result(HookResult {
                stdout: Some(stdout),
                stderr: Some(stderr),
                output_truncated: Some(stdout_truncated || stderr_truncated),
                ..HookResult::default()
            })
        }
    }
}

fn timeout_seconds(timeout: u64) -> u64 {
    if timeout > 0 {
        timeout
    } else {
        DEFAULT_TIMEOUT_SECONDS
    }
}

fn output_snapshot(output: &Arc<Mutex<OutputCapture>>) -> (String, bool) {
    let output = output.lock().unwrap_or_else(|poison| poison.into_inner());
    (output.text.clone(), output.truncated)
}

fn append_output(output: &Arc<Mutex<OutputCapture>>, chunk: &str) {
    let mut output = output.lock().unwrap_or_else(|poison| poison.into_inner());
    let (text, truncated) = append_capped(&output.text, chunk);
    output.text = text;
    output.truncated |= truncated;
}

#[cfg(unix)]
fn send_signal(process: &tokio::process::Child, signal: i32) {
    let Some(process_id) = process.id() else {
        return;
    };
    let process_id = process_id as i32;
    let result = unsafe { libc::kill(-process_id, signal) };
    if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
        unsafe {
            libc::kill(process_id, signal);
        }
    }
}

#[cfg(windows)]
fn request_kill(process: &tokio::process::Child) {
    let Some(process_id) = process.id() else {
        return;
    };
    use std::os::windows::process::CommandExt;
    let process_id = process_id.to_string();
    let mut command = Command::new("taskkill");
    command
        .args(["/T", "/F", "/PID", process_id.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000);
    let _ = command.spawn();
}

fn kill_process(process: tokio::process::Child) {
    #[cfg(unix)]
    send_signal(&process, libc::SIGTERM);
    #[cfg(windows)]
    request_kill(&process);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(KILL_GRACE_MS)).await;
        #[cfg(unix)]
        send_signal(&process, libc::SIGKILL);
        #[cfg(windows)]
        {
            let mut process = process;
            let _ = process.kill().await;
        }
    });
}

async fn read_capped<R>(stream: Option<R>, output: Arc<Mutex<OutputCapture>>)
where
    R: AsyncRead + Unpin,
{
    let Some(mut stream) = stream else {
        return;
    };
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut buffer = [0u8; 8192];
    while let Ok(read) = stream.read(&mut buffer).await {
        if read == 0 {
            let mut chunk = String::with_capacity(4);
            decoder.decode_to_string(&[], &mut chunk, true);
            append_output(&output, &chunk);
            break;
        }
        let mut chunk = String::with_capacity(read * 3 + 4);
        decoder.decode_to_string(&buffer[..read], &mut chunk, false);
        append_output(&output, &chunk);
    }
}

fn result_from_exit_code(
    exit_code: i32,
    stdout: String,
    stderr: String,
    output_truncated: bool,
) -> HookResult {
    if exit_code == 2 {
        let message = trim_js(&stderr).to_string();
        return HookResult {
            action: HookAction::Block,
            message: Some(message.clone()),
            reason: Some(message),
            stdout: Some(stdout),
            stderr: Some(stderr),
            exit_code: Some(exit_code),
            output_truncated: Some(output_truncated),
            ..HookResult::default()
        };
    }

    let structured = (exit_code == 0)
        .then(|| structured_output(&stdout))
        .flatten();
    if let Some(output) = &structured
        && output.action == Some(HookAction::Block)
    {
        return HookResult {
            action: HookAction::Block,
            message: output.message.clone().or_else(|| output.reason.clone()),
            reason: output.reason.clone(),
            stdout: Some(stdout),
            stderr: Some(stderr),
            exit_code: Some(exit_code),
            structured_output: Some(true),
            output_truncated: Some(output_truncated),
            ..HookResult::default()
        };
    }
    let structured_output = structured.is_some();
    allow_result(HookResult {
        message: structured.and_then(|output| output.message),
        stdout: Some(stdout),
        stderr: Some(stderr),
        exit_code: Some(exit_code),
        structured_output: structured_output.then_some(true),
        output_truncated: Some(output_truncated),
        ..HookResult::default()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StructuredOutput {
    action: Option<HookAction>,
    reason: Option<String>,
    message: Option<String>,
}

fn structured_output(stdout: &str) -> Option<StructuredOutput> {
    let text = trim_js(stdout);
    if text.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    let specific = object.get("hookSpecificOutput").and_then(Value::as_object);
    let message = optional_string(object.get("message"))
        .or_else(|| optional_string(specific.and_then(|output| output.get("message"))));
    let deny = specific
        .and_then(|output| output.get("permissionDecision"))
        .and_then(Value::as_str)
        == Some("deny");
    let reason = specific
        .and_then(|output| output.get("permissionDecisionReason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(StructuredOutput {
        action: deny.then_some(HookAction::Block),
        reason,
        message,
    })
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(js_number_string(number)),
        Some(Value::Bool(flag)) => Some(flag.to_string()),
        _ => None,
    }
}

fn js_number_string(number: &serde_json::Number) -> String {
    let raw = number.to_string();
    let source = serde_json::to_string(&raw).unwrap_or_else(|_| "\"0\"".to_string());
    let converted = rquickjs::Runtime::new().and_then(|runtime| {
        rquickjs::Context::full(&runtime).and_then(|context| {
            context.with(|ctx| ctx.eval::<String, _>(format!("String(Number({source}))")))
        })
    });
    converted.unwrap_or(raw)
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn allow_result(mut result: HookResult) -> HookResult {
    result.action = HookAction::Allow;
    result
}
