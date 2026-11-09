/// Bash process helpers shared by the managed-shell path.
///
/// The one-shot command execution primitives lived here alongside the
/// managed shell; they are served by `bash_spawn.rs` now. What remains is
/// the shared timeout ceiling (surfaced to JS as napi constants) and the
/// cross-platform process-tree kill.
use std::process::Command;

/// Default timeout for foreground commands (seconds).
pub const DEFAULT_TIMEOUT_S: u64 = 60;
/// Maximum timeout for foreground commands (seconds).
pub const MAX_TIMEOUT_S: u64 = 300;

/// Kill a process and its descendants.
///
/// - Unix: the child was spawned with `process_group(0)`, so signaling the
///   negative pid (the whole group) takes every descendant down.
/// - Windows: `taskkill /T` walks the tree; fall back to `child.kill()`.
pub(crate) fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        let _ = Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let pid = child.id();
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// Signal one process group (`pid` is the group leader the child was spawned
/// as, via `process_group(0)`).
///
/// Windows has no group signal, so `taskkill /T` walks the tree from the pid.
async fn signal_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        let _ = tokio::process::Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
}

/// Async [`kill_process_tree`] for a tokio child: signal the whole group, then
/// take the direct child down and reap it.
///
/// `Child::kill()` alone only stops the shell. A command like `npm run dev`
/// leaves its `node` grandchild alive, so every foreground/background timeout
/// and output-limit path goes through this instead.
pub(crate) async fn kill_process_tree_async(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        signal_process_tree(pid).await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Kill the tree of a child whose future was already dropped, so only its pid
/// survives. Used when a task registration fails after the future was handed
/// over: the drop stops the shell, this reaches its descendants.
pub(crate) async fn kill_process_tree_by_pid(pid: u32) {
    signal_process_tree(pid).await;
}
