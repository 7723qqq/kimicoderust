use kimi_core::primitives::errors::Error2;

use super::super::errors::{
    CAPABILITY_PROCESS_CONTROL_FAILED, register_capability_errors,
};
use super::super::host::{RunCommandOptions, run_command};
use super::context::CapabilityEntryContext;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsCuProcesses {
    pub powershell: String,
    pub count: u64,
}

pub fn windows_cu_process_script(stop: bool) -> String {
    let mut script = String::from(
        r#"
$ErrorActionPreference = 'Stop'
$candidates = @($env:KIMI_CU_WINDOWS_EXE)
if ($env:KIMI_CU_WINDOWS_HOME) { $candidates += (Join-Path $env:KIMI_CU_WINDOWS_HOME 'kimi-cu.exe') }
if ($env:LOCALAPPDATA) { $candidates += (Join-Path $env:LOCALAPPDATA 'KimiCU\kimi-cu.exe') }
if ($env:ProgramFiles) { $candidates += (Join-Path $env:ProgramFiles 'KimiCU\kimi-cu.exe') }
$paths = @($candidates | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | ForEach-Object { [IO.Path]::GetFullPath($_) })
$sessionId = (Get-Process -Id $PID).SessionId
$matched = @(Get-CimInstance Win32_Process -Filter "Name = 'kimi-cu.exe' AND SessionId = $sessionId" | Where-Object {
  $raw = $_.ExecutablePath
  if (-not $raw) { return $false }
  try { $exe = [IO.Path]::GetFullPath($raw) } catch { return $false }
  if ($paths -notcontains $exe) { return $false }
  $names = @([regex]::Escape($raw), [regex]::Escape($exe)) | Select-Object -Unique
  $alternation = ($names -join '|')
  $pattern = '^(?:"(?:' + $alternation + ')"|(?:' + $alternation + '))\s+(?:agent|agent-background|mcp)(?:\s|$)'
  return $_.CommandLine -match $pattern
})
"#,
    );
    if stop {
        script.push_str(
            r#"
foreach ($candidate in $matched) {
  $process = Get-Process -Id $candidate.ProcessId -ErrorAction SilentlyContinue
  if ($null -eq $process) { continue }
  try {
    $null = $process.Handle
    if ($process.HasExited) { continue }
    if ($process.MainModule.FileName -ine $candidate.ExecutablePath) { throw 'Kimi Computer Use process identity changed; retry installation' }
    $created = $candidate.CreationDate.ToUniversalTime()
    if ([Math]::Abs(($process.StartTime.ToUniversalTime() - $created).TotalMilliseconds) -gt 1) {
      throw 'Kimi Computer Use process identity changed; retry installation'
    }
    $process.Kill()
    if (-not $process.WaitForExit(10000)) { throw 'Kimi Computer Use process did not exit; retry installation' }
  } finally { $process.Dispose() }
}
"#,
        );
    }
    script.push_str(
        r#"
[Console]::Out.Write($matched.Count)
"#,
    );
    script
}

pub async fn manage_windows_cu_processes(
    ctx: &CapabilityEntryContext,
    powershell: &str,
    stop: bool,
) -> Result<u64, WireAsyncError> {
    register_capability_errors();
    let result = run_command(
        ctx.host_process.as_ref(),
        powershell,
        &string_args(&[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &windows_cu_process_script(stop),
        ]),
        RunCommandOptions {
            timeout_ms: Some(ctx.command_timeout_ms.unwrap_or(30_000)),
        },
    )
    .await?;
    let output = result.stdout.trim();
    if result.code != 0
        || output.is_empty()
        || !output.bytes().all(|byte| byte.is_ascii_digit())
    {
        let detail = nonempty_detail(&result.stderr, &result.stdout, result.code);
        return Err(Box::new(Error2::new(
            CAPABILITY_PROCESS_CONTROL_FAILED,
            format!(
                "Failed to {} Kimi Computer Use processes: {detail}",
                if stop { "stop" } else { "check" }
            ),
        )));
    }
    Ok(output.parse::<u64>().unwrap_or(0))
}

pub async fn find_windows_cu_processes(
    ctx: &CapabilityEntryContext,
    powershell_candidates: &[String],
) -> Result<WindowsCuProcesses, WireAsyncError> {
    register_capability_errors();
    let mut failures = Vec::new();
    for powershell in powershell_candidates {
        match manage_windows_cu_processes(ctx, powershell, false).await {
            Ok(count) => {
                return Ok(WindowsCuProcesses {
                    powershell: powershell.clone(),
                    count,
                });
            }
            Err(error) => failures.push(format!("{powershell}: {error}")),
        }
    }
    Err(Box::new(Error2::new(
        CAPABILITY_PROCESS_CONTROL_FAILED,
        format!(
            "Failed to check Kimi Computer Use processes: {}",
            failures.join("; ")
        ),
    )))
}

fn nonempty_detail(stderr: &str, stdout: &str, code: i32) -> String {
    let stderr = stderr.trim();
    let stdout = stdout.trim();
    if !stderr.is_empty() {
        stderr.to_string()
    } else if !stdout.is_empty() {
        stdout.to_string()
    } else {
        format!("exit code {code}")
    }
}

fn string_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}
