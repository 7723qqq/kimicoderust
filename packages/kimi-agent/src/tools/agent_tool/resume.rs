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

/// The tower resume gate (v2 `towerService`'s foreground-resume veto,
/// #3648): the denial message when the resumed agent is a tower agent —
/// its profile is the tower worker profile *or* its id sits in the tower
/// roster — and the resume is not backgrounded. The v2 exception stays:
/// with no background execution on the host table (`is_background_available`
/// false — no `TaskList`/`TaskOutput`/`TaskStop`), a foreground resume is the
/// only form the tool accepts, so it is not denied. `None` means the resume
/// proceeds as before: a plain profile with an empty roster query, an
/// explicit `run_in_background=true`, or an unreadable tower state (missing
/// `state.json` counts as no tower, so a false positive never blocks a
/// plain resume).
///
/// `cwd` (P2-2) is the base the roster lookup resolves the tower repo root
/// from: the toolset passes its canonical workspace root so the gate reads
/// the session's tower state rather than the process cwd's; no-toolset call
/// sites fall back to `std::env::current_dir()`.
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
        // No tower (or no cwd): the tower cannot vouch for any id, so no
        // foreground resume is refused here.
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

/// Execute a native `resume`: continue a conversation the manager still
/// holds. Returns `None` when the id is unknown (host-owned persistent
/// scopes) so the call falls back verbatim.
pub(super) async fn execute_resume(
    manager: &Arc<SubagentManager>,
    root: Option<&std::path::Path>,
    args: &serde_json::Value,
    resume_id: &str,
    timeout_ms: Option<u64>,
    parent_cancel: Option<&ParentCancel>,
    tool_call_id: Option<&str>,
) -> Option<ExecutableToolResult> {
    let profile_name = manager.resume_profile(resume_id).await?;
    // v2 `agentTool.ts` guards the resume path before it does anything:
    // `AGENT_NOT_A_SUBAGENT`, `AGENT_NOT_OWNED` and `AGENT_ALREADY_RUNNING`.
    // Without them any agent could name any other agent's id in `resume` and
    // drive a turn inside its conversation — the single-agent twin of the hole
    // `AgentSwarm` had. The two guards are the same functions the swarm resume
    // runs, so the two paths cannot drift apart.
    let caller_agent_id = crate::tools::CALLER_AGENT_ID
        .try_with(|id| id.clone())
        .unwrap_or_else(|_| crate::callbacks::MAIN_AGENT_ID.to_string());
    if let Err(error) =
        crate::swarm::service::require_owned_subagent(manager, &caller_agent_id, resume_id).await
    {
        return Some(ExecutableToolResult {
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

    // Tower resume discipline (v2 `towerService` Agent veto, #3648): the main
    // agent never needs a tower agent's return value inline — its output flows
    // back through the tower protocol files — while a foreground resume blocks
    // the whole turn and jams the fleet. A roster agent resumed in the
    // foreground is therefore refused with the background hint, unless this
    // caller lacks any of TaskList / TaskOutput / TaskStop.
    let task_runner = manager.get_task_runner().await;
    if let Some(reason) = tower_resume_denial(
        // P2-2: the toolset's canonical workspace root when the host wired
        // one, else the process cwd as before (no-toolset call sites).
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
            delivery: None,
            stop_turn: false,
            content: reason,
            is_error: true,
            note: None,
            display: None,
        });
    }

    if run_in_background == Some(true) {
        // The background helper emits its own spawned+started pair, so no
        // foreground one here.
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

    // v2 `taskService` arms the timeout only when `timeoutMs > 0` — an
    // explicit `0` means "no timeout", not the 2h default.
    let timeout = match timeout_ms {
        Some(0) => u64::MAX,
        Some(ms) => ms,
        None => DEFAULT_SUBAGENT_TIMEOUT_MS,
    };
    let run = tokio::time::timeout(
        std::time::Duration::from_millis(timeout),
        manager.resume_foreground_turn(resume_id, &prompt, parent_cancel),
    )
    .await;

    let run = match run {
        // The manager answered `None` only if its runtime vanished mid-call;
        // surface it as a regular failure instead of a fallback.
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
            // Same terminal the fresh-spawn and background-resume arms emit
            // (`:1038`, `:611`): without it `subagent.started` from
            // `emit_spawned_started` above is never settled and the member
            // stays Running forever.
            emit_cancelled(runtime.callbacks.as_ref(), resume_id, &profile_name);
            (
                format_failure(
                    resume_id,
                    &profile_name,
                    USER_INTERRUPTED_SUBAGENT_MESSAGE,
                    false,
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
                format_failure(resume_id, &profile_name, &message, false),
                true,
            )
        }
        Ok(None) => {
            // Runtime vanished mid-call; surface it as a regular failure.
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
                format_failure(resume_id, &profile_name, &message, false),
                true,
            )
        }
        Err(_elapsed) => {
            let _ = manager.kill(resume_id).await;
            let message = format!(
                "Agent timed out after {}.",
                format_timeout_description(timeout)
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
                format_failure(resume_id, &profile_name, &message, true),
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

/// A background resume (P58 semantics on the resume path): the continued
/// turn runs detached; the completion flows back through the
/// `subagent.completed` / `subagent.failed` lifecycle events, which the host
/// turns into the usual synthetic notification turn. Registered in the
/// TaskRunner when one is present so the task is listed and stoppable — the
/// `task_runner` argument is always `Some` on the call sites this helper
/// serves (the tower gate below falls back to foreground without one).
/// The task id is freshly minted per registration — the agent id is stable
/// across resumes, and TaskRunner keeps settled entries, so reusing it would
/// make every later resume a rejected duplicate.
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
        // No parent cancel: a background subagent outlives this turn and is
        // stopped through the task runner, not the turn's cancel signal
        // (the v2 registerTask arms no signal for detached tasks).
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
                // Not `Killed`: that status means the runner's own `stop()` cut
                // the task short, and this arm means the subagent was
                // cancelled upstream while the task ran to its end.
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
        // Session attribution rides the runtime the pipeline built for
        // this turn — background task events land on the right lane.
        let session_id = manager
            .runtime()
            .await
            .and_then(|r| r.session_id.clone())
            .filter(|session| !session.is_empty());
        // A resume reuses the agent's stable id, so the task id must be
        // freshly minted: TaskRunner rejects duplicate ids and keeps settled
        // entries, so the second background resume of the same agent under
        // the agent id would be silently swallowed (v2 `generateTaskId`
        // mints a fresh id per registration; the bash background path uses
        // the same `task_{fastrand}` pattern).
        let task_id = format!("task_{}", fastrand::u64(..));
        let registered = runner.spawn_task_with_meta(
            crate::storage::TaskSpawnMeta {
                session_id: session_id.as_deref(),
                kind: "subagent",
                subagent_type: Some(&profile),
                agent_id: Some(resume_id),
            },
            task_id.clone(),
            format!("Resume {profile}: {prompt}"),
            bg_future,
        );
        if let Err(error) = registered {
            // `bg_future` is consumed by the failed registration, so the
            // `subagent.started` already published above would never be
            // settled. Settle it here before returning.
            emit_failed(
                callbacks.as_ref(),
                resume_id,
                profile_name,
                &format!("failed to register the background resume task: {error}"),
            );
            return ExecutableToolResult {
                delivery: None,
                stop_turn: false,
                content: format!("failed to register the background resume task: {error}"),
                is_error: true,
                note: None,
                display: None,
            };
        }
        return ExecutableToolResult {
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
