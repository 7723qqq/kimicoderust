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

/// v2 `canRunInBackground`: all three task tools must be active for this
/// caller, both in the merged live table and in its own profile policy.
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

/// Execute the `Agent` tool natively (foreground core). Returns `None`
/// when the call must fall back to the host — see the module docs.
///
/// `root` is the toolset's canonical workspace root, threaded into the tower
/// resume gate's roster lookup: the gate must consult the tower state of the
/// workspace the session actually runs in, not wherever the process happens
/// to have its cwd. `None` — host-less calls — keeps the pre-P2-2 process
/// cwd lookup.
pub async fn execute_agent(
    manager: &Arc<SubagentManager>,
    root: Option<&std::path::Path>,
    args: &serde_json::Value,
    timeout_ms: Option<u64>,
    parent_cancel: Option<&ParentCancel>,
    tool_call_id: Option<&str>,
    pool: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
) -> Option<ExecutableToolResult> {
    // Without a `[secondary_model]` pool the engine still absorbs the `model`
    // parameter: omitting it inherits the caller's model, and any explicit
    // value — `primary` included (v2 #4418) — is the no-pool error. The host
    // no longer owns subagent model routing.
    let requested_model = string_arg(args, "model");
    // Native resume (P55): a `resume` for an agent whose conversation we
    // hold continues natively; unknown ids stay host-owned (v2 persistent
    // scopes live there).
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
    // v2 #4152 (`planSpawn`): a fork inherits the caller's profile when
    // `subagent_type` is omitted, so its system prompt — and with it the
    // whole request prefix — stays byte-identical to the caller's and the
    // provider's prompt cache survives the handoff. A plain spawn defaults
    // to `coder`.
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
        // v2 refuses `fork` outright when the flag is off. The flag defaults
        // ON here (see `subagent::fork::subagent_fork_enabled`), so this is
        // the opt-out path.
        if !crate::subagent::fork::subagent_fork_enabled() {
            return Some(ExecutableToolResult {
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
        if let Some(err) = crate::subagent::fork_incompatibility(
            resume.as_deref(),
            subagent_type.as_deref(),
            model.as_deref(),
            &caller_profile,
            None,
        ) {
            return Some(ExecutableToolResult {
                delivery: None,
                stop_turn: false,
                content: err.to_string(),
                is_error: true,
                note: None,
                display: None,
            });
        }
    }

    // Unknown profiles stay host-owned: plugin sources and external
    // backends never reach the pushed snapshot.
    manager.get_definition(&profile_name).await?;

    // v2 `planSpawn`: the requested type must be inside the caller's
    // allowlist — the caller profile's `subagents` declaration, else the
    // default profile's delegation set (plus the main agent's discovered
    // extras). Forks skip the gate: a fork inherits a type the caller was
    // already admitted as. The gate runs after the unknown-profile check so
    // types the engine never registered keep their host fallback (plugin
    // sources never reach the snapshot).
    if let Err(message) = manager
        .check_spawn_allowed(&caller_id, &profile_name, is_fork)
        .await
    {
        return Some(ExecutableToolResult {
            delivery: None,
            stop_turn: false,
            content: message,
            is_error: true,
            note: None,
            display: None,
        });
    }

    // No injected runtime (unwired transport) — the host tool still works.
    let runtime = manager.runtime().await?;

    if args.get("run_in_background").and_then(|v| v.as_bool()) == Some(true)
        && !background_agent_available(manager, &runtime, &caller_id).await
    {
        return Some(background_agent_unavailable());
    }

    // The pool binding applies to non-fork spawns (resumes keep their own
    // model either way, v2 `stripSubagentModelParameter`). A fork inherits
    // the caller's model and thinking verbatim (v2 #4152 `planSpawn`) — a
    // pool binding would move the fork onto the pool default and break the
    // request prefix it shares with the caller. A pool-bound caller's fork
    // binds that same pool entry (`instance_llm_of`); everyone else inherits
    // the session model.
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

    // P58: background execution — spawn, register in TaskRunner if present,
    // launch detached, return the v2 running shape immediately. Completion flows
    // back through the `subagent.completed` / `subagent.failed` lifecycle events,
    // which the host turns into the usual synthetic notification turn.
    if args.get("run_in_background").and_then(|v| v.as_bool()) == Some(true) {
        let agent_id = manager.spawn(&profile_name, &description).await.ok()?;
        if let Some(llm) = binding_llm.as_ref() {
            manager.set_instance_llm(&agent_id, llm.clone()).await;
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
                    // The user interrupted this background subagent: report it as
                    // cancelled (not failed), matching the foreground path.
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
            // Session attribution rides the runtime the pipeline built for
            // this turn — background task events land on the right lane.
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
                    },
                    agent_id.clone(),
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
            // The registration consumed `bg_future`, so the subagent can never
            // run to a terminal of its own. Reporting `status: running` here
            // while the future is already gone is the worst of the two, so
            // settle the lifecycle and hand the caller an error.
            let message = format!("failed to register the background task: {error}");
            emit_failed(
                runtime.callbacks.as_ref(),
                &agent_id,
                &profile_name,
                &message,
            );
            return Some(ExecutableToolResult {
                delivery: None,
                stop_turn: false,
                content: message,
                is_error: true,
                note: None,
                display: None,
            });
        }
        let content = [
            format!("task_id: {agent_id}"),
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
                 (\"{agent_id}\"), NOT task_id."
            ),
        ]
        .join("\n");
        return Some(ExecutableToolResult {
            delivery: None,
            stop_turn: false,
            content,
            is_error: false,
            note: None,
            display: None,
        });
    }

    // Spawn first so the id exists even if the run times out (the failure
    // text carries it, matching v2).
    let agent_id = manager.spawn(&profile_name, &description).await.ok()?;
    if let Some(llm) = binding_llm.as_ref() {
        manager.set_instance_llm(&agent_id, llm.clone()).await;
    }

    // v2 `emitAgentRunSpawned` + `mirrorAgentRun`'s SubagentStarted: the
    // host dispatches the same lifecycle events the host-side subagent
    // path does, keyed by the parent tool call id.
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

    // v2 `taskService` arms the timeout only when `timeoutMs > 0` — an
    // explicit `0` means "no timeout", not the 2h default.
    let timeout = match timeout_ms {
        Some(0) => u64::MAX,
        Some(ms) => ms,
        None => DEFAULT_SUBAGENT_TIMEOUT_MS,
    };

    let run = tokio::time::timeout(
        std::time::Duration::from_millis(timeout),
        manager.run_foreground_turn_with_history(
            &agent_id,
            &prompt,
            inherited_history,
            parent_cancel,
        ),
    )
    .await;

    let (content, is_error) = match run {
        Ok(Ok(ForegroundTurnOutcome::Completed(turn))) => report_foreground_turn(
            runtime.callbacks.as_ref(),
            &agent_id,
            &profile_name,
            &turn,
            parent_cancel,
        ),
        Ok(Ok(ForegroundTurnOutcome::ParentCancelled)) => {
            // The user interrupted the subagent: v2 emitted a distinct
            // `subagent.cancelled` here instead of a failure, so the TUI can
            // mark the member cancelled. The user-interruption message still
            // becomes the tool result.
            emit_cancelled(runtime.callbacks.as_ref(), &agent_id, &profile_name);
            (
                format_failure(
                    &agent_id,
                    &profile_name,
                    USER_INTERRUPTED_SUBAGENT_MESSAGE,
                    false,
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
                format_failure(&agent_id, &profile_name, &message, false),
                true,
            )
        }
        Err(_elapsed) => {
            let _ = manager.kill(&agent_id).await;
            let message = format!(
                "Agent timed out after {}.",
                format_timeout_description(timeout)
            );
            emit_failed(
                runtime.callbacks.as_ref(),
                &agent_id,
                &profile_name,
                &message,
            );
            (
                format_failure(&agent_id, &profile_name, &message, true),
                true,
            )
        }
    };
    Some(ExecutableToolResult {
        delivery: None,
        stop_turn: false,
        content,
        is_error,
        note: None,
        display: None,
    })
}
