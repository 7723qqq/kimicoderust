use serde_json::Value;
use tokio::time::Instant;

use super::task_wait::WAIT_FOR_MAX_TIMEOUT_S;
use crate::callbacks::HostCallbacks;
use crate::rpc::types::{StateReadRequest, StateWriteRequest};
use crate::storage::TaskWaitResult;
use crate::tools::task_format;
use crate::tools::task_tools::{
    STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE, is_task_not_found, is_terminal_status,
    output_preview, render_task_entry, truncated_line,
};
use crate::tools::{arg_error_text, err_result, ok_result};
use crate::turn_loop::types::ExecutableToolResult;

#[derive(Default)]
pub struct WaitTally {
    turn_id: Option<usize>,
    calls: u32,
    waited_ms: u64,
}

impl WaitTally {
    fn count_call(&mut self, turn_id: usize) {
        if self.turn_id != Some(turn_id) {
            *self = Self {
                turn_id: Some(turn_id),
                calls: 0,
                waited_ms: 0,
            };
        }
        self.calls += 1;
    }
}

pub struct WaitContext<'a> {
    pub tally: &'a std::sync::Arc<std::sync::Mutex<WaitTally>>,

    pub turn_id: usize,

    pub caller_agent_id: &'a str,

    pub display_name: &'a str,

    pub parent_cancel: Option<&'a crate::subagent::types::ParentCancel>,

    /// Streaming progress sink: `Some` when the host observes updates.
    pub on_update: Option<&'a (dyn Fn(&crate::tools::ToolUpdate<'_>) + Send + Sync)>,

    /// Telemetry sink (`HostCallbacks::telemetry`), for `wait_for_completed`.
    pub telemetry: Option<&'a (dyn Fn(Value) + Send + Sync)>,
}

impl WaitContext<'_> {
    fn lock_tally(&self) -> std::sync::MutexGuard<'_, WaitTally> {
        self.tally.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_subagent(&self) -> bool {
        self.caller_agent_id != "main"
    }

    fn track(
        &self,
        task_id: Option<&str>,
        started: Instant,
        timeout_ms: u64,
        outcome: &str,
        extra_completed_count: usize,
    ) {
        if let Some(telemetry) = self.telemetry {
            telemetry(serde_json::json!({
                "event": "wait_for_completed",
                "outcome": outcome,
                "timeout_ms": timeout_ms,
                "waited_ms": waited_ms(started),
                "has_task_id": task_id.is_some(),
                "extra_completed_count": extra_completed_count,
            }));
        }
    }

    fn emit_progress(&self, timeout_s: u64, elapsed_ms: u128, running: usize) {
        let Some(on_update) = self.on_update else {
            return;
        };
        let text = format!(
            "Waiting {} / {} · {running} background task{} still running",
            format_wait_seconds(elapsed_ms as u64),
            format_wait_seconds(timeout_s.saturating_mul(1000)),
            if running == 1 { "" } else { "s" },
        );
        on_update(&crate::tools::ToolUpdate {
            kind: "status",
            text: &text,
            percent: None,
            custom_kind: None,
            custom_data: None,
            replace: true,
        });
    }

    async fn with_repeat_warning(
        &self,
        mut result: ExecutableToolResult,
        calls: u32,
        callbacks: &dyn HostCallbacks,
        args: &Value,
    ) -> ExecutableToolResult {
        if calls < 2 {
            return result;
        }
        let waited_ms = self.lock_tally().waited_ms;
        let advice = if self.is_subagent() {
            subagent_repeat_advice(self.display_name)
        } else if goal_is_active(callbacks, args).await {
            goal_repeat_advice(self.display_name)
        } else {
            default_repeat_advice(self.display_name)
        };
        let waited = format_wait_seconds(waited_ms);
        result.content = format!(
            "{}\n\n[wait_warning]\nThis is {} call {} in this turn, and you have already spent {} waiting. {}",
            result.content, self.display_name, calls, waited, advice
        );
        result
    }
}

fn format_wait_seconds(waited_ms: u64) -> String {
    let total_seconds = (waited_ms + 500) / 1000;
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes < 60 {
        return if seconds == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m {seconds:02}s")
        };
    }
    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    if remaining_minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {remaining_minutes:02}m")
    }
}

fn subagent_repeat_advice(name: &str) -> String {
    format!(
        "Stop waiting by reflex: repeated waits burn time your caller is waiting on. Before calling {name} again, do every part of your task that does not depend on the running background task. Wait again only for a result you truly cannot finish without — ending your turn is your final hand-off, so do not hand off without it, but never wait for tasks you do not need."
    )
}

fn goal_repeat_advice(name: &str) -> String {
    format!(
        "Stop waiting by reflex: repeated waits stall the goal. Before calling {name} again, do every piece of remaining goal work that does not depend on the running background task — there is almost always some. Wait again only if nothing else can proceed until it finishes; even then, {name} beats polling with Bash sleep or ending the turn only to be continued again."
    )
}

fn default_repeat_advice(name: &str) -> String {
    format!(
        "Stop calling {name}. Repeated waiting wastes the user's time and is almost never the right move. Do not call it again in this turn unless the user explicitly asked you to wait. Do something useful now — another part of the task, or verifying earlier work — or end your turn with a progress update. Finished background tasks notify you automatically, so you will not miss the result."
    )
}

async fn goal_is_active(callbacks: &dyn HostCallbacks, args: &Value) -> bool {
    let request = StateReadRequest {
        domain: "goal".into(),
        key: "goal".into(),
        turn_id: args
            .get("turn_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        tool_call_id: args
            .get("tool_call_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
    };
    match callbacks.state_read(request).await {
        Ok(response) => response.value["goal"]["status"].as_str() == Some("active"),
        Err(_) => false,
    }
}

pub async fn execute_task_wait(
    callbacks: &dyn HostCallbacks,
    runner: Option<&crate::storage::TaskRunner>,
    steer: Option<&crate::subagent::types::ParentCancel>,
    wait: &WaitContext<'_>,
    args: &Value,
) -> Result<ExecutableToolResult, crate::wire::record::WireAsyncError> {
    let task_id = match parse_optional_task_id(args) {
        Ok(task_id) => task_id,
        Err(message) => return Ok(err_result(message)),
    };
    let timeout_s = match parse_timeout(args) {
        Ok(timeout) => timeout,
        Err(message) => return Ok(err_result(message)),
    };
    let timeout_ms = timeout_s * 1000;
    if !wait_for_enabled() {
        return Ok(err_result(
            "WaitFor is disabled: the wait_for experimental flag is off.".into(),
        ));
    }

    let calls = {
        let mut tally = wait.lock_tally();
        tally.count_call(wait.turn_id);
        tally.calls
    };
    let started = Instant::now();
    let named_task = runner.and_then(|runner| task_id.as_deref().and_then(|id| runner.entry(id)));
    if let Some(runner) = runner
        && (task_id.is_none() || named_task.is_some())
    {
        let (result, waited) = wait_with_runner(
            runner,
            steer,
            task_id.as_deref(),
            named_task,
            timeout_ms,
            started,
            wait,
        )
        .await?;
        if waited {
            wait.lock_tally().waited_ms += waited_ms(started);
        }
        return Ok(wait
            .with_repeat_warning(result, calls, callbacks, args)
            .await);
    }
    let Some(task_id) = task_id else {
        return Ok(err_result(
            "Invalid TaskWait arguments: `task_id` is required when the host owns background tasks."
                .into(),
        ));
    };
    let request = StateWriteRequest {
        domain: "task".into(),
        key: "task".into(),
        value: serde_json::json!({ "action": "wait", "id": task_id, "timeout_ms": timeout_ms }),
        undoable: false,
        turn_id: args
            .get("turn_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        tool_call_id: args
            .get("tool_call_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
    };
    let response = tokio::select! {
        biased;
        () = wait_for_signal(wait.parent_cancel) => {
            return Err(aborted_wait(wait, Some(&task_id), timeout_ms, started));
        }
        () = wait_for_signal(steer) => {
            let waited = waited_ms(started);
            wait.lock_tally().waited_ms += waited;
            wait.track(Some(&task_id), started, timeout_ms, "interrupted", 0);
            let result = render_wait_interrupted(Some(&task_id), timeout_ms, waited, &[]);
            return Ok(wait.with_repeat_warning(result, calls, callbacks, args).await);
        }
        response = async { callbacks.state_write(request).await } => response,
    };
    Ok(match response {
        Ok(response) => {
            let waited = waited_ms(started);
            wait.lock_tally().waited_ms += waited;
            let status = response.value.get("status").and_then(Value::as_str);
            wait.track(
                Some(&task_id),
                started,
                timeout_ms,
                if status.is_some_and(is_terminal_status) {
                    "completed"
                } else {
                    "timed_out"
                },
                0,
            );
            let result = render_task_wait(
                &response.value,
                Some(&task_id),
                timeout_ms,
                waited,
                &[],
                &[],
                wait,
            );
            wait.with_repeat_warning(result, calls, callbacks, args)
                .await
        }

        Err(error) => {
            let lower = error.to_lowercase();
            let not_found = is_task_not_found(&error);
            let timed_out = lower.contains("timed out") || lower.contains("timeout");
            if not_found || timed_out {
                wait.track(
                    Some(&task_id),
                    started,
                    timeout_ms,
                    if not_found {
                        "task_not_found"
                    } else {
                        "timed_out"
                    },
                    0,
                );
            }
            let result = map_wait_error(error, &task_id, timeout_ms, waited_ms(started), wait);
            if not_found || timed_out {
                wait.with_repeat_warning(result, calls, callbacks, args)
                    .await
            } else {
                result
            }
        }
    })
}

async fn wait_with_runner(
    runner: &crate::storage::TaskRunner,
    steer: Option<&crate::subagent::types::ParentCancel>,
    task_id: Option<&str>,
    named_task: Option<Value>,
    timeout_ms: u64,
    started: Instant,
    wait: &WaitContext<'_>,
) -> Result<(ExecutableToolResult, bool), crate::wire::record::WireAsyncError> {
    let running_at_start: Vec<String> = running_wires(runner)
        .iter()
        .filter_map(|entry| entry.get("taskId").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    if task_id.is_none() && running_at_start.is_empty() {
        wait.track(None, started, timeout_ms, "completed", 0);
        return Ok((ok_result(render_wait_no_tasks(timeout_ms)), false));
    }

    let terminal = named_task.filter(|entry| {
        entry
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(is_terminal_status)
    });
    let result = if let Some(entry) = terminal {
        TaskWaitResult::Completed(entry)
    } else {
        let waiting = async {
            match task_id {
                Some(id) => runner.wait_interruptible(id, timeout_ms, steer).await,
                None => runner.wait_any(&running_at_start, timeout_ms, steer).await,
            }
        };
        tokio::pin!(waiting);
        let interval = std::time::Duration::from_secs(1);
        let mut progress = tokio::time::interval_at(Instant::now() + interval, interval);
        progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = wait_for_signal(wait.parent_cancel) => {
                    return Err(aborted_wait(wait, task_id, timeout_ms, started));
                }
                result = &mut waiting => break result,
                _ = progress.tick(), if wait.on_update.is_some() => {
                    wait.emit_progress(
                        timeout_ms / 1000,
                        started.elapsed().as_millis(),
                        running_wires(runner).len(),
                    );
                }
            }
        }
    };
    let waited = waited_ms(started);
    let still_running = running_wires(runner);
    let (rendered, outcome, extra_count) = match result {
        TaskWaitResult::Interrupted => (
            render_wait_interrupted(task_id, timeout_ms, waited, &still_running),
            "interrupted",
            0,
        ),
        TaskWaitResult::NotFound => (
            err_result(format!("Task not found: {}", task_id.unwrap_or(""))),
            "task_not_found",
            0,
        ),
        TaskWaitResult::Completed(entry) | TaskWaitResult::TimedOut(entry) => {
            if entry
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(is_terminal_status)
            {
                let finished_id = entry
                    .get("taskId")
                    .or_else(|| entry.get("id"))
                    .and_then(Value::as_str)
                    .or(task_id)
                    .unwrap_or_default();
                let extras: Vec<Value> = running_at_start
                    .iter()
                    .filter(|id| id.as_str() != finished_id)
                    .filter_map(|id| runner.entry(id))
                    .filter(|entry| {
                        entry
                            .get("status")
                            .and_then(Value::as_str)
                            .is_some_and(is_terminal_status)
                    })
                    .collect();
                let rendered = render_wait_completed(
                    &entry,
                    finished_id,
                    timeout_ms,
                    waited,
                    &still_running,
                    &extras,
                );
                mark_delivered_via_wait(runner, &entry, &extras);
                (rendered, "completed", extras.len())
            } else {
                (
                    render_wait_timeout(task_id, timeout_ms, waited, &still_running, wait),
                    "timed_out",
                    0,
                )
            }
        }
    };
    wait.track(task_id, started, timeout_ms, outcome, extra_count);
    Ok((rendered, true))
}

fn mark_delivered_via_wait(runner: &crate::storage::TaskRunner, entry: &Value, extras: &[Value]) {
    let delivered = std::iter::once(entry)
        .chain(extras)
        .filter_map(|entry| {
            let id = entry.get("taskId").or_else(|| entry.get("id"))?.as_str()?;
            let status = entry.get("status")?.as_str()?;
            Some((id.to_string(), status.to_string()))
        })
        .collect::<Vec<_>>();
    runner.mark_notifications_delivered_via_wait(&delivered);
}

fn running_wires(runner: &crate::storage::TaskRunner) -> Vec<Value> {
    runner
        .list()
        .into_iter()
        .filter(|entry| {
            entry
                .get("status")
                .and_then(|s| s.as_str())
                .is_none_or(|status| !is_terminal_status(status))
        })
        .collect()
}

fn render_task_wait(
    value: &Value,
    requested_task_id: Option<&str>,
    timeout_ms: u64,
    waited_ms: u64,
    still_running: &[Value],
    extras: &[Value],
    wait: &WaitContext<'_>,
) -> ExecutableToolResult {
    let Some(obj) = value.as_object() else {
        return err_result("Invalid task state from host: expected a task output snapshot.".into());
    };
    let status = obj.get("status").and_then(|s| s.as_str()).unwrap_or("");

    let task_id = obj
        .get("taskId")
        .or_else(|| obj.get("id"))
        .and_then(|v| v.as_str())
        .or(requested_task_id)
        .unwrap_or("");
    if is_terminal_status(status) {
        render_wait_completed(value, task_id, timeout_ms, waited_ms, still_running, extras)
    } else {
        render_wait_timeout(
            requested_task_id,
            timeout_ms,
            waited_ms,
            still_running,
            wait,
        )
    }
}

fn render_wait_completed(
    value: &Value,
    task_id: &str,
    timeout_ms: u64,
    waited_ms: u64,
    still_running: &[Value],
    extras: &[Value],
) -> ExecutableToolResult {
    let mut lines = vec![
        wait_metadata("completed", None, Some(task_id), timeout_ms, waited_ms),
        String::new(),
        "[finished]".into(),
        render_task_entry(value),
    ];
    lines.push(String::new());
    if let Some(truncated) = truncated_line(value) {
        lines.push(truncated);
    }
    lines.push("[output]".into());
    lines.push(output_preview(value));

    if !extras.is_empty() {
        lines.push(String::new());
        lines.push("[completed_during_wait]".into());
        lines.push(
            extras
                .iter()
                .map(render_task_entry)
                .collect::<Vec<_>>()
                .join("\n---\n"),
        );
        lines.push(
            "Use TaskOutput with one of the task_id values above to read the full output.".into(),
        );
    }
    push_still_running(&mut lines, still_running);
    ok_result(lines.join("\n"))
}

fn render_wait_timeout(
    task_id: Option<&str>,
    timeout_ms: u64,
    waited_ms: u64,
    still_running: &[Value],
    wait: &WaitContext<'_>,
) -> ExecutableToolResult {
    let body = if wait.is_subagent() {
        format!(
            "The wait ended before the task finished — a timeout is not an error. If you need its result, call {} again: ending your turn is your hand-off, and a completion notification after it reaches no one.",
            wait.display_name
        )
    } else {
        "The wait ended before the task finished — a timeout is not an error. Prefer continuing with other work over waiting again; completion arrives via automatic notification.".to_string()
    };
    let mut lines = vec![
        wait_metadata("timed_out", None, task_id, timeout_ms, waited_ms),
        String::new(),
        body,
    ];
    push_still_running(&mut lines, still_running);
    ok_result(lines.join("\n"))
}

fn render_wait_interrupted(
    task_id: Option<&str>,
    timeout_ms: u64,
    waited_ms: u64,
    still_running: &[Value],
) -> ExecutableToolResult {
    let mut lines = vec![
        wait_metadata("interrupted", Some("steer"), task_id, timeout_ms, waited_ms),
        String::new(),
        "New input ended this wait early. Read the new input before deciding what to do next. Background tasks have not been stopped; completion still arrives via automatic notification.".into(),
    ];
    push_still_running(&mut lines, still_running);
    ok_result(lines.join("\n"))
}

fn render_wait_no_tasks(timeout_ms: u64) -> String {
    format!(
        "{}\n\nNo background tasks are running, so there is nothing to wait for. Finished tasks report back via automatic notification.",
        wait_metadata("no_tasks", None, None, timeout_ms, 0)
    )
}

fn push_still_running(lines: &mut Vec<String>, still_running: &[Value]) {
    if still_running.is_empty() {
        return;
    }
    lines.push(String::new());
    lines.push("[still_running]".into());
    lines.push(format!("active_background_tasks: {}", still_running.len()));
    let records: Vec<String> = still_running.iter().map(render_task_entry).collect();
    lines.push(records.join("\n---\n"));
}

fn wait_metadata(
    status: &str,
    reason: Option<&str>,
    task_id: Option<&str>,
    timeout_ms: u64,
    waited_ms: u64,
) -> String {
    let mut entries: Vec<(&str, Value)> = vec![("waitStatus", Value::String(status.into()))];
    if let Some(reason) = reason {
        entries.push(("reason", Value::String(reason.into())));
    }
    if let Some(task_id) = task_id {
        entries.push(("taskId", Value::String(task_id.into())));
    }
    entries.push(("waitedMs", Value::from(waited_ms)));
    entries.push(("timeoutMs", Value::from(timeout_ms)));
    let refs: Vec<(&str, &Value)> = entries.iter().map(|(k, v)| (*k, v)).collect();
    task_format::format_plain_object_entries(&refs)
}

fn parse_optional_task_id(args: &Value) -> Result<Option<String>, String> {
    match args.get("task_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(task_id)) if !task_id.is_empty() => Ok(Some(task_id.clone())),
        Some(_) => Err(
            "Invalid TaskWait arguments: `task_id` must be a non-empty string when provided."
                .into(),
        ),
    }
}

fn parse_timeout(args: &Value) -> Result<u64, String> {
    match args.get("timeout") {
        Some(value) => match value.as_u64() {
            Some(timeout) if (1..=WAIT_FOR_MAX_TIMEOUT_S).contains(&timeout) => Ok(timeout),
            _ => Err(format!(
                "Invalid TaskWait arguments: `timeout` must be an integer between 1 and {WAIT_FOR_MAX_TIMEOUT_S}."
            )),
        },
        None => Err(arg_error_text(
            "TaskWait",
            "timeout",
            "engine.tools.argRequired",
        )),
    }
}

async fn wait_for_signal(signal: Option<&crate::subagent::types::ParentCancel>) {
    match signal {
        Some(signal) => signal.wait().await,
        None => std::future::pending::<()>().await,
    }
}

fn aborted_wait(
    wait: &WaitContext<'_>,
    task_id: Option<&str>,
    timeout_ms: u64,
    started: Instant,
) -> crate::wire::record::WireAsyncError {
    wait.track(task_id, started, timeout_ms, "aborted", 0);
    Box::new(crate::_base::utils::abort::user_cancellation_reason())
}

fn waited_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn map_wait_error(
    error: String,
    task_id: &str,
    timeout_ms: u64,
    waited_ms: u64,
    wait: &WaitContext<'_>,
) -> ExecutableToolResult {
    if error.contains("does not support state bridge") {
        err_result(STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE.into())
    } else if is_task_not_found(&error) {
        err_result(format!("Task not found: {task_id}"))
    } else {
        let lower = error.to_lowercase();
        if lower.contains("timed out") || lower.contains("timeout") {
            render_wait_timeout(Some(task_id), timeout_ms, waited_ms, &[], wait)
        } else {
            err_result(error)
        }
    }
}

const TASK_WAIT_DESCRIPTION: &str = r#"Wait for background tasks to finish without ending the current turn.

Only call this tool when you really have no other work to do and your next step cannot proceed without the result of a running background task (a sub-agent, a background bash command, or a background AskUserQuestion). The call suspends inside the current turn until the task finishes or the timeout elapses, then returns the outcome so you can keep working in the same turn. While waiting, no LLM requests are made, but the user is kept waiting too.

When background tasks finish, they notify you automatically, so you do not need to busily wait for them.

Guidelines:

- Before calling WaitFor, think about what else you can do meanwhile: another part of the task, verifying earlier work, or ending your turn with a progress update. If there is anything, do that instead.
- Do not call WaitFor right after dispatching work whose result you do not need yet.
- After a timeout, prefer moving on to other work over calling WaitFor again; repeated waits keep the user waiting.
- Without `task_id`, tasks started during the wait are not covered by it; their completion arrives via the usual automatic notification.
- With `task_id`, the wait ends when that task finishes. An unknown `task_id` is an error; a task that has already finished returns immediately.
- When no background tasks are running, WaitFor returns immediately without waiting.
- Waiting has no side effects on the waited tasks: WaitFor never stops a task, and interrupting the wait (for example, a new user message) leaves every task running.
- A finished task's result is delivered exactly once: tasks reported by WaitFor do not also produce an automatic completion notification.
- You can only wait for background tasks started by this agent; task IDs belonging to other agents are unknown here."#;

fn task_wait_def(name: &str) -> crate::turn_loop::types::ToolInfo {
    crate::turn_loop::types::ToolInfo {
        name: name.into(),
        description: TASK_WAIT_DESCRIPTION.replace("WaitFor", name),
        input_schema: super::task_wait::wait_for_input_schema(),
    }
}

pub fn wait_for_flag(environment: Option<bool>, config: Option<bool>) -> bool {
    environment.or(config).unwrap_or(true)
}

/// v2's `FlagService.explain` precedence (`app/flag/flagService.ts:82-118`):
/// the flag's own env var when it parses as a boolean, else the
/// `[experimental] wait_for` config entry, else the
/// `KIMI_CODE_EXPERIMENTAL_FLAG` master switch, else the registered default.
/// An unparsable env value falls through instead of forcing the flag on.
pub fn wait_for_enabled() -> bool {
    let environment = crate::_base::utils::env::parse_boolean_env(
        std::env::var("KIMI_CODE_EXPERIMENTAL_WAIT_FOR")
            .ok()
            .as_deref(),
    );
    if let Some(environment) = environment {
        return environment;
    }
    let config = crate::config::KimiConfig::discover()
        .ok()
        .and_then(|(config, _)| match config.experimental.get("wait_for") {
            Some(crate::config::ExperimentalValue::Bool(value)) => Some(*value),
            Some(crate::config::ExperimentalValue::String(value)) => {
                crate::env::parse_bool_env(Some(value))
            }
            None => None,
        });
    if let Some(config) = config {
        return config;
    }
    if crate::_base::utils::env::parse_boolean_env(
        std::env::var("KIMI_CODE_EXPERIMENTAL_FLAG").ok().as_deref(),
    ) == Some(true)
    {
        return true;
    }
    true
}

pub fn wait_for_tool_def() -> crate::turn_loop::types::ToolInfo {
    task_wait_def("WaitFor")
}

pub fn task_wait_tool_def() -> crate::turn_loop::types::ToolInfo {
    task_wait_def("TaskWait")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::task_tools::tests::{read_ok, sample_snapshot, scripted, write_ok};
    use std::sync::Arc;

    fn test_tally() -> Arc<std::sync::Mutex<WaitTally>> {
        Arc::new(std::sync::Mutex::new(WaitTally::default()))
    }

    fn main_wait_ctx(tally: &Arc<std::sync::Mutex<WaitTally>>) -> WaitContext<'_> {
        WaitContext {
            tally,
            turn_id: 1,
            caller_agent_id: "main",
            display_name: "WaitFor",
            parent_cancel: None,
            on_update: None,
            telemetry: None,
        }
    }

    #[tokio::test]
    async fn test_wait_sends_action_and_renders_completed() {
        let (callbacks, read_received, write_received) =
            scripted(read_ok(Value::Null), write_ok(sample_snapshot()));
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let result = execute_task_wait(
            &callbacks,
            None,
            None,
            &wait,
            &serde_json::json!({ "task_id": "task-1", "timeout": 30 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(!result.is_error);
        let lines: Vec<&str> = result.content.lines().collect();
        assert_eq!(lines[0], "wait_status: completed");
        assert_eq!(lines[1], "task_id: task-1");
        assert!(lines[2].starts_with("waited_ms: "));
        assert_eq!(lines[3], "timeout_ms: 30000");
        assert!(lines.contains(&"[finished]"));
        assert!(result.content.contains("status: completed"));
        assert!(result.content.contains("[output]"));
        assert!(result.content.contains("All tests passed."));
        assert!(read_received.lock().unwrap().is_none());
        let request = write_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.domain, "task");
        assert_eq!(request.key, "task");
        assert!(!request.undoable);
        assert_eq!(request.value["action"], "wait");
        assert_eq!(request.value["id"], "task-1");
        assert_eq!(request.value["timeout_ms"], 30000);
    }

    #[test]
    fn test_wait_completed_render_golden() {
        let tally = test_tally();
        let rendered = render_task_wait(
            &sample_snapshot(),
            Some("task-1"),
            30000,
            1234,
            &[],
            &[],
            &main_wait_ctx(&tally),
        );
        assert!(!rendered.is_error);
        assert_eq!(
            rendered.content,
            "wait_status: completed\n\
             task_id: task-1\n\
             waited_ms: 1234\n\
             timeout_ms: 30000\n\
             \n\
             [finished]\n\
             Wall time: 1.000 seconds\n\
             description: Running tests\n\
             full_output_available: true\n\
             output_path: C:/logs/task-1.log\n\
             output_size_bytes: 1024\n\
             preview_bytes: 512\n\
             status: completed\n\
             task_id: task-1\n\
             truncated: false\n\
             \n\
             [output]\n\
             All tests passed."
        );
    }

    #[test]
    fn test_wait_completed_lists_tasks_finished_during_the_wait() {
        let tally = test_tally();
        let extra = serde_json::json!({
            "taskId": "task-2",
            "description": "Formatting",
            "status": "completed",
            "startedAt": 1700000000000u64,
            "endedAt": 1700000001000u64
        });
        let rendered = render_task_wait(
            &sample_snapshot(),
            Some("task-1"),
            30000,
            1234,
            &[],
            &[extra],
            &main_wait_ctx(&tally),
        );
        assert!(!rendered.is_error);
        assert!(
            rendered.content.contains("[completed_during_wait]"),
            "content: {}",
            rendered.content
        );
        assert!(
            rendered.content.contains("task_id: task-2"),
            "content: {}",
            rendered.content
        );
        assert!(
            rendered
                .content
                .contains("Use TaskOutput with one of the task_id values above"),
            "content: {}",
            rendered.content
        );
    }

    #[test]
    fn test_wait_entry_only_response_includes_an_empty_output_section() {
        let entry = serde_json::json!({
            "taskId": "task-1",
            "description": "Running tests",
            "status": "completed",
            "startedAt": 1700000000000u64,
            "endedAt": 1700000001000u64
        });
        let tally = test_tally();
        let rendered = render_task_wait(
            &entry,
            Some("task-1"),
            30000,
            1234,
            &[],
            &[],
            &main_wait_ctx(&tally),
        );
        assert!(!rendered.is_error);
        assert_eq!(
            rendered.content,
            "wait_status: completed\n\
             task_id: task-1\n\
             waited_ms: 1234\n\
             timeout_ms: 30000\n\
             \n\
             [finished]\n\
             Wall time: 1.000 seconds\n\
             description: Running tests\n\
             status: completed\n\
             task_id: task-1\n\
             \n\
             [output]\n\
             [no output available]"
        );
        assert!(rendered.content.contains("[output]"));
    }

    #[test]
    fn test_wait_non_terminal_status_renders_timeout() {
        let mut snapshot = sample_snapshot();
        snapshot["status"] = serde_json::json!("running");
        let tally = test_tally();
        let rendered = render_task_wait(
            &snapshot,
            Some("task-1"),
            30000,
            30000,
            &[],
            &[],
            &main_wait_ctx(&tally),
        );
        assert!(!rendered.is_error);
        assert_eq!(
            rendered.content,
            "wait_status: timed_out\n\
             task_id: task-1\n\
             waited_ms: 30000\n\
             timeout_ms: 30000\n\
             \n\
             The wait ended before the task finished — a timeout is not an error. Prefer continuing with other work over waiting again; completion arrives via automatic notification."
        );
    }

    #[tokio::test]
    async fn test_wait_timeout_error_renders_timeout_report() {
        let (callbacks, _, _) = scripted(
            read_ok(Value::Null),
            Err("State write error: [-32003] wait timed out after 30000ms".into()),
        );
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let result = execute_task_wait(
            &callbacks,
            None,
            None,
            &wait,
            &serde_json::json!({ "task_id": "task-1", "timeout": 30 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(!result.is_error);
        assert!(result.content.starts_with("wait_status: timed_out\n"));
        assert!(result.content.contains("a timeout is not an error"));
    }

    #[tokio::test]
    async fn test_wait_not_found_maps_to_task_not_found() {
        let (callbacks, _, _) = scripted(
            read_ok(Value::Null),
            Err("State write error: [-32002] task not found: task-9".into()),
        );
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let result = execute_task_wait(
            &callbacks,
            None,
            None,
            &wait,
            &serde_json::json!({ "task_id": "task-9", "timeout": 30 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(result.is_error);
        assert_eq!(result.content, "Task not found: task-9");
    }

    #[tokio::test]
    async fn test_wait_invalid_args_return_error_without_calling_host() {
        let (callbacks, read_received, write_received) =
            scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        for bad in [
            serde_json::json!({ "timeout": 30 }),
            serde_json::json!({ "task_id": "task-1" }),
            serde_json::json!({ "task_id": "task-1", "timeout": 0 }),
            serde_json::json!({ "task_id": "task-1", "timeout": 601 }),
            serde_json::json!({ "task_id": "task-1", "timeout": "30" }),
        ] {
            let result = execute_task_wait(&callbacks, None, None, &wait, &bad)
                .await
                .unwrap();
            assert!(result.is_error, "args: {bad}");
            assert!(result.content.contains("Invalid TaskWait arguments"));
        }
        assert!(read_received.lock().unwrap().is_none());
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_wait_unsupported_host_returns_failure_message() {
        let (callbacks, _, _) = scripted(
            read_ok(Value::Null),
            Err("State write error: [-32603] host does not support state bridge".into()),
        );
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let result = execute_task_wait(
            &callbacks,
            None,
            None,
            &wait,
            &serde_json::json!({ "task_id": "task-1", "timeout": 30 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(result.is_error);
        assert_eq!(result.content, STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE);
    }

    #[tokio::test]
    async fn test_wait_other_host_error_passes_through() {
        let (callbacks, _, _) = scripted(
            read_ok(Value::Null),
            Err("State write error: [-32603] something else broke".into()),
        );
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let result = execute_task_wait(
            &callbacks,
            None,
            None,
            &wait,
            &serde_json::json!({ "task_id": "task-1", "timeout": 30 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(result.is_error);
        assert!(result.content.contains("something else broke"));
    }

    fn held_runner() -> (
        tempfile::TempDir,
        Arc<crate::storage::TaskRunner>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(crate::storage::TaskRunner::for_workspace(dir.path()).unwrap());
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        runner
            .spawn_task("task-1".into(), "held task".into(), async move {
                let _ = held.await;
                "HELD-OUTPUT".to_string()
            })
            .unwrap();
        (dir, runner, release)
    }

    #[tokio::test]
    async fn test_wait_with_a_runner_blocks_until_the_timeout() {
        let (_dir, runner, release) = held_runner();
        let (callbacks, read_received, write_received) =
            scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let started = std::time::Instant::now();
        let result = execute_task_wait(
            &callbacks,
            Some(&runner),
            None,
            &wait,
            &serde_json::json!({ "task_id": "task-1", "timeout": 1 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(!result.is_error);
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(900),
            "the wait must suspend; elapsed {:?}",
            started.elapsed()
        );
        assert!(result.content.starts_with("wait_status: timed_out"));

        assert!(result.content.contains("[still_running]"));
        assert!(result.content.contains("task_id: task-1"));

        assert!(read_received.lock().unwrap().is_none());
        assert!(write_received.lock().unwrap().is_none());
        let _ = release.send(());
    }

    #[tokio::test]
    async fn test_wait_with_a_runner_reports_a_settled_task() {
        let (_dir, runner, release) = held_runner();
        let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let settle = runner.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = release.send(());
        });
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let result = execute_task_wait(
            &callbacks,
            Some(&runner),
            None,
            &wait,
            &serde_json::json!({ "task_id": "task-1", "timeout": 30 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(!result.is_error);
        assert_eq!(
            result.content.lines().next().unwrap(),
            "wait_status: completed"
        );
        assert!(result.content.contains("[finished]"));
        assert!(result.content.contains("HELD-OUTPUT"));
        assert_eq!(settle.entry("task-1").unwrap()["status"], "completed");
    }

    #[tokio::test]
    async fn wait_reports_a_terminal_task_without_repeating_its_notification() {
        use crate::storage::task_runner::{TaskOutcome, TaskSpawnMeta};

        let runner = Arc::new(crate::storage::TaskRunner::new(None));
        runner
            .spawn_task_with_meta(
                TaskSpawnMeta {
                    session_id: Some("session-1"),
                    kind: "bash",
                    subagent_type: None,
                    agent_id: None,
                    timeout_ms: None,
                },
                "task-1".into(),
                "build".into(),
                std::future::ready(TaskOutcome::Completed("build output".into())),
            )
            .unwrap();
        assert!(matches!(
            runner.wait("task-1", 1_000).await,
            TaskWaitResult::Completed(_)
        ));
        assert_eq!(runner.pending_notification_count(Some("session-1")), 1);
        let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let cancel = crate::subagent::types::ParentCancel::new();
        let steer = crate::subagent::types::ParentCancel::new();
        cancel.trigger();
        steer.trigger();
        let updates = std::sync::Mutex::new(Vec::new());
        let record = |update: &crate::tools::ToolUpdate<'_>| {
            updates.lock().unwrap().push(update.text.to_string());
        };
        let events = std::sync::Mutex::new(Vec::new());
        let track = |event: Value| events.lock().unwrap().push(event);
        let wait = WaitContext {
            parent_cancel: Some(&cancel),
            on_update: Some(&record),
            telemetry: Some(&track),
            ..main_wait_ctx(&tally)
        };
        let result = execute_task_wait(
            &callbacks,
            Some(&runner),
            Some(&steer),
            &wait,
            &serde_json::json!({ "task_id": "task-1", "timeout": 5 }),
        )
        .await
        .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("build output"));
        assert_eq!(runner.pending_notification_count(Some("session-1")), 0);
        assert!(updates.lock().unwrap().is_empty());
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["outcome"], "completed");
        assert_eq!(events[0]["extra_completed_count"], 0);
    }

    #[tokio::test]
    async fn wait_delivers_only_the_tasks_running_when_it_started() {
        use crate::storage::task_runner::{TaskOutcome, TaskSpawnMeta};

        let runner = Arc::new(crate::storage::TaskRunner::new(None));
        let mut releases = Vec::new();
        for id in ["task-1", "task-2"] {
            let (release, released) = tokio::sync::oneshot::channel();
            runner
                .spawn_task_with_meta(
                    TaskSpawnMeta {
                        session_id: Some("session-1"),
                        kind: "bash",
                        ..Default::default()
                    },
                    id.into(),
                    id.into(),
                    async move {
                        released.await.unwrap();
                        TaskOutcome::Completed(id.into())
                    },
                )
                .unwrap();
            releases.push(release);
        }
        let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let events = std::sync::Mutex::new(Vec::new());
        let track = |event: Value| events.lock().unwrap().push(event);
        let wait = WaitContext {
            telemetry: Some(&track),
            ..main_wait_ctx(&tally)
        };
        let args = serde_json::json!({ "timeout": 30 });
        let mut pending = Box::pin(execute_task_wait(
            &callbacks,
            Some(&runner),
            None,
            &wait,
            &args,
        ));
        assert!(futures_util::poll!(&mut pending).is_pending());
        runner
            .spawn_task_with_meta(
                TaskSpawnMeta {
                    session_id: Some("session-1"),
                    kind: "bash",
                    ..Default::default()
                },
                "task-later".into(),
                "started after wait".into(),
                std::future::ready(TaskOutcome::Completed("later output".into())),
            )
            .unwrap();
        for release in releases {
            release.send(()).unwrap();
        }
        for id in ["task-1", "task-2", "task-later"] {
            assert!(matches!(
                runner.wait(id, 1_000).await,
                TaskWaitResult::Completed(_)
            ));
        }
        let result = pending.await.unwrap();
        assert!(result.content.contains("[completed_during_wait]"));
        assert!(result.content.contains("task_id: task-1"));
        assert!(result.content.contains("task_id: task-2"));
        assert!(!result.content.contains("task-later"));
        let remaining = runner.take_pending_notifications(Some("session-1"));
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].task_id, "task-later");
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["outcome"], "completed");
        assert_eq!(events[0]["extra_completed_count"], 1);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_emits_progress_each_second_and_stops_on_completion() {
        let (_dir, runner, release) = held_runner();
        let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let updates = std::sync::Mutex::new(Vec::new());
        let record = |update: &crate::tools::ToolUpdate<'_>| {
            updates.lock().unwrap().push(update.text.to_string());
        };
        let wait = WaitContext {
            on_update: Some(&record),
            ..main_wait_ctx(&tally)
        };
        let args = serde_json::json!({ "task_id": "task-1", "timeout": 30 });
        let mut pending = Box::pin(execute_task_wait(
            &callbacks,
            Some(&runner),
            None,
            &wait,
            &args,
        ));
        assert!(futures_util::poll!(&mut pending).is_pending());
        assert!(updates.lock().unwrap().is_empty());
        for elapsed in [1, 2] {
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
            assert!(futures_util::poll!(&mut pending).is_pending());
            assert_eq!(
                updates.lock().unwrap().last().unwrap(),
                &format!("Waiting {elapsed}s / 30s · 1 background task still running"),
            );
        }
        release.send(()).unwrap();
        assert!(!pending.await.unwrap().is_error);
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        assert_eq!(updates.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn turn_cancellation_takes_precedence_over_steering_and_leaves_the_task_running() {
        use crate::_base::utils::abort::is_user_cancellation;
        use crate::subagent::types::ParentCancel;

        for args in [
            serde_json::json!({ "task_id": "task-1", "timeout": 30 }),
            serde_json::json!({ "timeout": 30 }),
        ] {
            let (_dir, runner, release) = held_runner();
            let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
            let cancel = ParentCancel::new();
            let steer = ParentCancel::new();
            let tally = test_tally();
            let events = std::sync::Mutex::new(Vec::new());
            let track = |event: Value| events.lock().unwrap().push(event);
            let wait = WaitContext {
                parent_cancel: Some(&cancel),
                telemetry: Some(&track),
                ..main_wait_ctx(&tally)
            };
            let mut pending = Box::pin(execute_task_wait(
                &callbacks,
                Some(&runner),
                Some(&steer),
                &wait,
                &args,
            ));
            assert!(futures_util::poll!(&mut pending).is_pending());
            steer.trigger();
            cancel.trigger();
            let error = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
                .await
                .expect("cancellation must end the wait")
                .unwrap_err();
            assert!(is_user_cancellation(error.as_ref()));
            let events = events.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["outcome"], "aborted");
            assert_eq!(events[0]["has_task_id"], args.get("task_id").is_some());
            drop(events);
            assert_eq!(runner.entry("task-1").unwrap()["status"], "running");
            release.send(()).unwrap();
            assert!(matches!(
                runner.wait("task-1", 1_000).await,
                TaskWaitResult::Completed(_)
            ));
        }
    }

    #[tokio::test]
    async fn test_wait_with_a_runner_ends_on_the_steer_signal() {
        let (_dir, runner, release) = held_runner();
        let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let steer = crate::subagent::types::ParentCancel::new();
        let trigger = steer.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            trigger.trigger();
        });
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let started = std::time::Instant::now();
        let result = execute_task_wait(
            &callbacks,
            Some(&runner),
            Some(&steer),
            &wait,
            &serde_json::json!({ "task_id": "task-1", "timeout": 30 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(!result.is_error, "an interruption is not an error");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the steer must end the wait, not the timeout"
        );
        let lines: Vec<&str> = result.content.lines().collect();
        assert_eq!(lines[0], "wait_status: interrupted");
        assert_eq!(lines[1], "reason: steer");
        assert!(result.content.contains("New input ended this wait early"));
        assert_eq!(
            runner.entry("task-1").unwrap()["status"],
            "running",
            "steering must not stop the task"
        );
        let _ = release.send(());
    }

    #[tokio::test]
    async fn test_wait_without_a_task_id_waits_for_any() {
        let (_dir, runner, release) = held_runner();
        let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let result = execute_task_wait(
            &callbacks,
            Some(&runner),
            None,
            &wait,
            &serde_json::json!({ "timeout": 5 }),
        )
        .await
        .expect("the wait must not be aborted");

        assert!(!result.is_error);
        assert!(result.content.starts_with("wait_status: timed_out"));

        assert!(!result.content.contains("\ntask_id: task-1\nwaited_ms"));
        assert!(result.content.contains("[still_running]"));
        let _ = release.send(());
    }

    #[tokio::test]
    async fn test_wait_any_with_no_running_tasks_reports_no_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(crate::storage::TaskRunner::for_workspace(dir.path()).unwrap());
        let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let result = execute_task_wait(
            &callbacks,
            Some(&runner),
            None,
            &wait,
            &serde_json::json!({ "timeout": 5 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(!result.is_error);
        assert!(result.content.starts_with("wait_status: no_tasks\n"));
        assert!(result.content.contains("nothing to wait for"));
    }

    #[tokio::test]
    async fn test_wait_with_a_runner_surfaces_the_bridge_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(crate::storage::TaskRunner::for_workspace(dir.path()).unwrap());
        let (callbacks, _, write_received) = scripted(
            read_ok(Value::Null),
            Err("State write error: [-32002] task not found: nope".into()),
        );
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let result = execute_task_wait(
            &callbacks,
            Some(&runner),
            None,
            &wait,
            &serde_json::json!({ "task_id": "nope", "timeout": 5 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(result.is_error);
        assert_eq!(result.content, "Task not found: nope");
        assert_eq!(
            write_received.lock().unwrap().as_ref().unwrap().value["action"],
            "wait"
        );
    }

    #[tokio::test]
    async fn test_wait_falls_back_to_the_bridge_for_an_unknown_task() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(crate::storage::TaskRunner::for_workspace(dir.path()).unwrap());
        let (callbacks, _, write_received) =
            scripted(read_ok(Value::Null), write_ok(sample_snapshot()));
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let result = execute_task_wait(
            &callbacks,
            Some(&runner),
            None,
            &wait,
            &serde_json::json!({ "task_id": "task-1", "timeout": 30 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(!result.is_error);
        assert_eq!(
            result.content.lines().next().unwrap(),
            "wait_status: completed"
        );
        assert_eq!(
            write_received.lock().unwrap().as_ref().unwrap().value["action"],
            "wait"
        );
    }

    #[tokio::test]
    async fn test_wait_any_without_a_runner_requires_a_task_id() {
        let (callbacks, _, write_received) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let result = execute_task_wait(
            &callbacks,
            None,
            None,
            &wait,
            &serde_json::json!({ "timeout": 5 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(result.is_error);
        assert!(result.content.contains("`task_id` is required"));
        assert!(write_received.lock().unwrap().is_none());
    }

    fn empty_runner() -> (tempfile::TempDir, Arc<crate::storage::TaskRunner>) {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(crate::storage::TaskRunner::for_workspace(dir.path()).unwrap());
        (dir, runner)
    }

    #[tokio::test]
    async fn the_second_wait_in_a_turn_appends_the_repeat_warning() {
        let (_dir, runner) = empty_runner();
        let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let args = serde_json::json!({ "timeout": 5 });
        let first = execute_task_wait(&callbacks, Some(&runner), None, &wait, &args)
            .await
            .unwrap();
        assert!(
            !first.content.contains("[wait_warning]"),
            "first call: {first:?}"
        );
        let second = execute_task_wait(&callbacks, Some(&runner), None, &wait, &args)
            .await
            .unwrap();
        assert!(
            second.content.contains("[wait_warning]"),
            "second call: {second:?}"
        );
        assert!(second.content.contains(
            "This is WaitFor call 2 in this turn, and you have already spent 0s waiting."
        ));

        assert!(
            second
                .content
                .contains("Stop calling WaitFor. Repeated waiting wastes the user's time")
        );
    }

    #[tokio::test]
    async fn the_repeat_warning_uses_the_subagent_advice_for_a_subagent_caller() {
        let (_dir, runner) = empty_runner();
        let (callbacks, read_received, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let wait = WaitContext {
            tally: &tally,
            turn_id: 1,
            caller_agent_id: "subagent-7",
            display_name: "WaitFor",
            parent_cancel: None,
            on_update: None,
            telemetry: None,
        };
        let args = serde_json::json!({ "timeout": 5 });
        let _ = execute_task_wait(&callbacks, Some(&runner), None, &wait, &args)
            .await
            .unwrap();
        let second = execute_task_wait(&callbacks, Some(&runner), None, &wait, &args)
            .await
            .unwrap();
        assert!(
            second
                .content
                .contains("repeated waits burn time your caller is waiting on"),
            "subagent advice: {second:?}"
        );

        assert!(read_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn the_repeat_warning_uses_the_goal_advice_when_a_goal_is_active() {
        let (_dir, runner) = empty_runner();
        let (callbacks, read_received, _) = scripted(
            read_ok(serde_json::json!({ "goal": { "status": "active" } })),
            write_ok(Value::Null),
        );
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let args = serde_json::json!({ "timeout": 5 });
        let _ = execute_task_wait(&callbacks, Some(&runner), None, &wait, &args)
            .await
            .unwrap();
        let second = execute_task_wait(&callbacks, Some(&runner), None, &wait, &args)
            .await
            .unwrap();
        assert!(
            second.content.contains("repeated waits stall the goal"),
            "goal advice: {second:?}"
        );
        assert!(
            second
                .content
                .contains("WaitFor beats polling with Bash sleep"),
            "goal advice tail: {second:?}"
        );

        assert_eq!(
            read_received.lock().unwrap().as_ref().unwrap().domain,
            "goal"
        );
    }

    #[tokio::test]
    async fn the_tally_resets_when_the_turn_changes() {
        let (_dir, runner) = empty_runner();
        let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let args = serde_json::json!({ "timeout": 5 });
        let turn_one = WaitContext {
            tally: &tally,
            turn_id: 1,
            caller_agent_id: "main",
            display_name: "WaitFor",
            parent_cancel: None,
            on_update: None,
            telemetry: None,
        };
        let _ = execute_task_wait(&callbacks, Some(&runner), None, &turn_one, &args)
            .await
            .unwrap();

        let turn_two = WaitContext {
            tally: &tally,
            turn_id: 2,
            caller_agent_id: "main",
            display_name: "WaitFor",
            parent_cancel: None,
            on_update: None,
            telemetry: None,
        };
        let fresh = execute_task_wait(&callbacks, Some(&runner), None, &turn_two, &args)
            .await
            .unwrap();
        assert!(
            !fresh.content.contains("[wait_warning]"),
            "a turn change must reset the tally: {fresh:?}"
        );
        let repeated = execute_task_wait(&callbacks, Some(&runner), None, &turn_two, &args)
            .await
            .unwrap();
        assert!(
            repeated
                .content
                .contains("This is WaitFor call 2 in this turn"),
            "the new turn counts from one: {repeated:?}"
        );
    }

    #[tokio::test]
    async fn the_repeat_warning_names_the_repl_alias() {
        let (_dir, runner) = empty_runner();
        let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let wait = WaitContext {
            tally: &tally,
            turn_id: 1,
            caller_agent_id: "main",
            display_name: "TaskWait",
            parent_cancel: None,
            on_update: None,
            telemetry: None,
        };
        let args = serde_json::json!({ "timeout": 5 });
        let _ = execute_task_wait(&callbacks, Some(&runner), None, &wait, &args)
            .await
            .unwrap();
        let second = execute_task_wait(&callbacks, Some(&runner), None, &wait, &args)
            .await
            .unwrap();
        assert!(
            second
                .content
                .contains("This is TaskWait call 2 in this turn"),
            "the REPL alias is named, not the product name: {second:?}"
        );
        assert!(second.content.contains("Stop calling TaskWait."));
    }

    #[tokio::test]
    async fn a_subagent_timeout_report_uses_the_handoff_wording() {
        let (_dir, runner, release) = held_runner();
        let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(Value::Null));
        let tally = test_tally();
        let wait = WaitContext {
            tally: &tally,
            turn_id: 1,
            caller_agent_id: "subagent-7",
            display_name: "WaitFor",
            parent_cancel: None,
            on_update: None,
            telemetry: None,
        };
        let result = execute_task_wait(
            &callbacks,
            Some(&runner),
            None,
            &wait,
            &serde_json::json!({ "task_id": "task-1", "timeout": 1 }),
        )
        .await
        .expect("the wait must not be aborted");
        assert!(result.content.starts_with("wait_status: timed_out"));
        assert!(
            result.content.contains(
                "If you need its result, call WaitFor again: ending your turn is your hand-off, and a completion notification after it reaches no one."
            ),
            "subagent timeout body: {result:?}"
        );
        let _ = release.send(());
    }

    #[test]
    fn wait_progress_formats_elapsed_milliseconds_as_elapsed_seconds() {
        let tally = test_tally();
        let wait = main_wait_ctx(&tally);
        let updates = std::sync::Mutex::new(Vec::new());
        let record = |update: &crate::tools::ToolUpdate<'_>| {
            updates.lock().unwrap().push(update.text.to_string());
        };
        let wait = WaitContext {
            on_update: Some(&record),
            ..wait
        };

        wait.emit_progress(30, 5_000, 2);

        assert!(
            updates.lock().unwrap()[0].starts_with("Waiting 5s / 30s · 2 background tasks"),
            "progress text must be human-scale: {updates:?}"
        );
    }

    #[test]
    fn test_format_wait_seconds_matches_v2() {
        assert_eq!(format_wait_seconds(45_000), "45s");
        assert_eq!(format_wait_seconds(1_499), "1s");
        assert_eq!(format_wait_seconds(1_500), "2s");
        assert_eq!(format_wait_seconds(60_000), "1m");
        assert_eq!(format_wait_seconds(90_000), "1m 30s");
        assert_eq!(format_wait_seconds(3_600_000), "1h");
        assert_eq!(format_wait_seconds(3_660_000), "1h 01m");
        assert_eq!(format_wait_seconds(0), "0s");
    }

    #[test]
    fn wait_for_flag_defaults_on_and_honors_environment_then_config() {
        assert!(wait_for_flag(None, None));
        assert!(!wait_for_flag(Some(false), Some(true)));
        assert!(wait_for_flag(Some(true), Some(false)));
        assert!(!wait_for_flag(None, Some(false)));
        assert!(wait_for_flag(None, Some(true)));
    }

    #[test]
    fn test_task_wait_tool_def_matches_v2_schema() {
        let def = task_wait_tool_def();
        assert_eq!(def.name, "TaskWait");

        assert_eq!(def.input_schema["required"][0], "timeout");
        assert_eq!(def.input_schema["required"].as_array().unwrap().len(), 1);
        assert!(def.input_schema["properties"]["task_id"].is_object());
        assert_eq!(def.input_schema["properties"]["timeout"]["maximum"], 90);
        assert!(def.description.contains("Wait for background tasks"));
    }

    #[test]
    fn task_wait_description_carries_the_ported_lines_and_omits_the_unimplemented_one() {
        let description = &wait_for_tool_def().description;

        for (what, line) in [
            ("kept waiting clause", "but the user is kept waiting too"),
            (
                "notify paragraph",
                "you do not need to busily wait for them",
            ),
            (
                "meanwhile guideline",
                "think about what else you can do meanwhile",
            ),
            (
                "timeout retightened",
                "repeated waits keep the user waiting",
            ),
        ] {
            assert!(description.contains(line), "missing {what}: {description}");
        }

        assert!(
            !description.contains("other tasks that finished during the wait window"),
            "describes an unimplemented result section: {description}"
        );

        assert_eq!(
            description.replace("WaitFor", "TaskWait"),
            task_wait_tool_def().description
        );
    }
}
