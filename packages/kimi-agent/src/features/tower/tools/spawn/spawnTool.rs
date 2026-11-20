use std::path::Path;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::task::task_service::is_agent_task_terminal;
use crate::agent::task::types::AgentTaskStatus;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::features::tower::protocol::frontmatter::parse_frontmatter;
use crate::features::tower::protocol::paths::{
    MISSIONS_DIR, TOWER_NAME, WORKTREES_DIR, is_reserved_tower_agent_name, mission_file_name,
};
use crate::features::tower::protocol::store::{
    TowerMissionPatch, TowerStore, TowerUpdateMissionOptions, resolve_mission_by_branch,
};
use crate::features::tower::protocol::types::{
    TowerAgentKind, TowerInboxItem, TowerMission, TowerMissionKind, TowerMissionStatus,
    TowerRosterEntry, TowerState,
};
use crate::features::tower::tools::plan::planTool::IAgentTowerService;
use crate::features::tower::tools::support::{
    TOWER_MAIN_AGENT_ONLY, TOWER_MODE_USER_ENABLED_ONLY, new_tower_store, run_tower_tool,
};
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::spawn::{TowerSpawnToolInput, tower_spawn_tool_input_schema};

pub const TOWER_SPAWN_TOOL_NAME: &str = "TowerSpawn";

const REVIEW_REQUEST_SCAN_LIMIT: usize = 50;
const REVIEW_HISTORY_ROUNDS: usize = 5;
const REVIEW_EXCERPT_MAX_CHARS: usize = 8000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerSpawnBinding {
    pub model: String,
    pub model_source: Option<String>,
    pub thinking: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TowerSpawnRequest {
    pub prompt: String,
    pub description: String,
    pub tool_call_id: String,
    pub caller_agent_id: String,
}

#[derive(Debug, Clone)]
pub struct TowerSpawnSettled {
    pub status: AgentTaskStatus,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TowerSpawnLaunched {
    pub agent_id: String,
    pub task_id: String,
    pub settled: Option<TowerSpawnSettled>,
}

pub trait TowerSpawnRunner: Send + Sync {
    fn caller_model_alias(&self) -> Option<String>;
    fn caller_thinking_level(&self) -> Option<String>;
    fn exposes_model_choice(&self) -> bool;
    fn model_descriptions(&self) -> Option<String>;
    fn resolve_binding(
        &self,
        model_alias: &str,
        thinking_level: Option<&str>,
        requested: Option<&str>,
    ) -> Result<TowerSpawnBinding, String>;
    fn launch(
        &self,
        request: TowerSpawnRequest,
        binding: Option<TowerSpawnBinding>,
    ) -> BoxFuture<'static, Result<TowerSpawnLaunched, String>>;
    fn abort(&self, agent_id: &str);
}

pub trait TowerSpawnRateLimit: Send + Sync {
    fn acquire(&self) -> Result<(), String>;
    fn release(&self);
}

#[derive(Clone)]
pub struct TowerSpawnTool {
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
    tower: Arc<dyn IAgentTowerService>,
    rate_limit: Arc<dyn TowerSpawnRateLimit>,
    runner: Arc<dyn TowerSpawnRunner>,
    description: String,
    parameters: Map<String, Value>,
}

impl TowerSpawnTool {
    pub fn new(
        session_context: Arc<dyn ISessionContext>,
        scope_context: Arc<dyn IAgentScopeContext>,
        tower: Arc<dyn IAgentTowerService>,
        rate_limit: Arc<dyn TowerSpawnRateLimit>,
        runner: Arc<dyn TowerSpawnRunner>,
    ) -> Self {
        let description = match runner.model_descriptions() {
            Some(model_lines) => format!("{}\n\n{model_lines}", include_str!("spawn.md")),
            None => include_str!("spawn.md").to_string(),
        };
        let mut parameters = tower_spawn_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerSpawn input schema is an object");
        if !runner.exposes_model_choice() {
            parameters.remove("model");
        }
        Self {
            session_context,
            scope_context,
            tower,
            rate_limit,
            runner,
            description,
            parameters,
        }
    }

    pub fn name(&self) -> &'static str {
        TOWER_SPAWN_TOOL_NAME
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn parameters(&self) -> Map<String, Value> {
        self.parameters.clone()
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerSpawnToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerSpawn: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerSpawnToolInput) -> ToolExecution {
        if self.scope_context.agent_id() != MAIN_AGENT_ID {
            return ToolExecution::Error(ExecutableToolResult::error(TOWER_MAIN_AGENT_ONLY));
        }
        let description = format!("Spawning tower {} \"{}\"", args.kind.as_str(), args.name);
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: TOWER_SPAWN_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |context| {
                let this = this.clone();
                let args = args.clone();
                let tool_call_id = context.tool_call_id.clone();
                Box::pin(async move { Ok(this.execute(args, tool_call_id).await) })
            }),
        })
    }

    async fn execute(
        &self,
        args: TowerSpawnToolInput,
        tool_call_id: String,
    ) -> ExecutableToolResult {
        run_tower_tool(async move {
            if !self.tower.is_active() {
                return Ok(ExecutableToolResult::error(TOWER_MODE_USER_ENABLED_ONLY));
            }
            let store = new_tower_store(&self.session_context.cwd());
            let session_id = self.session_context.session_id();
            let state = store.load().await?;
            if args.name.trim().is_empty() || args.name.trim() != args.name {
                return Ok(ExecutableToolResult::error(format!(
                    "tower agent name \"{}\" must not be blank or carry surrounding whitespace",
                    args.name
                )));
            }
            if is_reserved_tower_agent_name(&args.name) {
                return Ok(ExecutableToolResult::error(format!(
                    "tower agent name \"{}\" is reserved by the tower protocol — pick a different name",
                    args.name
                )));
            }
            if let Some(existing) = store.find_by_name(&state, &args.name) {
                let next = if existing.kind == TowerAgentKind::Worker {
                    format!(
                        "resume it instead of spawning a duplicate: Agent(resume=\"{}\", run_in_background=true, prompt=\"...\") — foreground is only the fallback when background task tools are unavailable",
                        existing.agent_id
                    )
                } else {
                    "a reviewer is one-shot: diagnose the previous round and spawn a fresh reviewer with a new name instead of resuming a roster agent".to_string()
                };
                return Ok(ExecutableToolResult::error(format!(
                    "tower agent \"{}\" is already registered (agent_id: {}, kind: {}) — {next}",
                    args.name,
                    existing.agent_id,
                    existing.kind.as_str()
                )));
            }
            let mut mission: Option<TowerMission> = None;
            let mut review_target: Option<String> = None;
            if args.kind == TowerAgentKind::Worker {
                let Some(mission_id) = args.mission_id.as_deref() else {
                    return Ok(ExecutableToolResult::error("worker spawns require mission_id"));
                };
                let Some(found) = state
                    .missions
                    .iter()
                    .find(|candidate| candidate.id == mission_id)
                    .cloned()
                else {
                    let known = state
                        .missions
                        .iter()
                        .map(|candidate| candidate.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let known = if known.is_empty() {
                        "(none planned yet)".to_string()
                    } else {
                        known
                    };
                    return Ok(ExecutableToolResult::error(format!(
                        "unknown mission \"{mission_id}\" — known missions: {known}"
                    )));
                };
                if found.status == TowerMissionStatus::Merged
                    || found.status == TowerMissionStatus::Abandoned
                {
                    return Ok(ExecutableToolResult::error(format!(
                        "mission {} is {} — terminal missions cannot spawn workers; no worktree or agent was created",
                        found.id,
                        found.status.as_str()
                    )));
                }
                mission = Some(found);
            } else {
                let Some(target) = args.review_target.as_deref() else {
                    return Ok(ExecutableToolResult::error(
                        "reviewer spawns require review_target",
                    ));
                };
                review_target = Some(target.to_string());
            }
            let lease_branch = mission
                .as_ref()
                .map(|mission| mission.branch.clone())
                .or_else(|| review_target.clone());
            let Some(lease_branch) = lease_branch else {
                return Err("tower spawn could not resolve the branch lease".to_string());
            };
            let own_model_alias = self.runner.caller_model_alias();
            if args.model.is_some() && own_model_alias.is_none() {
                return Ok(ExecutableToolResult::error("Caller agent has no model bound"));
            }
            let binding = match &own_model_alias {
                None => None,
                Some(model_alias) => {
                    let thinking_level = self.runner.caller_thinking_level();
                    match self.runner.resolve_binding(
                        model_alias,
                        thinking_level.as_deref(),
                        args.model.as_deref(),
                    ) {
                        Ok(binding) => Some(binding),
                        Err(error) => return Ok(ExecutableToolResult::error(error)),
                    }
                }
            };
            let _lease = self.tower.acquire_branch_lease(&lease_branch)?;
            let mut notes: Vec<String> = Vec::new();
            if let Some(current) = mission.as_mut() {
                let added = store
                    .add_worktree(&current.worktree, &current.branch, &state.base)
                    .await?;
                if let Some(spawn_base) = added.spawn_base {
                    store
                        .update_mission(
                            TOWER_NAME,
                            &current.id,
                            TowerMissionPatch {
                                spawn_base: Some(spawn_base.clone()),
                                ..Default::default()
                            },
                            TowerUpdateMissionOptions { silent: true },
                        )
                        .await?;
                    current.spawn_base = Some(spawn_base.clone());
                    notes.push(format!(
                        "base snapshot: {} — the base checkout had uncommitted changes; they are committed as the branch's first commit (the checkout itself was left untouched)",
                        short_commit(&spawn_base)
                    ));
                }
            }
            let review_mission = review_target
                .as_deref()
                .and_then(|target| resolve_mission_by_branch(&state, target));
            let prompt = build_prompt(
                &args,
                &store,
                &state,
                mission.as_ref(),
                review_target.as_deref(),
                review_mission.as_ref(),
            )
            .await?;
            let description = match mission.as_ref() {
                Some(mission) => format!("{} {}: {}", mission.id, args.name, mission.title),
                None => match review_mission.as_ref() {
                    Some(review_mission) => format!(
                        "{} review: {}",
                        review_mission.id,
                        review_target.as_deref().unwrap_or("")
                    ),
                    None => format!(
                        "review {}: {}",
                        args.name,
                        review_target.as_deref().unwrap_or("")
                    ),
                },
            };
            self.rate_limit.acquire()?;
            let launched = match self
                .runner
                .launch(
                    TowerSpawnRequest {
                        prompt,
                        description,
                        tool_call_id,
                        caller_agent_id: self.scope_context.agent_id(),
                    },
                    binding.clone(),
                )
                .await
            {
                Ok(launched) => launched,
                Err(error) => {
                    self.rate_limit.release();
                    return Ok(ExecutableToolResult::error(format!(
                        "tower spawn failed: {error}"
                    )));
                }
            };
            if let Some(settled) = &launched.settled {
                if is_agent_task_terminal(settled.status)
                    && settled.status != AgentTaskStatus::Completed
                {
                    store
                        .mark_agent_died(
                            &launched.agent_id,
                            settled.status.as_str(),
                            settled.stop_reason.as_deref(),
                            Some(&session_id),
                        )
                        .await?;
                }
            }
            let entry = TowerRosterEntry {
                name: args.name.clone(),
                agent_id: launched.agent_id.clone(),
                session_id: Some(session_id.clone()),
                kind: args.kind,
                mission_id: mission.as_ref().map(|mission| mission.id.clone()),
                review_target: review_target.clone(),
                review_mission_id: review_mission
                    .as_ref()
                    .map(|mission| mission.id.clone()),
                worktree: mission.as_ref().map(|mission| mission.worktree.clone()),
                branch: mission.as_ref().map(|mission| mission.branch.clone()),
                spawned_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                last_inbox_read_at: None,
                inbox_ack_ids: None,
                died_at: None,
                death_status: None,
                death_reason: None,
                status: None,
            };
            let mission_owner = mission
                .as_ref()
                .map(|mission| (mission.id.clone(), args.name.clone()));
            if let Err(error) = store.register_spawned_agent(entry, mission_owner).await {
                self.runner.abort(&launched.agent_id);
                return Ok(ExecutableToolResult::error(error));
            }
            let log_ref = mission.as_ref().map(|mission| {
                format!(
                    "{MISSIONS_DIR}/{}",
                    mission_file_name(&mission.id, &mission.slug)
                )
            });
            if let Err(error) = store
                .append_log(
                    TOWER_NAME,
                    "spawn",
                    &[
                        ("name", Some(args.name.as_str())),
                        ("kind", Some(args.kind.as_str())),
                        ("agent", Some(launched.agent_id.as_str())),
                        ("mission", mission.as_ref().map(|mission| mission.id.as_str())),
                        ("target", review_target.as_deref()),
                        ("model", binding.as_ref().map(|binding| binding.model.as_str())),
                    ],
                    log_ref.as_deref(),
                )
                .await
            {
                self.runner.abort(&launched.agent_id);
                return Ok(ExecutableToolResult::error(error));
            }
            let mut lines = vec![
                format!("name: {}", args.name),
                format!("kind: {}", args.kind.as_str()),
                format!("agent_id: {}", launched.agent_id),
                format!("task_id: {}", launched.task_id),
                "status: running".to_string(),
            ];
            if let Some(binding) = &binding {
                lines.push(format!("model: {}", binding.model));
            }
            match mission.as_ref() {
                Some(mission) => {
                    lines.push(format!("mission: {} — {}", mission.id, mission.title));
                    lines.push(format!("branch: {}", mission.branch));
                    lines.push(format!(
                        "worktree: {}",
                        store
                            .abs(&format!("{WORKTREES_DIR}/{}", mission.worktree))
                            .to_string_lossy()
                    ));
                }
                None => lines.push(format!(
                    "review_target: {}",
                    review_target.as_deref().unwrap_or("")
                )),
            }
            lines.extend(notes);
            lines.push(String::new());
            lines.push(if args.kind == TowerAgentKind::Worker {
                format!(
                    "The worker runs detached in the background; its completion arrives as a notification. Use TowerInbox/TowerStatus only when a decision needs them. If it dies, diagnose first: check why it died (the died entry's status/reason, its task state) before reviving. Resume with Agent(resume=\"{}\", run_in_background=true, prompt=\"...\") (foreground only when background task tools are unavailable) or reassign its work only for a transient cause (lost contact, timeout, OOM); a systematic cause (code or environment defect) is fixed or escalated to the human before any revive. A user-stopped worker is never resumed, reassigned, or replaced.",
                    launched.agent_id
                )
            } else {
                "The reviewer runs detached in the background; its completion arrives as a notification. If it dies, diagnose first: check the death status/reason and task state. A reviewer is one-shot — do not resume it or another roster agent; after diagnosis, spawn a fresh reviewer only if the mission still needs one. A user-stopped reviewer is never replaced.".to_string()
            });
            Ok(ExecutableToolResult {
                output: lines.join("\n").into(),
                ..Default::default()
            })
        })
        .await
    }
}

impl ExecutableTool for TowerSpawnTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Read)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        TowerSpawnTool::name(self)
    }

    fn description(&self) -> &str {
        TowerSpawnTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerSpawnTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}

async fn build_prompt(
    args: &TowerSpawnToolInput,
    store: &TowerStore,
    state: &TowerState,
    mission: Option<&TowerMission>,
    review_target: Option<&str>,
    target_mission: Option<&TowerMission>,
) -> Result<String, String> {
    let extra = match args.instructions.as_deref() {
        Some(instructions) if !instructions.trim().is_empty() => format!(
            "\n\n# Additional instructions from the tower\n{}",
            instructions.trim()
        ),
        _ => String::new(),
    };
    if let Some(mission) = mission {
        let mission_text = read_mission_text(store, mission).await?;
        let worktree_abs = store
            .abs(&format!("{WORKTREES_DIR}/{}", mission.worktree))
            .to_string_lossy()
            .to_string();
        let spawn_base_line = match mission.spawn_base.as_deref() {
            Some(spawn_base) => format!(
                "- Your branch starts from snapshot commit {}: the base checkout's uncommitted changes (WIP), captured at spawn so you can build on them. That commit is your foundation — never revert, amend, or claim it as your own work; your own commits go on top of it.\n",
                short_commit(spawn_base)
            ),
            None => String::new(),
        };
        let scope_line = if mission.kind == TowerMissionKind::Survey {
            format!(
                "- Scope — what you investigate (read-only; reserves nothing): {}\n\n",
                mission.scope.join(", ")
            )
        } else {
            format!(
                "- Scope — the only files you may change: {}\n\n",
                mission.scope.join(", ")
            )
        };
        let workplace = format!(
            "# Your workplace\n- Your private git worktree: {worktree_abs}\n- Your branch: {} (base: {})\n{spawn_base_line}- Your working directory is the main checkout, NOT your worktree — address the worktree explicitly: every Read/Write/Edit/Grep/Glob path must be absolute and under {worktree_abs}, and every Bash command must `cd {worktree_abs}` first. A permission guard hard-denies any Write/Edit outside it; reads are unrestricted — you may read the main checkout ({}) or another agent's worktree when coordination calls for it.\n{scope_line}",
            mission.branch, state.base, store.repo_root.to_string_lossy()
        );
        if mission.kind == TowerMissionKind::Survey {
            return Ok(format!(
                "You are \"{}\", a tower worker agent in a multi-agent workspace, assigned a READ-ONLY survey mission.\n\n{workplace}# Your mission\n\n{}\n\n# Read-only discipline\n- Your scope marks what you investigate, not what you may change. You MUST NOT modify, add, or delete any file in the repo, and your branch must end with zero commits — a changed file makes the merge gate reject your mission as a read-only violation.\n- Your deliverables are knowledge: record findings as TowerMission notes, send summaries to the tower and to dependent agents with TowerSend, and file TowerFinding for out-of-scope discoveries.\n\n# Communication protocol\n- Ambiguity is escalated, not guessed: if the mission leaves substantive doubt about what to investigate, TowerSend(to=\"tower\", subject=\"clarify-request\", body=what needs pinning down) BEFORE acting — the tower relays to the human; you never ask the user directly.\n\n# When the survey is done\n1. Call TowerInbox once and fold anything new into your summary — TowerComplete refuses while unread messages wait in your inbox.\n2. Call TowerComplete(report=the full survey result). It records completed and stores survey-summary to the tower in one protocol flow — do not substitute a TowerMission status patch or a manual TowerSend.\n3. Finish with a structured final summary: what you covered, key facts with file:line references, open questions.{extra}",
                args.name,
                mission_text.trim()
            ));
        }
        let history_section = worker_history_section(store, mission, &args.name).await?;
        let artifact = mission.kind == TowerMissionKind::Artifact;
        let ambiguity = if artifact {
            "- Ambiguity is escalated, not guessed: if the mission and its Context leave substantive doubt about what to deliver, TowerSend(to=\"tower\", subject=\"clarify-request\", body=what needs pinning down) BEFORE acting — the tower relays to the human; you never ask the user directly.\n\n"
        } else {
            "- Ambiguity is escalated, not guessed: if the mission and its Context leave substantive doubt about what to build, TowerSend(to=\"tower\", subject=\"clarify-request\", body=what needs pinning down) BEFORE acting — the tower relays to the human; you never ask the user directly.\n\n"
        };
        let step1 = if artifact {
            "1. `git add` + `git commit` every artifact needed for acceptance in the worktree, inside your scope. Uncommitted files are excluded from acceptance and merge.\n"
        } else {
            "1. `git add` + `git commit` your mission's changes in the worktree (source files only — no build outputs).\n"
        };
        let step3 = if artifact {
            "3. Call TowerComplete(report=what you delivered and why, reconciled against the mission tasks item by item, key_artifacts=[the committed paths of the key deliverables the human should inspect]). Every key path must be committed, exist as a file at the current tip, and stay inside your scope, but the list does not need to cover the whole diff. It records completed and stores acceptance-request to the tower in one protocol flow — do not substitute a TowerMission status patch or a manual TowerSend. The tower will present those key artifacts and explain that approval authorizes the entire tip, including additional committed files; no independent reviewer is required. If you commit anything after TowerComplete, run it again with the updated report and key_artifacts before the tower requests acceptance — acceptance is bound to the last completion tip.\n"
        } else {
            "3. Call TowerComplete(report=what you changed and why, reconciled against the mission tasks item by item — the reviewer maps each task to your diff). It records completed and stores review-request to the tower in one protocol flow — do not substitute a TowerMission status patch or a manual TowerSend.\n"
        };
        let step4 = if artifact {
            "4. Finish with a structured final summary: key artifact paths, key decisions, open follow-ups."
        } else {
            "4. Finish with a structured final summary: files changed, key decisions, open follow-ups."
        };
        return Ok(format!(
            "You are \"{}\", a tower worker agent in a multi-agent workspace.\n\n{workplace}# Your mission\n\n{}\n\n{history_section}# Communication protocol\n- Found something notable outside your scope? File it with TowerFinding instead of fixing it.\n- Keep your mission current with TowerMission: task_done as you finish tasks, note for decisions, blocker when stuck.\n{ambiguity}# When the mission is done\n{step1}2. Before completing, call TowerInbox once and incorporate anything new into the delivery — TowerComplete refuses while unread messages wait in your inbox. Re-read your mission too (TowerMission(id=\"{}\") with no patch fields): the tower may have changed its tasks mid-flight.\n{step3}{step4}{extra}",
            args.name,
            mission_text.trim(),
            mission.id
        ));
    }
    let target = review_target.unwrap_or("");
    let author = target_mission.and_then(|mission| mission.owner.as_deref());
    let review_base = match target_mission {
        Some(mission) => store.diff_base(state, mission).await?,
        None => state.base.clone(),
    };
    let mission_section = match target_mission {
        Some(mission) => format!(
            "# Mission under review — verify the diff against this intent, not only against code health\n\n{}\n\n",
            read_mission_text(store, mission).await?.trim()
        ),
        None => String::new(),
    };
    let completion_report = match author {
        Some(author) => {
            find_completion_report(
                store,
                author,
                if target_mission.map(|mission| mission.kind) == Some(TowerMissionKind::Artifact)
                {
                    "acceptance-request"
                } else {
                    "review-request"
                },
            )
            .await?
        }
        None => None,
    };
    let self_report_section = match &completion_report {
        Some(report) => format!(
            "# The author's own account (their {} to the tower)\n{}\n\n",
            if target_mission.map(|mission| mission.kind) == Some(TowerMissionKind::Artifact) {
                "acceptance-request"
            } else {
                "review-request"
            },
            report.body.trim()
        ),
        None => String::new(),
    };
    let history_section = review_history_section(store, target).await?;
    let checklist = if target_mission.is_some() {
        "1. Intent — does the diff deliver the mission above? Map every task to the changes; healthy code that answers the wrong requirement or silently drops a task is a finding, not a pass.\n2. Security\n3. Data integrity\n4. Performance\n5. Error handling\n6. Code quality\n\n"
    } else {
        "1. Security\n2. Data integrity\n3. Performance\n4. Error handling\n5. Code quality\n\n"
    };
    let verdict = if target_mission.map(|mission| mission.kind) == Some(TowerMissionKind::Artifact)
    {
        format!(
            "1. Submit your verdict with TowerReview: {{ target: \"{target}\", status: \"clean\" | \"p1-Nitems\" | \"p2-Nitems\", merge: \"merge\" | \"fix-then-merge\" | \"hold\", findings, checks, decision }}. This independent review is advisory only: it cannot replace explicit human acceptance recorded by reviewer=tower on the exact branch tip. Standard review-result notifications are stored automatically — do not send a second manual TowerSend.\n"
        )
    } else {
        format!(
            "1. Submit your verdict with TowerReview: {{ target: \"{target}\", status: \"clean\" | \"p1-Nitems\" | \"p2-Nitems\", merge: \"merge\" | \"fix-then-merge\" | \"hold\", findings, checks, decision }}. Only a clean review with merge verdict \"merge\" on the exact branch tip lets the tower merge. Standard review-result notifications are stored automatically — do not send a second manual TowerSend.\n"
        )
    };
    Ok(format!(
        "You are \"{}\", a tower reviewer agent in a multi-agent workspace.\n\n# Your assignment\nReview branch \"{target}\" against base \"{review_base}\".\n- Work read-only in the main checkout ({}): `git diff {review_base}...{target}`, `git log {review_base}..{target}`, and read files as needed.\n- Do NOT modify any code.\n\n{mission_section}{self_report_section}{history_section}# Review checklist (in priority order)\n{checklist}# When done — TowerReview is the only mandatory protocol step\n{verdict}Then finish with a structured summary of the review.{extra}",
        args.name,
        store.repo_root.to_string_lossy()
    ))
}

async fn worker_history_section(
    store: &TowerStore,
    mission: &TowerMission,
    agent_name: &str,
) -> Result<String, String> {
    let latest = store.latest_review(&mission.branch).await?;
    let predecessor = mission
        .owner
        .as_deref()
        .filter(|owner| *owner != agent_name);
    let completion_report = match predecessor {
        Some(predecessor) => {
            find_completion_report(
                store,
                predecessor,
                if mission.kind == TowerMissionKind::Artifact {
                    "acceptance-request"
                } else {
                    "review-request"
                },
            )
            .await?
        }
        None => None,
    };
    if latest.is_none() && completion_report.is_none() {
        return Ok(String::new());
    }
    let mut parts = vec![
        "# Previous work on this branch".to_string(),
        if mission.kind == TowerMissionKind::Artifact {
            "An earlier worker already ran on this branch — continue from its work instead of starting over, and address the latest human feedback below rather than re-opening what it already settled.".to_string()
        } else {
            "An earlier worker already ran on this branch — continue from its work instead of starting over, and address the review verdict below rather than re-opening what it already settled.".to_string()
        },
    ];
    if let Some(latest) = &latest {
        parts.push(if mission.kind == TowerMissionKind::Artifact {
            format!(
                "## Latest feedback — round {} by {}: {}, merge verdict \"{}\"",
                latest.round, latest.reviewer, latest.status, latest.merge
            )
        } else {
            format!(
                "## Latest review — round {} by {}: {}, merge verdict \"{}\"",
                latest.round, latest.reviewer, latest.status, latest.merge
            )
        });
        parts.push(review_excerpt(store, &latest.file).await?);
    }
    if let Some(report) = &completion_report {
        parts.push(if mission.kind == TowerMissionKind::Artifact {
            "## The previous worker's acceptance-request to the tower".to_string()
        } else {
            "## The previous worker's review-request to the tower".to_string()
        });
        parts.push(truncate_section(
            report.body.trim(),
            REVIEW_EXCERPT_MAX_CHARS,
        ));
    }
    Ok(format!("{}\n\n", parts.join("\n\n")))
}

async fn review_history_section(store: &TowerStore, target: &str) -> Result<String, String> {
    let reviews = store.reviews_for(target).await?;
    if reviews.is_empty() {
        return Ok(String::new());
    }
    let start = reviews.len().saturating_sub(REVIEW_HISTORY_ROUNDS);
    let recent = &reviews[start..];
    let omitted = reviews.len() - recent.len();
    let mut parts = vec![
        "# Review history on this branch".to_string(),
        "Earlier review rounds already ran on this branch — verify their findings against the current tip instead of reporting them again; re-report only what still reproduces.".to_string(),
    ];
    if omitted > 0 {
        parts.push(format!(
            "({omitted} earlier round(s) omitted — the full record lives in .tower/comms/reviews/.)"
        ));
    }
    for review in recent {
        parts.push(format!(
            "## Round {} by {}: {}, merge verdict \"{}\"",
            review.round, review.reviewer, review.status, review.merge
        ));
        parts.push(review_excerpt(store, &review.file).await?);
    }
    Ok(format!("{}\n\n", parts.join("\n\n")))
}

async fn find_completion_report(
    store: &TowerStore,
    author: &str,
    subject: &str,
) -> Result<Option<TowerInboxItem>, String> {
    Ok(store
        .read_inbox(TOWER_NAME, REVIEW_REQUEST_SCAN_LIMIT)
        .await?
        .into_iter()
        .find(|item| item.from == author && item.subject.starts_with(subject)))
}

async fn review_excerpt(store: &TowerStore, file: &str) -> Result<String, String> {
    let text = tokio::fs::read_to_string(store.abs(file))
        .await
        .map_err(|error| error.to_string())?;
    let (_, body) = parse_frontmatter(&text);
    Ok(truncate_section(body.trim(), REVIEW_EXCERPT_MAX_CHARS))
}

async fn read_mission_text(store: &TowerStore, mission: &TowerMission) -> Result<String, String> {
    tokio::fs::read_to_string(store.abs(&format!(
        "{MISSIONS_DIR}/{}",
        mission_file_name(&mission.id, &mission.slug)
    )))
    .await
    .map_err(|error| error.to_string())
}

fn truncate_section(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    format!(
        "{}\n…(truncated)",
        text.chars().take(max_chars).collect::<String>()
    )
}

fn short_commit(commit: &str) -> &str {
    if commit.len() >= 7 {
        &commit[..7]
    } else {
        commit
    }
}
