use std::sync::Arc;

use super::defs::{
    DEFAULT_SUBAGENT_TIMEOUT_MS, SUBAGENT_STOPPED_MESSAGE, TOWER_WORKER_PROFILE,
    USER_INTERRUPTED_SUBAGENT_MESSAGE, background_agent_unavailable, format_timeout_description,
    required_string_arg,
};
use super::events::{
    emit_cancelled, emit_completed, emit_failed, emit_spawned_started, emit_subagent_event,
};
use super::execute::background_agent_available;
use super::format::{format_failure, report_foreground_turn, stopped_task_output};
use crate::storage::TaskOutcome;
use crate::subagent::SubagentManager;
use crate::subagent::manager::ForegroundTurnOutcome;
use crate::subagent::types::ParentCancel;
use crate::turn_loop::types::{ExecutableToolResult, LoopTurnStopReason};

pub const RESUME_WITH_TYPE_UNAVAILABLE: &str =
    "Cannot set subagent_type when resuming an existing agent. Resume by agent id only.";

pub(super) async fn tower_resume_denial(
    cwd: Option<std::path::PathBuf>,
    profile_name: &str,
    resume_id: &str,
    run_in_background: Option<bool>,
    is_background_available: bool,
) -> Option<String> {
    if run_in_background == Some(true) {
        return None;
    }
    if !is_background_available {
        return None;
    }
    let roster_hit = match crate::tools::tower::tower_resume_target(cwd, resume_id).await {
        Ok(hit) => hit,

        Err(_) => return None,
    };
    if profile_name != TOWER_WORKER_PROFILE && roster_hit.is_none() {
        return None;
    }
    let entry_name = roster_hit
        .map(|entry| entry.name)
        .unwrap_or_else(|| resume_id.to_string());
    Some(format!(
        "Resuming tower agent \"{entry_name}\" in the foreground would freeze the tower until \
         it finishes — pass run_in_background=true instead: \
         Agent(resume=\"{resume_id}\", run_in_background=true, prompt=\"...\"); \
         its completion (and any inbox traffic) will wake you."
    ))
}

pub(super) async fn execute_resume(
    manager: &Arc<SubagentManager>,
    root: Option<&std::path::Path>,
    args: &serde_json::Value,
    resume_id: &str,
    timeout_ms: Option<u64>,
    parent_cancel: Option<&ParentCancel>,
    tool_call_id: Option<&str>,
) -> Option<ExecutableToolResult> {
    if args
        .get("subagent_type")
        .and_then(|value| value.as_str())
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: RESUME_WITH_TYPE_UNAVAILABLE.to_string(),
            is_error: true,
            note: None,
            display: None,
        });
    }
    let profile_name = manager.resume_profile(resume_id).await?;

    let caller_agent_id = crate::tools::CALLER_AGENT_ID
        .try_with(|id| id.clone())
        .unwrap_or_else(|_| crate::callbacks::MAIN_AGENT_ID.to_string());
    if let Err(error) =
        crate::swarm::service::require_owned_subagent(manager, &caller_agent_id, resume_id).await
    {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: error,
            is_error: true,
            note: None,
            display: None,
        });
    }
    if let Err(error) = crate::swarm::service::require_idle_subagent(manager, resume_id).await {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: error,
            is_error: true,
            note: None,
            display: None,
        });
    }
    let runtime = manager.runtime().await?;
    let run_in_background = args.get("run_in_background").and_then(|v| v.as_bool());
    let background_available =
        background_agent_available(manager, &runtime, &caller_agent_id).await;
    if run_in_background == Some(true) && !background_available {
        return Some(background_agent_unavailable());
    }
    let prompt = match required_string_arg(args, "prompt") {
        Ok(prompt) => prompt,
        Err(error) => return Some(error),
    };

    let task_runner = manager.get_task_runner().await;
    if let Some(reason) = tower_resume_denial(
        root.map(std::path::Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok()),
        &profile_name,
        resume_id,
        run_in_background,
        background_available,
    )
    .await
    {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: reason,
            is_error: true,
            note: None,
            display: None,
        });
    }

    if run_in_background == Some(true) {
        return Some(
            run_resume_in_background(
                manager,
                runtime.callbacks.clone(),
                task_runner,
                resume_id,
                &profile_name,
                &prompt,
                tool_call_id,
            )
            .await,
        );
    }

    emit_spawned_started(
        runtime.callbacks.as_ref(),
        resume_id,
        &profile_name,
        tool_call_id,
        None,
        false,
        None,
        &prompt,
    );

    let bound_ms = match timeout_ms {
        Some(0) => None,
        Some(ms) => Some(ms),
        None => Some(DEFAULT_SUBAGENT_TIMEOUT_MS),
    };
    let turn = manager.resume_foreground_turn(resume_id, &prompt, parent_cancel);

    let run: Result<_, u64> = match bound_ms {
        Some(ms) => tokio::time::timeout(std::time::Duration::from_millis(ms), turn)
            .await
            .map_err(|_| ms),
        None => Ok(turn.await),
    };

    let run = match run {
        Ok(None) => Ok(Some(Err("resume state was lost".to_string()))),
        other => other,
    };

    let (content, is_error) = match run {
        Ok(Some(Ok(ForegroundTurnOutcome::Completed(turn)))) => report_foreground_turn(
            runtime.callbacks.as_ref(),
            resume_id,
            &profile_name,
            &turn,
            parent_cancel,
        ),
        Ok(Some(Ok(ForegroundTurnOutcome::ParentCancelled))) => {
            emit_cancelled(runtime.callbacks.as_ref(), resume_id, &profile_name);
            (
                format_failure(
                    resume_id,
                    &profile_name,
                    USER_INTERRUPTED_SUBAGENT_MESSAGE,
                    "cancelled",
                    None,
                ),
                true,
            )
        }
        Ok(Some(Err(message))) => {
            emit_subagent_event(
                runtime.callbacks.as_ref(),
                serde_json::json!({
                    "type": "subagent.failed",
                    "subagent_id": resume_id,
                    "error": message,
                }),
            );
            (
                format_failure(
                    resume_id,
                    &profile_name,
                    &message,
                    super::format::run_failure_reason(&message),
                    None,
                ),
                true,
            )
        }
        Ok(None) => {
            let message = "resume state was lost".to_string();
            emit_subagent_event(
                runtime.callbacks.as_ref(),
                serde_json::json!({
                    "type": "subagent.failed",
                    "subagent_id": resume_id,
                    "error": message,
                }),
            );
            (
                format_failure(resume_id, &profile_name, &message, "error", None),
                true,
            )
        }
        Err(timeout_ms) => {
            let _ = manager.kill(resume_id).await;
            let message = format!(
                "Agent timed out after {}.",
                format_timeout_description(timeout_ms)
            );
            emit_subagent_event(
                runtime.callbacks.as_ref(),
                serde_json::json!({
                    "type": "subagent.failed",
                    "subagent_id": resume_id,
                    "error": message,
                }),
            );
            (
                format_failure(resume_id, &profile_name, &message, "timed_out", None),
                true,
            )
        }
    };
    Some(ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content,
        is_error,
        note: None,
        display: None,
    })
}

pub(super) async fn run_resume_in_background(
    manager: &Arc<SubagentManager>,
    callbacks: Arc<dyn crate::callbacks::HostCallbacks>,
    task_runner: Option<Arc<crate::storage::TaskRunner>>,
    resume_id: &str,
    profile_name: &str,
    prompt: &str,
    tool_call_id: Option<&str>,
) -> ExecutableToolResult {
    emit_spawned_started(
        callbacks.as_ref(),
        resume_id,
        profile_name,
        tool_call_id,
        None,
        true,
        None,
        prompt,
    );
    let mgr = manager.clone();
    let cb = callbacks.clone();
    let agent = resume_id.to_string();
    let bg_prompt = prompt.to_string();
    let profile = profile_name.to_string();
    let bg_profile = profile.clone();
    let bg_future = async move {
        let outcome = mgr.resume_foreground_turn(&agent, &bg_prompt, None).await;
        match outcome {
            Some(Ok(ForegroundTurnOutcome::Completed(turn))) => {
                if matches!(turn.stop_reason, LoopTurnStopReason::Aborted) {
                    emit_failed(cb.as_ref(), &agent, &bg_profile, SUBAGENT_STOPPED_MESSAGE);
                    TaskOutcome::Failed(stopped_task_output(SUBAGENT_STOPPED_MESSAGE, &turn))
                } else {
                    let summary = crate::subagent::manager::final_assistant_summary(&turn.messages);
                    emit_completed(cb.as_ref(), &agent, &bg_profile, &summary, &turn.usage);
                    TaskOutcome::Completed(summary)
                }
            }
            Some(Ok(ForegroundTurnOutcome::ParentCancelled)) => {
                emit_cancelled(cb.as_ref(), &agent, &bg_profile);

                TaskOutcome::Failed(USER_INTERRUPTED_SUBAGENT_MESSAGE.to_string())
            }
            Some(Err(message)) => {
                emit_failed(cb.as_ref(), &agent, &bg_profile, &message);
                TaskOutcome::Failed(format!("Error: {message}"))
            }
            None => {
                let message = "resume state was lost".to_string();
                emit_failed(cb.as_ref(), &agent, &bg_profile, &message);
                TaskOutcome::Failed(message)
            }
        }
    };

    if let Some(runner) = task_runner {
        let session_id = manager
            .runtime()
            .await
            .and_then(|r| r.session_id.clone())
            .filter(|session| !session.is_empty());

        let task_id = format!("task_{}", fastrand::u64(..));
        let registered = runner.spawn_task_with_meta(
            crate::storage::TaskSpawnMeta {
                session_id: session_id.as_deref(),
                kind: "subagent",
                subagent_type: Some(&profile),
                agent_id: Some(resume_id),
                timeout_ms: None,
            },
            task_id.clone(),
            format!("Resume {profile}: {prompt}"),
            bg_future,
        );
        if let Err(error) = registered {
            emit_failed(
                callbacks.as_ref(),
                resume_id,
                profile_name,
                &format!("failed to register the background resume task: {error}"),
            );
            return ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: format!("failed to register the background resume task: {error}"),
                is_error: true,
                note: None,
                display: None,
            };
        }
        return ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: [
                format!("task_id: {task_id}"),
                "status: running".into(),
                format!("agent_id: {resume_id}"),
                format!("actual_subagent_type: {profile_name}"),
                "automatic_notification: true".into(),
                String::new(),
                "next_step: The completion arrives automatically in a later turn — do NOT wait, \
                 poll, or call TaskOutput on it; continue with other work or hand back to the user."
                    .into(),
                format!(
                    "resume_hint: To continue or recover this same subagent later, call \
                     Agent(resume=\"{resume_id}\", run_in_background=true, prompt=\"...\"). \
                     The parameter is agent_id (\"{resume_id}\"), NOT task_id (\"{task_id}\")."
                ),
            ]
            .join("\n"),
            is_error: false,
            note: None,
            display: None,
        };
    }
    tokio::spawn(bg_future);

    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: [
            format!("agent_id: {resume_id}"),
            "status: running".into(),
            format!("actual_subagent_type: {profile_name}"),
            "automatic_notification: true".into(),
            String::new(),
            "next_step: The completion arrives automatically in a later turn — do NOT wait, \
             poll, or call TaskOutput on it; continue with other work or hand back to the user."
                .into(),
        ]
        .join("\n"),
        is_error: false,
        note: None,
        display: None,
    }
}
