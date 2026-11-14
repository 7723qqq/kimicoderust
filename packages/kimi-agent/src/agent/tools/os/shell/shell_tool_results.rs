use std::sync::Arc;

use serde_json::Value;

use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::utils::abort::{AbortSignal, user_cancellation_reason};
use crate::os::interface::host_process::IHostProcess;
use crate::rpc::types::BoxFuture;
use crate::tool::output_accumulator::{ToolOutputAccumulator, ToolOutputAccumulatorResult};
use crate::tool::output_pointer::{OutputUnit, output_next_step, output_size_line};
use crate::tool::tool_contract::DEFAULT_TOOL_RESULT_MAX_CHARS;
use crate::turn_loop::types::ExecutableToolResult;

pub use crate::tools::shell_results::{
    BackgroundStart, format_timeout_label, next_step_lines, validate_run_request,
};

pub const MS_PER_SECOND: u64 = 1000;
pub const DEFAULT_TIMEOUT_S: u64 = 60;
pub const MAX_TIMEOUT_S: u64 = 5 * 60;
pub const DEFAULT_BACKGROUND_TIMEOUT_S: u64 = 10 * 60;
pub const MAX_BACKGROUND_TIMEOUT_S: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundStartScenario {
    BackgroundStarted,
    ForegroundDetached,
    ForegroundDetachedByUser,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellToolCommandInput {
    pub command: String,
    pub run_in_background: Option<bool>,
    pub description: Option<String>,
}

pub fn timeout_cap_s(is_background: bool) -> u64 {
    if is_background {
        MAX_BACKGROUND_TIMEOUT_S
    } else {
        MAX_TIMEOUT_S
    }
}

pub fn normalize_timeout_ms(timeout: Option<u64>, is_background: bool) -> u64 {
    let default_seconds = if is_background {
        DEFAULT_BACKGROUND_TIMEOUT_S
    } else {
        DEFAULT_TIMEOUT_S
    };
    timeout
        .unwrap_or(default_seconds)
        .min(timeout_cap_s(is_background))
        * MS_PER_SECOND
}

pub fn foreground_description(label: &str, command: &str, description: Option<&str>) -> String {
    if let Some(explicit) = description.map(str::trim).filter(|text| !text.is_empty()) {
        return explicit.to_string();
    }
    if command.encode_utf16().count() <= 60 {
        return format!("{label}: {command}");
    }
    let mut units = 0;
    let mut end = 0;
    for (index, character) in command.char_indices() {
        let next = units + character.len_utf16();
        if next > 60 {
            break;
        }
        units = next;
        end = index + character.len_utf8();
    }
    format!("{label}: {}…", &command[..end])
}

pub async fn dispose_process(proc: Arc<dyn IHostProcess>) {
    proc.dispose().await;
}

pub async fn kill_spawned_process(proc: Arc<dyn IHostProcess>) {
    if let Err(error) = proc.kill("SIGTERM").await {
        on_unexpected_error(error.to_string());
    }
    dispose_process(proc).await;
}

pub trait ShellTaskService: Send + Sync {
    fn get_task(&self, task_id: &str) -> Option<Value>;
    fn get_output_snapshot(&self, task_id: &str, offset: u64) -> BoxFuture<'static, Value>;
}

pub struct ShellResultLabels {
    pub title: String,
    pub brief: String,
}

pub struct ShellToolResults {
    tasks: Arc<dyn ShellTaskService>,
    allow_background: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl ShellToolResults {
    pub fn new(
        tasks: Arc<dyn ShellTaskService>,
        allow_background: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            tasks,
            allow_background,
        }
    }

    pub fn validate_run_request(
        &self,
        args: &ShellToolCommandInput,
        signal: &AbortSignal,
    ) -> Option<ExecutableToolResult> {
        if signal.aborted() {
            return Some(tool_result("Aborted before command started", true));
        }
        if args.command.is_empty() {
            return Some(tool_result("Command cannot be empty.", true));
        }
        if args.run_in_background != Some(true) {
            return None;
        }
        if !(self.allow_background)() {
            return Some(tool_result(
                "Background execution is not available for this agent because TaskOutput and TaskStop are not enabled.",
                true,
            ));
        }
        if args
            .description
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            return Some(tool_result(
                "description is required when run_in_background is true.",
                true,
            ));
        }
        None
    }

    pub async fn foreground_completion_result(
        &self,
        task_id: &str,
        proc: Arc<dyn IHostProcess>,
        builder: &mut ToolOutputAccumulator,
        foreground_timeout_ms: u64,
    ) -> ExecutableToolResult {
        let current = self.tasks.get_task(task_id);
        let (exit_code, missing_exit_code) = match current
            .as_ref()
            .filter(|task| task.get("kind").and_then(Value::as_str) == Some("process"))
        {
            Some(task) => match task.get("exitCode") {
                Some(Value::Null) => (None, "null"),
                Some(value) => (value.as_i64().map(|code| code as i32), "undefined"),
                None => (None, "undefined"),
            },
            None => (proc.exit_code(), "null"),
        };
        let status = current
            .as_ref()
            .and_then(|task| task.get("status"))
            .and_then(Value::as_str);
        let stop_reason = current
            .as_ref()
            .and_then(|task| task.get("stopReason"))
            .and_then(Value::as_str);
        let result = if status == Some("timed_out") {
            let label = format_timeout_label(foreground_timeout_ms);
            builder.error(
                &format!("Command killed by timeout ({label})"),
                Some(&format!("Killed by timeout ({label})")),
            )
        } else if status == Some("killed")
            && stop_reason == Some(user_cancellation_reason().to_string().as_str())
        {
            builder.error("Interrupted by user", Some("Interrupted by user"))
        } else if matches!(status, Some("failed") | Some("killed")) && stop_reason.is_some() {
            let reason = stop_reason.unwrap_or_default();
            builder.error(reason, Some(reason))
        } else if exit_code == Some(0) {
            builder.ok("Command executed successfully.", None)
        } else {
            if builder.n_chars() == 0 {
                builder.write(&format!(
                    "Process exited with code {}",
                    exit_code_text(exit_code, missing_exit_code)
                ));
            }
            builder.error(
                &format!("Command failed with exit code: {}.", exit_code_text(exit_code, missing_exit_code)),
                Some(&format!("Failed with exit code: {}", exit_code_text(exit_code, missing_exit_code))),
            )
        };
        self.add_foreground_output_reference(task_id, result, builder.total_chars())
            .await
    }

    pub async fn add_foreground_output_reference(
        &self,
        task_id: &str,
        result: ToolOutputAccumulatorResult,
        total_chars: usize,
    ) -> ExecutableToolResult {
        if total_chars <= DEFAULT_TOOL_RESULT_MAX_CHARS {
            return from_accumulator(result);
        }
        let output = self.tasks.get_output_snapshot(task_id, 0).await;
        if output
            .get("fullOutputAvailable")
            .and_then(Value::as_bool)
            != Some(true)
        {
            return from_accumulator(result);
        }
        let Some(output_path) = output
            .get("outputPath")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return from_accumulator(result);
        };
        let task_output_hint = if (self.allow_background)() {
            format!("\n{}", output_next_step("task-output", Some(task_id)))
        } else {
            String::new()
        };
        let output_size = output
            .get("outputSizeBytes")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let task_info = format!(
            "task_id: {task_id}\n{}{task_output_hint}",
            output_size_line(OutputUnit::Bytes, output_size, None)
        );
        let mut converted = from_accumulator(result);
        let existing_suffix = converted.spill.as_ref().and_then(|spill| spill.suffix.clone());
        converted.spill = Some(crate::turn_loop::types::ToolResultSpill {
            output_path: Some(output_path),
            total_chars: Some(total_chars),
            suffix: Some(match existing_suffix {
                Some(suffix) => format!("{suffix}\n{task_info}"),
                None => task_info,
            }),
        });
        converted
    }

    pub fn background_started_result(
        &self,
        task_id: &str,
        proc: Arc<dyn IHostProcess>,
        description: &str,
        labels: &ShellResultLabels,
        builder: Option<&ToolOutputAccumulator>,
        scenario: BackgroundStartScenario,
    ) -> ExecutableToolResult {
        let current = self.tasks.get_task(task_id);
        let status = current
            .as_ref()
            .and_then(|task| task.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("running");
        let output_path = current
            .as_ref()
            .and_then(|task| task.get("outputPath"))
            .and_then(Value::as_str);
        let detached_by_user = if scenario == BackgroundStartScenario::ForegroundDetachedByUser {
            "detached_by_user: true\n"
        } else {
            ""
        };
        let output_path = output_path
            .map(|path| format!("output_path: {path}\n"))
            .unwrap_or_default();
        let metadata = format!(
            "task_id: {task_id}\npid: {}\ndescription: {description}\nstatus: {status}\n{output_path}{detached_by_user}automatic_notification: true\n{}human_shell_hint: The task is visible in the background-task panel.",
            proc.pid(),
            self.next_step_lines(scenario)
        );
        let default_builder;
        let builder = match builder {
            Some(builder) => builder,
            None => {
                default_builder = ToolOutputAccumulator::new();
                &default_builder
            }
        };
        let foreground_output = builder.ok("", None).content;
        let content = if foreground_output.is_empty() {
            metadata
        } else {
            format!("{metadata}\n\nforeground_output:\n{foreground_output}")
        };
        ExecutableToolResult {
            stop_turn: false,
            content,
            is_error: false,
            note: Some(labels.brief.clone()),
            delivery: None,
            parts: Vec::new(),
            display: None,
            spill: None,
        }
    }

    pub fn next_step_lines(&self, scenario: BackgroundStartScenario) -> String {
        if matches!(
            scenario,
            BackgroundStartScenario::ForegroundDetached
                | BackgroundStartScenario::ForegroundDetachedByUser
        ) {
            let avoid = if (self.allow_background)() {
                "do NOT wait, poll, or call TaskOutput on it"
            } else {
                "do NOT wait or poll"
            };
            let moved = if scenario == BackgroundStartScenario::ForegroundDetachedByUser {
                "The user moved this task to the background."
            } else {
                "The task now runs in the background."
            };
            return format!(
                "next_step: {moved} You will be automatically notified when it completes — {avoid}; continue with your current work.\n"
            );
        }
        if !(self.allow_background)() {
            return "next_step: You will be automatically notified when it completes.\n".to_string();
        }
        concat!(
            "next_step: The completion arrives automatically in a later turn — do NOT wait, poll, ",
            "or call TaskOutput on it; continue with your current work.\n",
            "next_step: Use TaskStop only if the task must be cancelled.\n"
        )
        .to_string()
    }
}

fn exit_code_text(exit_code: Option<i32>, missing: &str) -> String {
    exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| missing.to_string())
}

fn tool_result(content: impl Into<String>, is_error: bool) -> ExecutableToolResult {
    ExecutableToolResult {
        stop_turn: false,
        content: content.into(),
        is_error,
        note: None,
        delivery: None,
        parts: Vec::new(),
        display: None,
        spill: None,
    }
}

fn from_accumulator(result: ToolOutputAccumulatorResult) -> ExecutableToolResult {
    ExecutableToolResult {
        stop_turn: false,
        content: result.content,
        is_error: result.is_error,
        note: result.brief,
        delivery: None,
        parts: Vec::new(),
        display: None,
        spill: result.spill,
    }
}
