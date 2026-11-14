use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::_base::utils::abort::user_cancellation_reason;
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::task::task::{AgentTaskInfo, IAgentTaskService};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::features::tower::protocol::types::{
    TowerMissionKind, TowerMissionStatus, TowerRosterEntry,
};
use crate::features::tower::tools::support::run_tower_tool;
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::status::{TowerStatusToolInput, tower_status_tool_input_schema};
use super::statusReader::{
    InitializedTowerStatus, ReadTowerStatusInput, TowerStatus, TowerStatusConcurrency,
    TowerStatusGate, TowerStatusState, read_tower_status,
};

pub const TOWER_STATUS_TOOL_NAME: &str = "TowerStatus";

pub trait ITowerRateLimitService: Send + Sync {
    fn snapshot(&self) -> TowerStatusConcurrency;
}

#[derive(Clone)]
pub struct TowerStatusTool {
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
    rate_limit: Arc<dyn ITowerRateLimitService>,
    tasks: Arc<dyn IAgentTaskService>,
}

impl TowerStatusTool {
    pub fn new(
        session_context: Arc<dyn ISessionContext>,
        scope_context: Arc<dyn IAgentScopeContext>,
        rate_limit: Arc<dyn ITowerRateLimitService>,
        tasks: Arc<dyn IAgentTaskService>,
    ) -> Self {
        Self {
            session_context,
            scope_context,
            rate_limit,
            tasks,
        }
    }

    pub fn name(&self) -> &'static str {
        TOWER_STATUS_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        include_str!("status.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_status_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerStatus input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerStatusToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerStatus: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerStatusToolInput) -> ToolExecution {
        let limit = args.limit.unwrap_or(20);
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some("Reading tower status".to_string()),
            stop_batch_after_this: None,
            approval_rule: TOWER_STATUS_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                Box::pin(async move { Ok(this.execute(limit).await) })
            }),
        })
    }

    async fn execute(&self, limit: usize) -> ExecutableToolResult {
        run_tower_tool(async move {
            let cwd = self.session_context.cwd();
            let agent_id = self.scope_context.agent_id();
            let tasks = self.tasks.clone();
            let rate_limit = self.rate_limit.clone();
            let running_agent_ids = move || active_agent_ids(tasks.as_ref());
            let concurrency = move || rate_limit.snapshot();
            let status = read_tower_status(&ReadTowerStatusInput {
                active: true,
                cwd: Path::new(&cwd),
                agent_id: &agent_id,
                running_agent_ids: &running_agent_ids,
                concurrency: &concurrency,
            })
            .await?;
            match status {
                TowerStatus::NotInitialized {
                    active,
                    state_lost,
                } => {
                    if state_lost {
                        return Ok(ExecutableToolResult {
                            output: render_state_lost_report().into(),
                            ..Default::default()
                        });
                    }
                    let _ = active;
                    Ok(ExecutableToolResult::error(
                        "tower is not initialized in this repository — run TowerInit first",
                    ))
                }
                TowerStatus::Initialized(status) => {
                    if !status.active {
                        return Ok(ExecutableToolResult {
                            output: "# Tower status — OFF".into(),
                            ..Default::default()
                        });
                    }
                    Ok(ExecutableToolResult {
                        output: render_initialized_status(&status, limit).into(),
                        ..Default::default()
                    })
                }
            }
        })
        .await
    }
}

impl ExecutableTool for TowerStatusTool {
    fn name(&self) -> &str {
        TowerStatusTool::name(self)
    }

    fn description(&self) -> &str {
        TowerStatusTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerStatusTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}

fn active_agent_ids(tasks: &dyn IAgentTaskService) -> HashSet<String> {
    tasks
        .list(Some(true), None)
        .into_iter()
        .filter_map(|task| match task {
            AgentTaskInfo::Agent(info) => info.agent_id,
            _ => None,
        })
        .collect()
}

pub(crate) fn render_status_text(status: &TowerStatus, limit: usize) -> (String, bool) {
    match status {
        TowerStatus::NotInitialized { state_lost, .. } => {
            if *state_lost {
                (render_state_lost_report(), false)
            } else {
                (
                    "tower is not initialized in this repository — run TowerInit first".to_string(),
                    true,
                )
            }
        }
        TowerStatus::Initialized(status) => {
            if status.active {
                (render_initialized_status(status, limit), false)
            } else {
                ("# Tower status — OFF".to_string(), false)
            }
        }
    }
}

fn render_initialized_status(status: &InitializedTowerStatus, limit: usize) -> String {
    let state = &status.state;
    let mut sections = vec![
        format!(
            "# Tower status — base: {} (mode: {}), you are: {}",
            state.base, state.mode, status.caller
        ),
        String::new(),
    ];
    sections.extend(render_recovered_at_warning(state));
    sections.push("## Missions".to_string());
    sections.push(String::new());
    sections.extend(render_missions(state, limit));
    sections.extend(render_unspawned_missions(state, limit));
    sections.extend(render_death_warnings(state, limit));
    sections.push(String::new());
    sections.push("## Roster".to_string());
    sections.push(String::new());
    sections.extend(render_roster(state, limit));
    sections.push(String::new());
    sections.push("## Review gate (unmerged branches)".to_string());
    sections.push(String::new());
    sections.extend(render_review_gate(state, &status.gate, limit));
    if !state.missions.is_empty()
        && state.missions.iter().all(|mission| {
            mission.status == TowerMissionStatus::Merged
                || mission.status == TowerMissionStatus::Abandoned
        })
    {
        sections.push(String::new());
        sections.push("## Done".to_string());
        sections.push(String::new());
        sections.push("All missions are merged or abandoned. Free the worktree checkouts now: run TowerTeardown (branches and .tower/comms/ are kept; dirty worktrees are protected).".to_string());
    }
    sections.push(String::new());
    sections.push("## Inbox".to_string());
    sections.push(String::new());
    sections.push(format!(
        "{} message(s) visible to you — read with TowerInbox.",
        status.inbox.count
    ));
    sections.push(String::new());
    sections.push("## Concurrency (adaptive)".to_string());
    sections.push(String::new());
    sections.push(render_concurrency(&status.concurrency));
    sections.push(String::new());
    sections.push("## Recent activity".to_string());
    sections.push(String::new());
    if status.recent_activity.is_empty() {
        sections.push("(activity log is empty)".to_string());
    } else {
        sections.extend(status.recent_activity.iter().cloned());
    }
    sections.join("\n")
}

fn render_state_lost_report() -> String {
    [
        "# Tower status — STATE LOST",
        "",
        ".tower/comms/state.json is missing but the .tower/ directory still exists — this tower was initialized here before, and its comms state was deleted (e.g. by git clean). All recorded mission, roster, and review history is lost.",
        "",
        "Report this history loss to the user now — continuing silently would pretend the loss never happened.",
        "",
        "What still works:",
        "- TowerSend and TowerComplete recover a minimal state automatically (stamped recoveredAt, base taken from the current checkout branch) and keep delivering messages — worker reports are not lost.",
        "- TowerMerge and TowerRebase refuse with an explicit state-lost error instead of silently running against an empty history.",
        "- Mission branches and worktrees under .tower/worktrees survive on disk; re-plan from that evidence only after the user decides how to proceed.",
    ]
    .join("\n")
}

fn render_recovered_at_warning(state: &TowerStatusState) -> Vec<String> {
    let Some(recovered_at) = state.recovered_at.as_deref() else {
        return Vec::new();
    };
    vec![
        "## ⚠️ State recovered after a loss".to_string(),
        String::new(),
        format!(
            "This tower's state was reconstructed at {recovered_at} after .tower/comms/state.json was lost — every mission, roster entry, and review from before that point is gone, so the lists below are the recovered minimum, not reality. Report the history loss to the user before continuing routine orchestration."
        ),
        String::new(),
    ]
}

fn render_review_gate(
    state: &TowerStatusState,
    gate: &[TowerStatusGate],
    limit: usize,
) -> Vec<String> {
    let pending = state
        .missions
        .iter()
        .filter(|mission| {
            mission.status != TowerMissionStatus::Merged
                && mission.status != TowerMissionStatus::Abandoned
        })
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return vec!["(no open missions — or none planned yet)".to_string()];
    }
    let visible = gate.iter().rev().take(limit).collect::<Vec<_>>();
    let visible_len = visible.len();
    let mut lines = vec![
        format!(
            "Showing {visible_len} of {} open mission(s), newest first.",
            pending.len()
        ),
        String::new(),
    ];
    for mission_gate in &visible {
        let review_text = match &mission_gate.selected_review {
            None => String::new(),
            Some(review) => format!(
                "; review=round {} by {} {}/{} binding={}",
                review.round,
                review.reviewer,
                review.status,
                if review.merge.is_empty() {
                    "legacy"
                } else {
                    review.merge.as_str()
                },
                mission_gate
                    .review_binding
                    .map(|binding| binding.as_str())
                    .unwrap_or("unavailable")
            ),
        };
        if mission_gate.ready {
            lines.push(format!(
                "- {} ({}): READY reasons=none{}{review_text}",
                mission_gate.branch,
                mission_gate.mission_id,
                if mission_gate.noop {
                    "; zero-diff survey no-op, review skipped"
                } else {
                    ""
                }
            ));
            continue;
        }
        let reasons = mission_gate
            .blocks
            .iter()
            .map(|block| block.reason)
            .collect::<Vec<_>>()
            .join(",");
        let details = mission_gate
            .blocks
            .iter()
            .map(|block| {
                block
                    .message
                    .strip_prefix("merge blocked: ")
                    .unwrap_or(&block.message)
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(format!(
            "- {} ({}): BLOCKED reasons={}; {}{review_text}",
            mission_gate.branch, mission_gate.mission_id, reasons, details
        ));
    }
    lines.extend(render_omission(
        pending.len() - visible_len,
        "review gate entries",
    ));
    lines
}

fn render_concurrency(snapshot: &TowerStatusConcurrency) -> String {
    let budget = if snapshot.budget.is_finite() {
        snapshot.budget.to_string()
    } else {
        "unlimited".to_string()
    };
    let mut parts = vec![format!(
        "budget: {budget} agent(s) · inflight: {}",
        snapshot.inflight
    )];
    match snapshot.blocked_until {
        Some(blocked_until) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis() as f64)
                .unwrap_or(0.0);
            let remaining = blocked_until - now;
            parts.push(if remaining > 0.0 {
                format!(
                    "spawns PAUSED for ~{}s (provider rate limit — successful requests lift the pause early)",
                    (remaining / 1000.0).ceil() as i64
                )
            } else {
                "spawn pause expired — budget probing resumes".to_string()
            });
        }
        None => parts.push("spawns open".to_string()),
    }
    parts.join(" · ")
}

fn render_missions(state: &TowerStatusState, limit: usize) -> Vec<String> {
    if state.missions.is_empty() {
        return vec!["(no missions planned — use TowerPlan)".to_string()];
    }
    let mut counts: Vec<(TowerMissionStatus, usize)> = Vec::new();
    for mission in &state.missions {
        match counts
            .iter_mut()
            .find(|(status, _)| *status == mission.status)
        {
            Some((_, count)) => *count += 1,
            None => counts.push((mission.status, 1)),
        }
    }
    let totals = counts
        .iter()
        .map(|(status, count)| format!("{count} {}", status.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let visible = state
        .missions
        .iter()
        .rev()
        .take(limit)
        .collect::<Vec<_>>();
    let mut lines = vec![
        format!(
            "Showing {} of {} mission(s), newest first.",
            visible.len(),
            state.missions.len()
        ),
        format!("Status totals: {totals}."),
        String::new(),
        "| ID | Mission | Branch | Worktree | Status | Owner | Kind |".to_string(),
        "| -- | ------- | ------ | -------- | ------ | ----- | ---- |".to_string(),
    ];
    lines.extend(visible.iter().map(|mission| {
        format!(
            "| {} | {}{} | {} | {} | {} {} | {} | {} |",
            mission.id,
            mission.title,
            if mission.kind == TowerMissionKind::Survey {
                " 🔍"
            } else {
                ""
            },
            mission.branch,
            mission.worktree,
            mission.status.emoji(),
            mission.status.as_str(),
            mission.owner.as_deref().unwrap_or("—"),
            mission.kind.as_str()
        )
    }));
    lines.extend(render_omission(
        state.missions.len() - visible.len(),
        "missions",
    ));
    lines
}

fn render_roster(state: &TowerStatusState, limit: usize) -> Vec<String> {
    if state.roster.agents.is_empty() {
        return vec![
            "(no agents registered — spawn workers/reviewers with TowerSpawn)".to_string(),
        ];
    }
    let workers = state
        .roster
        .agents
        .iter()
        .filter(|agent| agent.kind.as_str() == "worker")
        .count();
    let reviewers = state.roster.agents.len() - workers;
    let mut agents = state.roster.agents.iter().collect::<Vec<_>>();
    agents.sort_by(|a, b| b.spawned_at.cmp(&a.spawned_at));
    let visible = agents.into_iter().take(limit).collect::<Vec<_>>();
    let assigned = visible.iter().map(|agent| {
        let assignment = if agent.kind.as_str() == "worker" {
            format!(
                "mission {} (branch {}, worktree {})",
                agent.mission_id.as_deref().unwrap_or("?"),
                agent.branch.as_deref().unwrap_or("?"),
                agent.worktree.as_deref().unwrap_or("?")
            )
        } else {
            format!(
                "reviewing {}",
                agent.review_target.as_deref().unwrap_or("?")
            )
        };
        let death = match agent.died_at {
            None => String::new(),
            Some(_) => format!(
                " — 💀 {}",
                agent.death_status.as_deref().unwrap_or("died")
            ),
        };
        format!(
            "- {} ({}) — agent {}, {assignment}{death}",
            agent.name,
            agent.kind.as_str(),
            agent.agent_id
        )
    });
    let mut lines = vec![
        format!(
            "Showing {} of {} agent(s) ({} workers, {} reviewers), newest spawned first.",
            visible.len(),
            state.roster.agents.len(),
            workers,
            reviewers
        ),
        String::new(),
    ];
    lines.extend(assigned);
    lines.extend(render_omission(
        state.roster.agents.len() - visible.len(),
        "roster entries",
    ));
    lines
}

fn render_unspawned_missions(state: &TowerStatusState, limit: usize) -> Vec<String> {
    let pending = state
        .missions
        .iter()
        .filter(|mission| {
            mission.status == TowerMissionStatus::Planned && mission.owner.is_none()
        })
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Vec::new();
    }
    let visible = pending.iter().rev().take(limit).collect::<Vec<_>>();
    let mut lines = vec![
        String::new(),
        "## Awaiting spawn".to_string(),
        String::new(),
        format!(
            "Showing {} of {} planned mission(s), newest first.",
            visible.len(),
            pending.len()
        ),
        String::new(),
    ];
    lines.extend(visible.iter().map(|mission| {
        format!(
            "- {} [{}] ({}) — planned but no worker spawned yet: launch one with TowerSpawn(kind=\"worker\", mission_id=\"{}\", name=\"...\")",
            mission.id,
            mission.kind.as_str(),
            mission.branch,
            mission.id
        )
    }));
    lines.extend(render_omission(
        pending.len() - visible.len(),
        "awaiting-spawn missions",
    ));
    lines
}

fn render_death_warnings(state: &TowerStatusState, limit: usize) -> Vec<String> {
    let dead_by_name: HashMap<&str, &TowerRosterEntry> = state
        .roster
        .agents
        .iter()
        .filter(|agent| agent.died_at.is_some())
        .map(|agent| (agent.name.as_str(), agent))
        .collect();
    let mut warnings: Vec<(String, String)> = Vec::new();
    for mission in &state.missions {
        let Some(owner) = mission.owner.as_deref() else {
            continue;
        };
        if mission.status == TowerMissionStatus::Merged
            || mission.status == TowerMissionStatus::Abandoned
        {
            continue;
        }
        let Some(entry) = dead_by_name.get(owner) else {
            continue;
        };
        let died_at = entry.died_at.clone().unwrap_or_default();
        let status = entry.death_status.as_deref().unwrap_or("unknown");
        let text = if is_stopped_by_user(entry) {
            format!(
                "- 🛑 {} owner {} was stopped by the user ({status}) — dead by intent: never resume it and do not reassign the mission unless the human asks",
                mission.id, entry.name
            )
        } else if entry.kind.as_str() == "reviewer" {
            format!(
                "- ⚠️ {} reviewer {} died ({status}) — diagnose first: check the death status/reason and task state. A reviewer is one-shot: do not resume it or another roster agent; after diagnosis, spawn a fresh reviewer only if the mission still needs one",
                mission.id, entry.name
            )
        } else {
            format!(
                "- ⚠️ {} owner {} died ({status}) — diagnose first: check why it died (the died entry's status/reason, its task state) before reviving anything. Resume with Agent(resume=\"{}\", run_in_background=true, prompt=\"...\") (foreground only when background task tools are unavailable) or reassign the mission only when the cause is transient (lost contact, timeout, OOM); a systematic cause (code or environment defect) is fixed or escalated to the human before any revive",
                mission.id, entry.name, entry.agent_id
            )
        };
        warnings.push((died_at, text));
    }
    if warnings.is_empty() {
        return Vec::new();
    }
    warnings.sort_by(|a, b| b.0.cmp(&a.0));
    let total = warnings.len();
    let visible = warnings.into_iter().take(limit).collect::<Vec<_>>();
    let visible_len = visible.len();
    let mut lines = vec![
        String::new(),
        "## Dead workers".to_string(),
        String::new(),
        format!("Showing {visible_len} of {total} death warning(s), newest death first."),
        String::new(),
    ];
    lines.extend(visible.into_iter().map(|(_, text)| text));
    lines.extend(render_omission(total - visible_len, "death warnings"));
    lines
}

fn render_omission(omitted: usize, noun: &str) -> Vec<String> {
    if omitted == 0 {
        return Vec::new();
    }
    vec![
        String::new(),
        format!(
            "{omitted} earlier {noun} omitted; call TowerStatus with a larger limit to inspect them."
        ),
    ]
}

fn is_stopped_by_user(entry: &TowerRosterEntry) -> bool {
    let reason = user_cancellation_reason().to_string();
    entry.death_reason.as_deref().map(str::trim) == Some(reason.as_str())
}
