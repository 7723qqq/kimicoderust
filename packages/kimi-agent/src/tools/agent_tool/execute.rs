use std::sync::Arc;

use super::defs::{
    DEFAULT_PROFILE_NAME, DEFAULT_SUBAGENT_TIMEOUT_MS, SUBAGENT_STOPPED_MESSAGE,
    USER_INTERRUPTED_SUBAGENT_MESSAGE, background_agent_available_for_caller,
    background_agent_unavailable, format_timeout_description, required_string_arg, string_arg,
};
use super::events::{emit_cancelled, emit_completed, emit_failed, emit_spawned_started};
use super::format::{format_failure, report_foreground_turn, stopped_task_output};
use super::resume::execute_resume;
use crate::storage::TaskOutcome;
use crate::subagent::SubagentManager;
use crate::subagent::manager::ForegroundTurnOutcome;
use crate::subagent::types::ParentCancel;
use crate::turn_loop::types::{ExecutableToolResult, LoopTurnStopReason};

const BACKENDS_UNAVAILABLE: &str =
    "backend is disabled: the subagent_backends experimental flag is off.";

pub(super) async fn background_agent_available(
    manager: &Arc<SubagentManager>,
    runtime: &crate::subagent::manager::SubagentRuntime,
    caller_id: &str,
) -> bool {
    let Ok(response) = runtime.callbacks.list_tools().await else {
        return false;
    };
    background_agent_available_for_caller(manager, caller_id, &|required| {
        response
            .tools
            .iter()
            .any(|tool| tool.name.eq_ignore_ascii_case(required))
    })
    .await
}

pub async fn execute_agent(
    manager: &Arc<SubagentManager>,
    root: Option<&std::path::Path>,
    args: &serde_json::Value,
    timeout_ms: Option<u64>,
    parent_cancel: Option<&ParentCancel>,
    tool_call_id: Option<&str>,
    pool: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
) -> Option<ExecutableToolResult> {
    if string_arg(args, "backend").is_some_and(|value| !value.trim().is_empty()) {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: BACKENDS_UNAVAILABLE.to_string(),
            is_error: true,
            note: None,
            display: None,
        });
    }

    let requested_model = string_arg(args, "model");

    if let Some(resume_id) = string_arg(args, "resume") {
        return execute_resume(
            manager,
            root,
            args,
            &resume_id,
            timeout_ms,
            parent_cancel,
            tool_call_id,
        )
        .await;
    }

    let is_fork = args.get("fork").and_then(|v| v.as_bool()).unwrap_or(false);

    let caller_id = crate::tools::CALLER_AGENT_ID
        .try_with(|id| id.clone())
        .unwrap_or_else(|_| crate::callbacks::MAIN_AGENT_ID.to_string());
    let caller_profile = manager
        .caller_profile_name(&caller_id)
        .await
        .unwrap_or_else(|| DEFAULT_PROFILE_NAME.into());
    let profile_name = if is_fork {
        string_arg(args, "subagent_type").unwrap_or_else(|| caller_profile.clone())
    } else {
        string_arg(args, "subagent_type").unwrap_or_else(|| DEFAULT_PROFILE_NAME.into())
    };

    if is_fork {
        if !manager.agent_exists(&caller_id).await {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: format!("agent.not_found: Source agent \"{caller_id}\" does not exist"),
                is_error: true,
                note: None,
                display: None,
            });
        }

        if !crate::subagent::fork::subagent_fork_enabled() {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: crate::subagent::fork::FORK_EXPERIMENTAL_UNAVAILABLE.to_string(),
                is_error: true,
                note: None,
                display: None,
            });
        }
        let resume = string_arg(args, "resume");
        let subagent_type = string_arg(args, "subagent_type");
        let model = string_arg(args, "model");

        let caller_model_alias = manager.caller_model_alias().await;
        if let Some(err) = crate::subagent::fork_incompatibility(
            resume.as_deref(),
            subagent_type.as_deref(),
            model.as_deref(),
            &caller_profile,
            caller_model_alias.as_deref(),
        ) {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: err.to_string(),
                is_error: true,
                note: None,
                display: None,
            });
        }
    }

    manager.get_definition(&profile_name).await?;

    if let Err(message) = manager
        .check_spawn_allowed(&caller_id, &profile_name, is_fork)
        .await
    {
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: message,
            is_error: true,
            note: None,
            display: None,
        });
    }

    let runtime = manager.runtime().await?;

    if args.get("run_in_background").and_then(|v| v.as_bool()) == Some(true)
        && !background_agent_available(manager, &runtime, &caller_id).await
    {
        return Some(background_agent_unavailable());
    }

    let binding_llm = if is_fork {
        Some(
            manager
                .instance_llm_of(&caller_id)
                .await
                .unwrap_or_else(|| runtime.llm.clone()),
        )
    } else {
        match pool {
            Some(pool) => match pool.resolve(requested_model.as_deref()) {
                Ok(binding) => Some(binding.llm),
                Err(message) => {
                    return Some(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: false,
                        content: message,
                        is_error: true,
                        note: None,
                        display: None,
                    });
                }
            },
            None => {
                if let Err(message) =
                    crate::subagent::secondary::SecondaryModelRuntime::resolve_without_pool(
                        requested_model.as_deref(),
                    )
                {
                    return Some(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: false,
                        content: message,
                        is_error: true,
                        note: None,
                        display: None,
                    });
                }
                None
            }
        }
    };

    let prompt = match required_string_arg(args, "prompt") {
        Ok(prompt) => prompt,
        Err(error) => return Some(error),
    };
    let description = string_arg(args, "description").unwrap_or_default();

    let inherited_history = if is_fork {
        crate::tools::CURRENT_CONVERSATION_HISTORY
            .try_with(|slot| slot.lock().unwrap().clone())
            .ok()
            .or_else(|| {
                crate::tools::CALLER_AGENT_ID
                    .try_with(|id| manager.get_foreground_history(id))
                    .ok()
                    .flatten()
            })
    } else {
        None
    };

    if args.get("run_in_background").and_then(|v| v.as_bool()) == Some(true) {
        let agent_id = match manager.spawn(&profile_name, &description).await {
            Ok(id) => id,
            Err(message) => {
                return Some(ExecutableToolResult {
                    spill: None,
                    parts: Vec::new(),
                    delivery: None,
                    stop_turn: false,
                    content: message,
                    is_error: true,
                    note: None,
                    display: None,
                });
            }
        };
        if let Some(llm) = binding_llm.as_ref() {
            manager.set_instance_llm(&agent_id, llm.clone()).await;
        }
        if is_fork {
            manager
                .set_forked_from(&agent_id, Some(caller_id.clone()))
                .await;

            manager.arm_fork_probe(&agent_id, &caller_id);
        }
        emit_spawned_started(
            runtime.callbacks.as_ref(),
            &agent_id,
            &profile_name,
            tool_call_id,
            Some(&description),
            true,
            None,
            &prompt,
        );
        let mgr = manager.clone();
        let cb = runtime.callbacks.clone();
        let agent = agent_id.clone();
        let bg_history = inherited_history.clone();
        let prompt_clone = prompt.clone();
        let task_desc = if description.is_empty() {
            format!("Subagent {profile_name}: {prompt_clone}")
        } else {
            description.clone()
        };
        let task_runner = manager.get_task_runner().await;

        let task_id = crate::storage::generate_task_id("agent");

        let profile = profile_name.clone();
        let bg_future = async move {
            let outcome = mgr
                .run_foreground_turn_with_history(&agent, &prompt_clone, bg_history, None)
                .await;
            match outcome {
                Ok(ForegroundTurnOutcome::Completed(turn)) => {
                    if matches!(turn.stop_reason, LoopTurnStopReason::Aborted) {
                        emit_failed(cb.as_ref(), &agent, &profile, SUBAGENT_STOPPED_MESSAGE);
                        TaskOutcome::Failed(stopped_task_output(SUBAGENT_STOPPED_MESSAGE, &turn))
                    } else {
                        let summary =
                            crate::subagent::manager::final_assistant_summary(&turn.messages);
                        emit_completed(cb.as_ref(), &agent, &profile, &summary, &turn.usage);
                        TaskOutcome::Completed(summary)
                    }
                }
                Ok(ForegroundTurnOutcome::ParentCancelled) => {
                    emit_cancelled(cb.as_ref(), &agent, &profile);
                    TaskOutcome::Failed(USER_INTERRUPTED_SUBAGENT_MESSAGE.to_string())
                }
                Err(err_msg) => {
                    emit_failed(cb.as_ref(), &agent, &profile, &err_msg);
                    TaskOutcome::Failed(format!("Error: {err_msg}"))
                }
            }
        };

        let spawn_error = if let Some(runner) = task_runner {
            let session_id = manager
                .runtime()
                .await
                .and_then(|r| r.session_id.clone())
                .filter(|session| !session.is_empty());
            runner
                .spawn_task_with_meta(
                    crate::storage::TaskSpawnMeta {
                        session_id: session_id.as_deref(),
                        kind: "subagent",
                        subagent_type: Some(&profile_name),
                        agent_id: Some(agent_id.as_str()),
                        timeout_ms: None,
                    },
                    task_id.clone(),
                    task_desc,
                    bg_future,
                )
                .err()
        } else {
            tokio::spawn(async move {
                let _ = bg_future.await;
            });
            None
        };
        if let Some(error) = spawn_error {
            let message = format!("failed to register the background task: {error}");
            emit_failed(
                runtime.callbacks.as_ref(),
                &agent_id,
                &profile_name,
                &message,
            );
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: message,
                is_error: true,
                note: None,
                display: None,
            });
        }
        let content = [
            format!("task_id: {task_id}"),
            "status: running".into(),
            format!("agent_id: {agent_id}"),
            format!("actual_subagent_type: {profile_name}"),
            "automatic_notification: true".into(),
            String::new(),
            format!("description: {description}"),
            String::new(),
            "next_step: The completion arrives automatically in a later turn — do NOT wait, \
             poll, or call TaskOutput on it; continue with other work or hand back to the user. \
             (If you have nothing to do until it finishes, run such tasks in the foreground next time.)"
                .into(),
            format!(
                "resume_hint: To continue or recover this same subagent later, call \
                 Agent(resume=\"{agent_id}\", prompt=\"...\"). The parameter is agent_id \
                 (\"{agent_id}\"), NOT task_id (\"{task_id}\") or source_id from a later \
                 <notification>. Recovery cases: a later <notification type=\"task.lost\" | \
                 \"task.failed\" | \"task.killed\"> for this subagent — its conversation history \
                 is preserved across session restarts and resume will pick it up."
            ),
        ]
        .join("\n");
        return Some(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content,
            is_error: false,
            note: None,
            display: None,
        });
    }

    let agent_id = match manager.spawn(&profile_name, &description).await {
        Ok(id) => id,
        Err(message) => {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: message,
                is_error: true,
                note: None,
                display: None,
            });
        }
    };
    if let Some(llm) = binding_llm.as_ref() {
        manager.set_instance_llm(&agent_id, llm.clone()).await;
    }
    if is_fork {
        manager
            .set_forked_from(&agent_id, Some(caller_id.clone()))
            .await;

        manager.arm_fork_probe(&agent_id, &caller_id);
    }

    emit_spawned_started(
        runtime.callbacks.as_ref(),
        &agent_id,
        &profile_name,
        tool_call_id,
        Some(&description),
        false,
        None,
        &prompt,
    );

    let bound_ms = match timeout_ms {
        Some(0) => None,
        Some(ms) => Some(ms),
        None => Some(DEFAULT_SUBAGENT_TIMEOUT_MS),
    };

    let turn = manager.run_foreground_turn_with_history(
        &agent_id,
        &prompt,
        inherited_history,
        parent_cancel,
    );

    let run: Result<_, u64> = match bound_ms {
        Some(ms) => tokio::time::timeout(std::time::Duration::from_millis(ms), turn)
            .await
            .map_err(|_| ms),
        None => Ok(turn.await),
    };

    let (content, is_error) = match run {
        Ok(Ok(ForegroundTurnOutcome::Completed(turn))) => report_foreground_turn(
            runtime.callbacks.as_ref(),
            &agent_id,
            &profile_name,
            &turn,
            parent_cancel,
        ),
        Ok(Ok(ForegroundTurnOutcome::ParentCancelled)) => {
            emit_cancelled(runtime.callbacks.as_ref(), &agent_id, &profile_name);
            (
                format_failure(
                    &agent_id,
                    &profile_name,
                    USER_INTERRUPTED_SUBAGENT_MESSAGE,
                    "cancelled",
                    None,
                ),
                true,
            )
        }
        Ok(Err(message)) => {
            emit_failed(
                runtime.callbacks.as_ref(),
                &agent_id,
                &profile_name,
                &message,
            );

            (
                format_failure(
                    &agent_id,
                    &profile_name,
                    &message,
                    super::format::run_failure_reason(&message),
                    None,
                ),
                true,
            )
        }
        Err(timeout_ms) => {
            let _ = manager.kill(&agent_id).await;
            let message = format!(
                "Agent timed out after {}.",
                format_timeout_description(timeout_ms)
            );
            emit_failed(
                runtime.callbacks.as_ref(),
                &agent_id,
                &profile_name,
                &message,
            );
            (
                format_failure(&agent_id, &profile_name, &message, "timed_out", None),
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
