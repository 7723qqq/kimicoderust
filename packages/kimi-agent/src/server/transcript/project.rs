use std::collections::HashMap;

use serde_json::Value;

use super::model::{
    ActivityMeta, AgentPermission, AgentStatusMeta, AgentUsageMeta, AttachmentSource,
    InteractionKind, InteractionState, StepState, StepTiming, StepUsage, TaskKind, TaskState,
    TextFrame, TextRole, ThinkingFrame, ToolCallFrame, ToolFrameProgress, ToolFrameState,
    ToolProgressKind, TranscriptAttachment, TranscriptFrame, TranscriptInteraction, TranscriptItem,
    TranscriptMarker, TranscriptMeta, TranscriptMetaMerge, TranscriptPrompt,
    TranscriptPromptStatus, TranscriptStep, TranscriptTask, TranscriptTurn, TranscriptUserOrigin,
    TurnOrigin, TurnState, UserOriginKind,
};
use super::ops::{
    AgentTranscriptSnapshot, AppendTarget, StepHeader, StepKind, TranscriptOperation, TurnHeader,
    TurnKind,
};
use crate::events::EngineEvent;

#[derive(Debug, Clone, Copy)]
struct Cursor {
    turn: usize,
    step: usize,
    frame: Option<usize>,
}

pub struct TranscriptProjector {
    turns: Vec<TranscriptTurn>,
    turn_indices: HashMap<String, usize>,
    tasks: Vec<TranscriptTask>,
    interactions: Vec<TranscriptInteraction>,
    prompts: Vec<TranscriptPrompt>,
    attachments: Vec<TranscriptAttachment>,
    meta: TranscriptMeta,
    cursor: Option<Cursor>,

    pending_steers: Vec<PendingSteer>,

    spawning_turns: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct PendingSteer {
    text: String,
    prompt_ids: Option<Vec<String>>,
}

fn is_terminal_prompt_status(status: TranscriptPromptStatus) -> bool {
    matches!(
        status,
        TranscriptPromptStatus::Completed
            | TranscriptPromptStatus::Failed
            | TranscriptPromptStatus::Aborted
            | TranscriptPromptStatus::Blocked
    )
}

impl TranscriptProjector {
    pub fn new() -> Self {
        Self {
            turns: Vec::new(),
            turn_indices: HashMap::new(),
            tasks: Vec::new(),
            interactions: Vec::new(),
            prompts: Vec::new(),
            attachments: Vec::new(),
            meta: TranscriptMeta::default(),
            cursor: None,
            pending_steers: Vec::new(),
            spawning_turns: HashMap::new(),
        }
    }

    pub fn apply_event(&mut self, event: &EngineEvent) -> Vec<TranscriptOperation> {
        self.apply_event_at(event, None)
    }

    pub fn apply_event_at(
        &mut self,
        event: &EngineEvent,
        at: Option<i64>,
    ) -> Vec<TranscriptOperation> {
        match event {
            EngineEvent::TurnStarted {
                turn_id, prompt, ..
            } => {
                let key = turn_key(turn_id);

                let mut ops = Vec::new();
                if let Some(prev_idx) = self
                    .turns
                    .iter()
                    .rposition(|turn| turn.state == TurnState::Running && turn.turn_id != key)
                {
                    let prev = &mut self.turns[prev_idx];
                    prev.state = TurnState::Failed;
                    prev.wedged = Some(true);
                    prev.ended_at = Some(at.map(epoch_ms_iso).unwrap_or_else(now_iso));
                    ops.push(TranscriptOperation::TurnUpsert {
                        turn: turn_header(prev),
                    });
                }
                let turn_idx = self.ensure_turn(&key, at);
                self.turns[turn_idx].state = TurnState::Running;
                self.turns[turn_idx].started_at =
                    Some(at.map(epoch_ms_iso).unwrap_or_else(now_iso));
                if let Some(prompt) = prompt
                    && !prompt.is_empty()
                {
                    self.turns[turn_idx].prompt = Some(prompt.clone());
                }
                self.cursor = Some(Cursor {
                    turn: turn_idx,
                    step: 0,
                    frame: None,
                });
                ops.push(TranscriptOperation::TurnUpsert {
                    turn: turn_header(&self.turns[turn_idx]),
                });
                ops
            }
            EngineEvent::LlmStepBegin { turn_id, step, .. } => {
                let turn_idx = self.ensure_turn(&turn_key(turn_id), at);
                let step_idx = self.ensure_step(turn_idx, i64::from(*step));
                self.cursor = Some(Cursor {
                    turn: turn_idx,
                    step: step_idx,
                    frame: None,
                });
                let mut ops = vec![TranscriptOperation::StepUpsert {
                    turn_id: self.turns[turn_idx].turn_id.clone(),
                    step: step_header(&self.turns[turn_idx].steps[step_idx]),
                }];

                ops.extend(self.flush_pending_steers());
                ops
            }
            EngineEvent::AssistantDelta { turn_id, delta, .. } => {
                let turn_idx = self.ensure_turn(&turn_key(turn_id), at);
                self.push_delta(turn_idx, delta, false)
            }
            EngineEvent::ThinkingDelta { turn_id, delta, .. } => {
                let turn_idx = self.ensure_turn(&turn_key(turn_id), at);
                self.push_delta(turn_idx, delta, true)
            }
            EngineEvent::ToolCallStarted {
                turn_id,
                tool_call_id,
                name,
                args,
                ..
            } => {
                let turn_idx = self.ensure_turn(&turn_key(turn_id), at);
                if let Some((step_idx, frame_idx)) = self.find_tool_frame(turn_idx, tool_call_id) {
                    if let TranscriptFrame::Tool(frame) =
                        &mut self.turns[turn_idx].steps[step_idx].frames[frame_idx]
                    {
                        frame.name = name.clone();
                        frame.input = Some(args.clone());
                        frame.state = ToolFrameState::Running;
                    }
                    self.cursor = Some(Cursor {
                        turn: turn_idx,
                        step: step_idx,
                        frame: Some(frame_idx),
                    });
                    return vec![self.tool_frame_op(turn_idx, step_idx, frame_idx)];
                }
                let step_idx = self.step_index_for_turn(turn_idx);
                let step_id = self.turns[turn_idx].steps[step_idx].step_id.clone();
                let frame = TranscriptFrame::Tool(ToolCallFrame {
                    frame_id: format!("{step_id}.{tool_call_id}"),
                    tool_call_id: tool_call_id.clone(),
                    name: name.clone(),
                    view: None,
                    state: ToolFrameState::Running,
                    input: Some(args.clone()),
                    output: None,
                    display: None,
                    error: None,
                    input_text: None,
                    progress: None,
                    task_id: None,
                    approval_id: None,
                    todo_id: None,
                    agent_refs: None,
                });
                self.turns[turn_idx].steps[step_idx].frames.push(frame);
                let frame_idx = self.turns[turn_idx].steps[step_idx].frames.len() - 1;
                self.cursor = Some(Cursor {
                    turn: turn_idx,
                    step: step_idx,
                    frame: Some(frame_idx),
                });
                vec![self.tool_frame_op(turn_idx, step_idx, frame_idx)]
            }
            EngineEvent::ToolCallCompleted {
                turn_id,
                tool_call_id,
                result,
                ..
            } => {
                let Some(turn_idx) = self.resolve_turn(turn_id) else {
                    return Vec::new();
                };
                let Some((step_idx, frame_idx)) = self.find_tool_frame(turn_idx, tool_call_id)
                else {
                    return Vec::new();
                };
                if let TranscriptFrame::Tool(frame) =
                    &mut self.turns[turn_idx].steps[step_idx].frames[frame_idx]
                {
                    frame.state = ToolFrameState::Done;
                    frame.output = Some(result.clone());
                }
                vec![self.tool_frame_op(turn_idx, step_idx, frame_idx)]
            }
            EngineEvent::ToolCallFailed {
                turn_id,
                tool_call_id,
                error,
                ..
            } => {
                let Some(turn_idx) = self.resolve_turn(turn_id) else {
                    return Vec::new();
                };
                let Some((step_idx, frame_idx)) = self.find_tool_frame(turn_idx, tool_call_id)
                else {
                    return Vec::new();
                };
                if let TranscriptFrame::Tool(frame) =
                    &mut self.turns[turn_idx].steps[step_idx].frames[frame_idx]
                {
                    frame.state = ToolFrameState::Error;
                    frame.error = Some(error.clone());
                }
                vec![self.tool_frame_op(turn_idx, step_idx, frame_idx)]
            }
            EngineEvent::ToolProgress {
                turn_id,
                tool_call_id,
                update,
                ..
            } => {
                let Some(turn_idx) = self.resolve_turn(turn_id) else {
                    return Vec::new();
                };
                let Some((step_idx, frame_idx)) = self.find_tool_frame(turn_idx, tool_call_id)
                else {
                    return Vec::new();
                };
                if let TranscriptFrame::Tool(frame) =
                    &mut self.turns[turn_idx].steps[step_idx].frames[frame_idx]
                {
                    frame.progress = Some(progress_from_update(update));
                }
                vec![self.tool_frame_op(turn_idx, step_idx, frame_idx)]
            }
            EngineEvent::ToolNative {
                turn_id,
                tool_call_id,
                tool_name,
                arguments,
                content,
                is_error,
                ..
            } => {
                let turn_idx = self.ensure_turn(&turn_key(turn_id), at);
                let state = if *is_error {
                    ToolFrameState::Error
                } else {
                    ToolFrameState::Done
                };
                if let Some((step_idx, frame_idx)) = self.find_tool_frame(turn_idx, tool_call_id) {
                    if let TranscriptFrame::Tool(frame) =
                        &mut self.turns[turn_idx].steps[step_idx].frames[frame_idx]
                    {
                        frame.state = state;
                        frame.output = Some(serde_json::json!(content));
                    }
                    self.cursor = Some(Cursor {
                        turn: turn_idx,
                        step: step_idx,
                        frame: Some(frame_idx),
                    });
                    return vec![self.tool_frame_op(turn_idx, step_idx, frame_idx)];
                }
                let step_idx = self.step_index_for_turn(turn_idx);
                let step_id = self.turns[turn_idx].steps[step_idx].step_id.clone();
                let frame = TranscriptFrame::Tool(ToolCallFrame {
                    frame_id: format!("{step_id}.{tool_call_id}"),
                    tool_call_id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    view: None,
                    state,
                    input: Some(arguments.clone()),
                    output: Some(serde_json::json!(content)),
                    display: None,
                    error: None,
                    input_text: None,
                    progress: None,
                    task_id: None,
                    approval_id: None,
                    todo_id: None,
                    agent_refs: None,
                });
                self.turns[turn_idx].steps[step_idx].frames.push(frame);
                let frame_idx = self.turns[turn_idx].steps[step_idx].frames.len() - 1;
                self.cursor = Some(Cursor {
                    turn: turn_idx,
                    step: step_idx,
                    frame: Some(frame_idx),
                });
                vec![self.tool_frame_op(turn_idx, step_idx, frame_idx)]
            }
            EngineEvent::LlmStepEnd {
                turn_id,
                step,
                usage,
                timing,
                ..
            } => {
                let turn_idx = self.ensure_turn(&turn_key(turn_id), at);
                let step_idx = self.ensure_step(turn_idx, i64::from(*step));
                {
                    let step = &mut self.turns[turn_idx].steps[step_idx];
                    step.state = StepState::Completed;
                    step.ended_at = Some(now_iso());
                    if let Some(usage) = usage {
                        step.usage = Some(StepUsage {
                            input_other: usage.input_other,
                            output: usage.output,
                            input_cache_read: usage.input_cache_read,
                            input_cache_creation: usage.input_cache_creation,
                        });
                    }

                    if let Some(timing) = timing {
                        step.timing = Some(StepTiming {
                            llm_first_token_latency_ms: timing.first_token_latency_ms,
                            llm_stream_duration_ms: timing.stream_duration_ms,
                            llm_request_build_ms: timing.request_build_ms,
                            llm_server_first_token_ms: timing.server_first_token_ms,
                            llm_server_decode_ms: timing.server_decode_ms,
                            llm_client_consume_ms: timing.client_consume_ms,
                        });
                    }
                }
                vec![TranscriptOperation::StepUpsert {
                    turn_id: self.turns[turn_idx].turn_id.clone(),
                    step: step_header(&self.turns[turn_idx].steps[step_idx]),
                }]
            }
            EngineEvent::ToolCallDelta {
                tool_call_id,
                arguments_part,
                ..
            } => {
                let Some((turn_idx, step_idx, frame_idx)) = self.find_tool_frame_any(tool_call_id)
                else {
                    return Vec::new();
                };
                if let Some(part) = arguments_part
                    && let TranscriptFrame::Tool(frame) =
                        &mut self.turns[turn_idx].steps[step_idx].frames[frame_idx]
                {
                    frame
                        .input_text
                        .get_or_insert_with(String::new)
                        .push_str(part);
                }
                vec![self.tool_frame_op(turn_idx, step_idx, frame_idx)]
            }
            EngineEvent::TurnEnded {
                turn_id, reason, ..
            } => {
                let turn_idx = self.ensure_turn(&turn_key(turn_id), at);
                let state = match reason.as_str() {
                    "cancelled" => TurnState::Cancelled,
                    "failed" | "blocked" => TurnState::Failed,
                    _ => TurnState::Completed,
                };

                let step_state = match reason.as_str() {
                    "cancelled" => StepState::Interrupted,
                    "failed" | "blocked" => StepState::Failed,
                    _ => StepState::Completed,
                };
                let ended_at = at.map(epoch_ms_iso).unwrap_or_else(now_iso);
                self.turns[turn_idx].state = state;
                self.turns[turn_idx].ended_at = Some(ended_at.clone());
                let turn_id = self.turns[turn_idx].turn_id.clone();
                let mut ops = vec![TranscriptOperation::TurnUpsert {
                    turn: turn_header(&self.turns[turn_idx]),
                }];
                for step in self.turns[turn_idx].steps.iter_mut() {
                    if step.state != StepState::Running {
                        continue;
                    }
                    step.state = step_state;
                    step.ended_at = Some(ended_at.clone());
                    ops.push(TranscriptOperation::StepUpsert {
                        turn_id: turn_id.clone(),
                        step: step_header(step),
                    });
                }
                ops
            }
            EngineEvent::SubagentSpawned { subagent_id, .. } => {
                if let Some(turn) = self
                    .turns
                    .iter()
                    .rev()
                    .find(|t| t.state == TurnState::Running)
                {
                    self.spawning_turns
                        .insert(subagent_id.clone(), turn.turn_id.clone());
                }
                self.subagent_task_ops(subagent_id, TaskState::Running, None, None, None)
            }
            EngineEvent::SubagentCompleted {
                subagent_id,
                result_summary,
                usage,
            } => self.subagent_task_ops(
                subagent_id,
                TaskState::Completed,
                result_summary.clone(),
                None,
                usage.clone().map(rpc_usage_as_step_usage),
            ),
            EngineEvent::SubagentFailed { subagent_id, error } => self.subagent_task_ops(
                subagent_id,
                TaskState::Failed,
                None,
                Some(error.clone()),
                None,
            ),

            EngineEvent::SessionWorkChanged { busy, .. } => self.merge_activity(*busy),
            EngineEvent::Custom(value) => match value.get("type").and_then(|t| t.as_str()) {
                Some("event.session.work_changed") => {
                    let busy = value.get("busy").and_then(|v| v.as_bool()).unwrap_or(false);
                    self.merge_activity(busy)
                }
                Some("task.started")
                | Some("task.terminated")
                | Some("event.task.created")
                | Some("event.task.completed") => match task_from_event(value) {
                    Some(task) => {
                        let task = self.merge_task(task);
                        vec![TranscriptOperation::TaskUpsert { task }]
                    }
                    None => Vec::new(),
                },
                Some("task.output") | Some("event.task.progress") => {
                    let Some(task_id) = value
                        .get("taskId")
                        .or_else(|| value.get("task_id"))
                        .and_then(|v| v.as_str())
                    else {
                        return Vec::new();
                    };
                    let Some(chunk) = value
                        .get("chunk")
                        .or_else(|| value.get("output_chunk"))
                        .and_then(|v| v.as_str())
                    else {
                        return Vec::new();
                    };
                    let Some(idx) = self.tasks.iter().position(|task| task.task_id == task_id)
                    else {
                        return Vec::new();
                    };
                    let offset = self.tasks[idx].output_tail.encode_utf16().count() as u64;
                    self.tasks[idx].output_tail.push_str(chunk);
                    vec![TranscriptOperation::Append {
                        target: AppendTarget::Task {
                            task_id: task_id.to_string(),
                        },
                        offset,
                        text: chunk.to_string(),
                    }]
                }
                Some("event.assistant.delta") => {
                    let Some(delta) = value.get("delta").and_then(|v| v.as_str()) else {
                        return Vec::new();
                    };
                    let Some(turn_idx) = self.custom_delta_turn(value, at) else {
                        return Vec::new();
                    };
                    self.push_delta(turn_idx, delta, false)
                }

                Some("event.thinking.delta") => {
                    let Some(delta) = value.get("delta").and_then(|v| v.as_str()) else {
                        return Vec::new();
                    };
                    let Some(turn_idx) = self.custom_delta_turn(value, at) else {
                        return Vec::new();
                    };
                    self.push_delta(turn_idx, delta, true)
                }
                Some("event.question.requested") => {
                    let Some(id) = value.get("question_id").and_then(|v| v.as_str()) else {
                        return Vec::new();
                    };
                    let interaction = TranscriptInteraction {
                        interaction_id: id.to_string(),
                        interaction_kind: InteractionKind::Question,
                        tool_call_id: value
                            .get("tool_call_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        state: InteractionState::Pending,
                        request: Some(value.clone()),
                        response: None,
                    };
                    vec![self.upsert_interaction(interaction)]
                }
                Some("event.question.answered") | Some("event.question.dismissed") => {
                    let Some(id) = value.get("question_id").and_then(|v| v.as_str()) else {
                        return Vec::new();
                    };
                    let state = if value.get("type").and_then(|v| v.as_str())
                        == Some("event.question.answered")
                    {
                        InteractionState::Answered
                    } else {
                        InteractionState::Dismissed
                    };
                    self.patch_interaction(id, state, Some(value.clone()))
                        .into_iter()
                        .collect()
                }
                Some("event.approval.requested") => {
                    let Some(id) = value.get("approval_id").and_then(|v| v.as_str()) else {
                        return Vec::new();
                    };
                    let interaction = TranscriptInteraction {
                        interaction_id: id.to_string(),
                        interaction_kind: InteractionKind::Approval,
                        tool_call_id: value
                            .get("tool_call_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        state: InteractionState::Pending,
                        request: Some(value.clone()),
                        response: None,
                    };
                    vec![self.upsert_interaction(interaction)]
                }
                Some("event.approval.resolved") => {
                    let Some(id) = value.get("approval_id").and_then(|v| v.as_str()) else {
                        return Vec::new();
                    };
                    let state = match value.get("decision").and_then(|v| v.as_str()) {
                        Some("allow") | Some("approved") => InteractionState::Approved,
                        _ => InteractionState::Rejected,
                    };
                    self.patch_interaction(id, state, Some(value.clone()))
                        .into_iter()
                        .collect()
                }
                Some("event.approval.expired") => {
                    let Some(id) = value.get("approval_id").and_then(|v| v.as_str()) else {
                        return Vec::new();
                    };
                    self.patch_interaction(id, InteractionState::Cancelled, None)
                        .into_iter()
                        .collect()
                }
                Some("event.message.created") => {
                    let Some(message) = value.get("message") else {
                        return Vec::new();
                    };
                    if message.get("role").and_then(|v| v.as_str()) != Some("user") {
                        return Vec::new();
                    }
                    let Some(id) = message.get("id").and_then(|v| v.as_str()) else {
                        return Vec::new();
                    };
                    let announcement_id = id.to_string();
                    let created_at = message
                        .get("created_at")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(now_iso);
                    let mut ops = vec![self.upsert_prompt(id, |prev| TranscriptPrompt {
                        prompt_id: announcement_id.clone(),
                        status: TranscriptPromptStatus::Running,
                        user_message_id: Some(announcement_id.clone()),
                        content: message.get("content").cloned(),

                        client_metadata: prev.and_then(|prev| prev.client_metadata.clone()),
                        created_at,
                        finished_at: None,
                        steered_at: None,
                    })];
                    let mut attachment_ids = Vec::new();
                    if let Some(blocks) = message.get("content").and_then(|v| v.as_array()) {
                        for block in blocks {
                            if let Some(attachment) = attachment_from_block(block) {
                                attachment_ids.push(attachment.attachment_id.clone());
                                ops.push(self.upsert_attachment(attachment));
                            }
                        }
                    }

                    if !attachment_ids.is_empty()
                        && let Some(turn_number) = id
                            .strip_prefix("msg-u")
                            .and_then(|digits| digits.parse::<u64>().ok())
                    {
                        let turn_idx = self.ensure_turn(&turn_key_u64(turn_number), at);
                        self.turns[turn_idx].attachment_ids = Some(attachment_ids);
                    }
                    ops
                }
                Some("agent.status.updated") => self.merge_agent_meta(value),

                Some("prompt.submitted") => {
                    let Some(prompt_id) = value.get("promptId").and_then(|v| v.as_str()) else {
                        return Vec::new();
                    };
                    let status = match value.get("status").and_then(|v| v.as_str()) {
                        Some("queued") => TranscriptPromptStatus::Queued,
                        Some("blocked") => TranscriptPromptStatus::Blocked,
                        Some("completed") => TranscriptPromptStatus::Completed,
                        Some("failed") => TranscriptPromptStatus::Failed,
                        Some("aborted") => TranscriptPromptStatus::Aborted,
                        _ => TranscriptPromptStatus::Running,
                    };

                    let client_metadata = value
                        .get("metadata")
                        .filter(|metadata| metadata.is_object())
                        .map(|metadata| vec![metadata.clone()]);
                    let event_created_at = value
                        .get("createdAt")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let user_message_id = value
                        .get("userMessageId")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let content = value.get("content").cloned();

                    vec![self.upsert_prompt(prompt_id, |prev| {
                        TranscriptPrompt {
                            prompt_id: prompt_id.to_string(),
                            status: match prev {
                                Some(prev) if is_terminal_prompt_status(prev.status) => prev.status,
                                _ => status,
                            },
                            user_message_id,
                            content,
                            client_metadata: client_metadata
                                .or_else(|| prev.and_then(|prev| prev.client_metadata.clone())),
                            created_at: prev
                                .map(|prev| prev.created_at.clone())
                                .or(event_created_at)
                                .unwrap_or_else(now_iso),
                            finished_at: prev.and_then(|prev| prev.finished_at.clone()),
                            steered_at: prev.and_then(|prev| prev.steered_at.clone()),
                        }
                    })]
                }
                Some("llm.step.begin") => self.flush_pending_steers(),
                Some("turn.steer") => self.fold_turn_steer(value),
                Some("prompt.steered") => self.fold_prompt_steered(value),
                Some("prompt.queued") => self.fold_prompt_queued(value),
                Some("prompt.started") => self.fold_prompt_started(value),
                Some("prompt.completed") => self.fold_prompt_completed(value),
                Some("prompt.aborted") => self.fold_prompt_aborted(value),

                Some("hook.result") => vec![TranscriptOperation::MarkerUpsert {
                    item: TranscriptMarker {
                        marker_id: format!("hook-{:016x}", fastrand::u64(..)),
                        marker: "hook".to_string(),
                        payload: Some(value.clone()),
                        at: Some(now_iso()),
                    },
                    before_turn: None,
                }],
                Some("context.compaction")
                | Some("context.apply_compaction")
                | Some("compaction.started")
                | Some("compaction.completed") => vec![TranscriptOperation::MarkerUpsert {
                    item: TranscriptMarker {
                        marker_id: format!("compaction-{:016x}", fastrand::u64(..)),
                        marker: "compaction".to_string(),
                        payload: Some(value.clone()),
                        at: Some(now_iso()),
                    },
                    before_turn: None,
                }],
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    pub fn snapshot(&self) -> AgentTranscriptSnapshot {
        AgentTranscriptSnapshot {
            items: self
                .turns
                .iter()
                .cloned()
                .map(TranscriptItem::Turn)
                .collect(),
            tasks: self.tasks.clone(),
            interactions: self.interactions.clone(),
            attachments: self.attachments.clone(),
            todos: Vec::new(),
            prompts: self.prompts.clone(),
            meta: self.meta.clone(),
            has_more_older: None,
        }
    }

    pub fn cold_snapshot_tasks(&self) -> Vec<TranscriptTask> {
        self.tasks
            .iter()
            .map(|task| {
                let mut task = task.clone();
                if task.kind == TaskKind::Subagent && task.state == TaskState::Running {
                    let spawning_turn_running = task
                        .agent_id
                        .as_deref()
                        .and_then(|agent_id| self.spawning_turns.get(agent_id))
                        .is_some_and(|turn_id| {
                            self.turns.iter().any(|turn| {
                                turn.turn_id == *turn_id && turn.state == TurnState::Running
                            })
                        });
                    if !spawning_turn_running {
                        task.state = TaskState::Lost;
                        task.ended_at = task.ended_at.or_else(|| Some(now_iso()));
                    }
                }
                task
            })
            .collect()
    }

    pub fn mark_wedged_turns(&mut self, active_wire_turn_id: Option<i64>) {
        let Some(tail) = self.turns.iter().map(|turn| turn.ordinal).max() else {
            return;
        };
        let ended_at = now_iso();
        for turn in self.turns.iter_mut() {
            if turn.state != TurnState::Running {
                continue;
            }
            if turn.ordinal == tail && active_wire_turn_id == Some(tail) {
                continue;
            }
            turn.state = TurnState::Failed;
            turn.wedged = Some(true);
            turn.ended_at = Some(ended_at.clone());
        }
    }

    fn upsert_interaction(&mut self, interaction: TranscriptInteraction) -> TranscriptOperation {
        if let Some(existing) = self
            .interactions
            .iter_mut()
            .find(|item| item.interaction_id == interaction.interaction_id)
        {
            *existing = interaction.clone();
        } else {
            self.interactions.push(interaction.clone());
        }
        TranscriptOperation::InteractionUpsert { interaction }
    }

    fn patch_interaction(
        &mut self,
        id: &str,
        state: InteractionState,
        response: Option<Value>,
    ) -> Option<TranscriptOperation> {
        let existing = self
            .interactions
            .iter_mut()
            .find(|item| item.interaction_id == id)?;
        existing.state = state;
        if response.is_some() {
            existing.response = response;
        }
        Some(TranscriptOperation::InteractionUpsert {
            interaction: existing.clone(),
        })
    }

    fn upsert_prompt(
        &mut self,
        prompt_id: &str,
        build: impl FnOnce(Option<&TranscriptPrompt>) -> TranscriptPrompt,
    ) -> TranscriptOperation {
        let existing = self.prompts.iter().find(|item| item.prompt_id == prompt_id);
        let prompt = build(existing);
        if let Some(slot) = self
            .prompts
            .iter_mut()
            .find(|item| item.prompt_id == prompt.prompt_id)
        {
            *slot = prompt.clone();
        } else {
            self.prompts.push(prompt.clone());
        }
        TranscriptOperation::PromptUpsert { prompt }
    }

    fn flush_pending_steers(&mut self) -> Vec<TranscriptOperation> {
        if self.pending_steers.is_empty() {
            return Vec::new();
        }
        let Some(cursor) = self.cursor else {
            return Vec::new();
        };
        if self.turns[cursor.turn].state != TurnState::Running {
            return Vec::new();
        }
        let turn_idx = cursor.turn;
        let turn_id = self.turns[turn_idx].turn_id.clone();
        let mut ops = Vec::new();

        let step_idx = if cursor.step < self.turns[turn_idx].steps.len() {
            cursor.step
        } else {
            let step_idx = self.ensure_step(turn_idx, 1);
            ops.push(TranscriptOperation::StepUpsert {
                turn_id: turn_id.clone(),
                step: step_header(&self.turns[turn_idx].steps[step_idx]),
            });
            step_idx
        };
        let step_id = self.turns[turn_idx].steps[step_idx].step_id.clone();
        for pending in std::mem::take(&mut self.pending_steers) {
            let frame_id = self.next_text_frame_id(turn_idx, step_idx);
            let frame = TranscriptFrame::Text(TextFrame {
                frame_id,
                text: pending.text,
                role: TextRole::User,
                attachment_ids: None,
                task_id: None,
                prompt_ids: pending.prompt_ids,
                origin: Some(TranscriptUserOrigin {
                    kind: UserOriginKind::User,
                    skill_activations: None,
                }),
            });
            self.turns[turn_idx].steps[step_idx]
                .frames
                .push(frame.clone());
            let frame_idx = self.turns[turn_idx].steps[step_idx].frames.len() - 1;
            ops.push(TranscriptOperation::FrameUpsert {
                turn_id: turn_id.clone(),
                step_id: step_id.clone(),
                frame,
            });

            self.cursor = Some(Cursor {
                turn: turn_idx,
                step: step_idx,
                frame: Some(frame_idx),
            });
        }
        ops
    }

    fn fold_turn_steer(&mut self, value: &Value) -> Vec<TranscriptOperation> {
        let Some(origin) = value.get("origin") else {
            return Vec::new();
        };
        let kind = origin.get("kind").and_then(|v| v.as_str());
        if !matches!(kind, Some("user") | Some("skill_activation")) {
            return Vec::new();
        }
        let Some(input) = value.get("input").and_then(|v| v.as_array()) else {
            return Vec::new();
        };
        let text: String = input
            .iter()
            .filter_map(|part| {
                if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                    part.get("text").and_then(|v| v.as_str())
                } else {
                    None
                }
            })
            .collect();
        let prompt_ids = value
            .get("promptIds")
            .and_then(|v| v.as_array())
            .map(|ids| {
                ids.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<String>>()
            })
            .filter(|ids| !ids.is_empty());
        self.pending_steers.push(PendingSteer { text, prompt_ids });
        Vec::new()
    }

    fn fold_prompt_steered(&mut self, value: &Value) -> Vec<TranscriptOperation> {
        let Some(steered_at) = value.get("steeredAt").and_then(|v| v.as_str()) else {
            return Vec::new();
        };
        let steered_at = steered_at.to_string();
        let content = value.get("content").cloned();
        let prompt_ids: Vec<String> = value
            .get("promptIds")
            .and_then(|v| v.as_array())
            .map(|ids| {
                ids.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let active_prompt_id = value
            .get("activePromptId")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let mut ops = Vec::new();

        if !active_prompt_id.is_empty() {
            let active_owned = active_prompt_id.to_string();
            let active_steered_at = steered_at.clone();
            ops.push(self.upsert_prompt(active_prompt_id, |prev| {
                TranscriptPrompt {
                    prompt_id: active_owned.clone(),
                    status: prev
                        .map(|prev| prev.status)
                        .unwrap_or(TranscriptPromptStatus::Running),
                    user_message_id: prev.and_then(|prev| prev.user_message_id.clone()),
                    content: prev
                        .and_then(|prev| prev.content.clone())
                        .or_else(|| content.clone()),
                    client_metadata: prev.and_then(|prev| prev.client_metadata.clone()),
                    created_at: prev
                        .map(|prev| prev.created_at.clone())
                        .unwrap_or_else(|| active_steered_at.clone()),
                    finished_at: prev.and_then(|prev| prev.finished_at.clone()),
                    steered_at: Some(active_steered_at.clone()),
                }
            }));
        }
        for prompt_id in &prompt_ids {
            let id_steered_at = steered_at.clone();
            ops.push(self.upsert_prompt(prompt_id, |prev| {
                TranscriptPrompt {
                    prompt_id: prompt_id.clone(),
                    status: TranscriptPromptStatus::Completed,
                    user_message_id: prev.and_then(|prev| prev.user_message_id.clone()),
                    content: prev.and_then(|prev| prev.content.clone()),
                    client_metadata: prev.and_then(|prev| prev.client_metadata.clone()),
                    created_at: prev
                        .map(|prev| prev.created_at.clone())
                        .unwrap_or_else(|| id_steered_at.clone()),
                    finished_at: Some(id_steered_at.clone()),
                    steered_at: Some(id_steered_at.clone()),
                }
            }));
        }
        ops
    }

    fn fold_prompt_completed(&mut self, value: &Value) -> Vec<TranscriptOperation> {
        let Some(prompt_id) = value.get("promptId").and_then(|v| v.as_str()) else {
            return Vec::new();
        };
        let status = match value.get("reason").and_then(|v| v.as_str()) {
            Some("failed") => TranscriptPromptStatus::Failed,
            Some("blocked") => TranscriptPromptStatus::Blocked,
            _ => TranscriptPromptStatus::Completed,
        };
        let finished_at = value
            .get("finishedAt")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(now_iso);
        vec![self.upsert_prompt(prompt_id, |prev| {
            TranscriptPrompt {
                prompt_id: prompt_id.to_string(),
                status,
                user_message_id: prev.and_then(|prev| prev.user_message_id.clone()),
                content: prev.and_then(|prev| prev.content.clone()),
                client_metadata: prev.and_then(|prev| prev.client_metadata.clone()),
                created_at: prev
                    .map(|prev| prev.created_at.clone())
                    .unwrap_or_else(|| finished_at.clone()),
                finished_at: Some(finished_at.clone()),
                steered_at: prev.and_then(|prev| prev.steered_at.clone()),
            }
        })]
    }

    fn fold_prompt_aborted(&mut self, value: &Value) -> Vec<TranscriptOperation> {
        let Some(prompt_id) = value.get("promptId").and_then(|v| v.as_str()) else {
            return Vec::new();
        };
        let aborted_at = value
            .get("abortedAt")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(now_iso);
        vec![self.upsert_prompt(prompt_id, |prev| {
            TranscriptPrompt {
                prompt_id: prompt_id.to_string(),
                status: TranscriptPromptStatus::Aborted,
                user_message_id: prev.and_then(|prev| prev.user_message_id.clone()),
                content: prev.and_then(|prev| prev.content.clone()),
                client_metadata: prev.and_then(|prev| prev.client_metadata.clone()),
                created_at: prev
                    .map(|prev| prev.created_at.clone())
                    .unwrap_or_else(|| aborted_at.clone()),
                finished_at: Some(aborted_at.clone()),
                steered_at: prev.and_then(|prev| prev.steered_at.clone()),
            }
        })]
    }

    fn fold_prompt_queued(&mut self, value: &Value) -> Vec<TranscriptOperation> {
        let Some(prompt_id) = value.get("promptId").and_then(|v| v.as_str()) else {
            return Vec::new();
        };
        vec![self.upsert_prompt(prompt_id, |prev| {
            TranscriptPrompt {
                prompt_id: prompt_id.to_string(),
                status: TranscriptPromptStatus::Queued,
                user_message_id: prev.and_then(|prev| prev.user_message_id.clone()),
                content: value
                    .get("content")
                    .cloned()
                    .or_else(|| prev.and_then(|prev| prev.content.clone())),
                client_metadata: prev.and_then(|prev| prev.client_metadata.clone()),
                created_at: prev
                    .map(|prev| prev.created_at.clone())
                    .unwrap_or_else(now_iso),
                finished_at: prev.and_then(|prev| prev.finished_at.clone()),
                steered_at: prev.and_then(|prev| prev.steered_at.clone()),
            }
        })]
    }

    fn fold_prompt_started(&mut self, value: &Value) -> Vec<TranscriptOperation> {
        let Some(prompt_id) = value.get("promptId").and_then(|v| v.as_str()) else {
            return Vec::new();
        };
        vec![self.upsert_prompt(prompt_id, |prev| {
            TranscriptPrompt {
                prompt_id: prompt_id.to_string(),
                status: TranscriptPromptStatus::Running,
                user_message_id: prev.and_then(|prev| prev.user_message_id.clone()),
                content: prev.and_then(|prev| prev.content.clone()),
                client_metadata: prev.and_then(|prev| prev.client_metadata.clone()),
                created_at: prev
                    .map(|prev| prev.created_at.clone())
                    .unwrap_or_else(now_iso),
                finished_at: prev.and_then(|prev| prev.finished_at.clone()),
                steered_at: prev.and_then(|prev| prev.steered_at.clone()),
            }
        })]
    }

    fn upsert_attachment(&mut self, attachment: TranscriptAttachment) -> TranscriptOperation {
        if let Some(existing) = self
            .attachments
            .iter_mut()
            .find(|item| item.attachment_id == attachment.attachment_id)
        {
            *existing = attachment.clone();
        } else {
            self.attachments.push(attachment.clone());
        }
        TranscriptOperation::AttachmentUpsert { attachment }
    }

    fn merge_agent_meta(&mut self, value: &Value) -> Vec<TranscriptOperation> {
        let model = value
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let thinking_effort = value
            .get("thinkingEffort")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let context_tokens = value.get("contextTokens").and_then(|v| v.as_i64());
        let permission =
            value
                .get("permission")
                .and_then(|v| v.as_str())
                .and_then(|mode| match mode.to_ascii_lowercase().as_str() {
                    "yolo" => Some(AgentPermission::Yolo),
                    "auto" => Some(AgentPermission::Auto),
                    "manual" => Some(AgentPermission::Manual),
                    _ => None,
                });
        let usage = value.get("usage").and_then(Self::parse_agent_usage);
        if model.is_none()
            && thinking_effort.is_none()
            && context_tokens.is_none()
            && permission.is_none()
            && usage.is_none()
        {
            return Vec::new();
        }
        let agent = self.meta.agent.get_or_insert(AgentStatusMeta {
            model: None,
            thinking_effort: None,
            usage: None,
            context_tokens: None,
            max_context_tokens: None,
            context_usage: None,
            permission: None,
            phase: None,
        });
        if model.is_some() {
            agent.model = model;
        }
        if thinking_effort.is_some() {
            agent.thinking_effort = thinking_effort;
        }
        if context_tokens.is_some() {
            agent.context_tokens = context_tokens;
        }
        if permission.is_some() {
            agent.permission = permission;
        }
        if usage.is_some() {
            agent.usage = usage;
        }
        vec![TranscriptOperation::MetaMerge {
            meta: TranscriptMetaMerge {
                agent: Some(agent.clone()),
                ..Default::default()
            },
        }]
    }

    fn parse_agent_usage(value: &Value) -> Option<AgentUsageMeta> {
        let by_model = value.get("byModel").cloned().and_then(|by_model| {
            serde_json::from_value::<HashMap<String, StepUsage>>(by_model).ok()
        });
        let current_turn = value
            .get("currentTurn")
            .cloned()
            .and_then(|current| serde_json::from_value::<StepUsage>(current).ok());
        let total = value
            .get("total")
            .cloned()
            .and_then(|total| serde_json::from_value::<StepUsage>(total).ok());
        if by_model.is_none() && current_turn.is_none() && total.is_none() {
            return None;
        }
        Some(AgentUsageMeta {
            by_model,
            current_turn,
            total,
        })
    }

    fn custom_delta_turn(&mut self, value: &Value, at: Option<i64>) -> Option<usize> {
        match value.get("turn_id").and_then(|v| v.as_u64()) {
            Some(turn_id) => Some(self.ensure_turn(&turn_key_u64(turn_id), at)),
            None => self.cursor.map(|cursor| cursor.turn),
        }
    }

    fn merge_activity(&mut self, busy: bool) -> Vec<TranscriptOperation> {
        let activity = if busy {
            ActivityMeta::Turn
        } else {
            ActivityMeta::Idle
        };
        self.meta.activity = Some(activity);
        vec![TranscriptOperation::MetaMerge {
            meta: TranscriptMetaMerge {
                activity: Some(activity),
                ..Default::default()
            },
        }]
    }

    fn subagent_task_ops(
        &mut self,
        agent_id: &str,
        state: TaskState,
        summary: Option<String>,
        error: Option<String>,
        usage: Option<StepUsage>,
    ) -> Vec<TranscriptOperation> {
        let task = TranscriptTask {
            task_id: agent_id.to_string(),
            kind: TaskKind::Subagent,
            state,
            detached: false,
            description: Some(agent_id.to_string()),
            agent_id: Some(agent_id.to_string()),
            output_tail: String::new(),
            started_at: Some(now_iso()),
            ended_at: if state == TaskState::Running {
                None
            } else {
                Some(now_iso())
            },
            result_summary: summary,
            error,
            state_reason: None,
            usage,
            model: None,
            thinking_effort: None,
        };
        let task = self.merge_task(task);
        vec![TranscriptOperation::TaskUpsert { task }]
    }

    fn ensure_turn(&mut self, key: &str, at: Option<i64>) -> usize {
        if let Some(&idx) = self.turn_indices.get(key) {
            return idx;
        }
        let ordinal = match key
            .strip_prefix('t')
            .and_then(|digits| digits.parse::<i64>().ok())
        {
            Some(parsed) => parsed,
            None => self
                .turns
                .iter()
                .map(|turn| turn.ordinal)
                .max()
                .map_or(0, |max| max + 1),
        };
        let idx = self.turns.len();
        self.turns.push(TranscriptTurn {
            turn_id: key.to_string(),
            trigger_prompt_id: None,
            ordinal,
            state: TurnState::Running,
            origin: TurnOrigin::User { payload: None },
            prompt: None,
            attachment_ids: None,
            steps: Vec::new(),
            started_at: Some(at.map(epoch_ms_iso).unwrap_or_else(now_iso)),
            ended_at: None,
            usage: None,
            duration_ms: None,
            error: None,
            wedged: None,
        });
        self.turn_indices.insert(key.to_string(), idx);
        idx
    }

    fn ensure_step(&mut self, turn_idx: usize, ordinal: i64) -> usize {
        if let Some(idx) = self.turns[turn_idx]
            .steps
            .iter()
            .position(|step| step.ordinal == ordinal)
        {
            return idx;
        }
        let turn_id = self.turns[turn_idx].turn_id.clone();
        let idx = self.turns[turn_idx].steps.len();
        self.turns[turn_idx].steps.push(TranscriptStep {
            step_id: format!("{turn_id}.{ordinal}"),
            turn_id,
            ordinal,
            state: StepState::Running,
            frames: Vec::new(),
            started_at: Some(now_iso()),
            ended_at: None,
            usage: None,
            finish_reason: None,
            timing: None,
            retry: None,
            end_reason: None,
            end_message: None,
        });
        idx
    }

    fn step_index_for_turn(&mut self, turn_idx: usize) -> usize {
        if let Some(cursor) = self.cursor
            && cursor.turn == turn_idx
            && cursor.step < self.turns[turn_idx].steps.len()
        {
            return cursor.step;
        }
        let len = self.turns[turn_idx].steps.len();
        if len > 0 {
            len - 1
        } else {
            self.ensure_step(turn_idx, 1)
        }
    }

    fn resolve_turn(&self, turn_id: &str) -> Option<usize> {
        self.turn_indices
            .get(&turn_key(turn_id))
            .copied()
            .or_else(|| self.cursor.map(|cursor| cursor.turn))
    }

    fn find_tool_frame(&self, turn_idx: usize, tool_call_id: &str) -> Option<(usize, usize)> {
        let turn = &self.turns[turn_idx];
        for (step_idx, step) in turn.steps.iter().enumerate() {
            for (frame_idx, frame) in step.frames.iter().enumerate() {
                if let TranscriptFrame::Tool(tool) = frame
                    && tool.tool_call_id == tool_call_id
                {
                    return Some((step_idx, frame_idx));
                }
            }
        }
        None
    }

    fn find_tool_frame_any(&self, tool_call_id: &str) -> Option<(usize, usize, usize)> {
        for (turn_idx, turn) in self.turns.iter().enumerate() {
            for (step_idx, step) in turn.steps.iter().enumerate() {
                for (frame_idx, frame) in step.frames.iter().enumerate() {
                    if let TranscriptFrame::Tool(tool) = frame
                        && tool.tool_call_id == tool_call_id
                    {
                        return Some((turn_idx, step_idx, frame_idx));
                    }
                }
            }
        }
        None
    }

    fn push_delta(
        &mut self,
        turn_idx: usize,
        delta: &str,
        thinking: bool,
    ) -> Vec<TranscriptOperation> {
        let step_idx = self.step_index_for_turn(turn_idx);
        let turn_id = self.turns[turn_idx].turn_id.clone();
        let step_id = self.turns[turn_idx].steps[step_idx].step_id.clone();
        let trailing = self.turns[turn_idx].steps[step_idx].frames.last();
        let cursor_frame = self
            .cursor
            .filter(|cursor| cursor.turn == turn_idx && cursor.step == step_idx)
            .and_then(|cursor| cursor.frame);
        let last_index = self.turns[turn_idx].steps[step_idx]
            .frames
            .len()
            .checked_sub(1);
        let matches_trailing = cursor_frame.is_some()
            && cursor_frame == last_index
            && match (trailing, thinking) {
                (Some(TranscriptFrame::Text(frame)), false) => frame.role == TextRole::Assistant,
                (Some(TranscriptFrame::Thinking(_)), true) => true,
                _ => false,
            };
        let mut ops = Vec::new();
        let frame_idx = if matches_trailing {
            self.turns[turn_idx].steps[step_idx].frames.len() - 1
        } else {
            let frame_id = self.next_text_frame_id(turn_idx, step_idx);
            let frame = if thinking {
                TranscriptFrame::Thinking(ThinkingFrame {
                    frame_id,
                    text: String::new(),
                })
            } else {
                TranscriptFrame::Text(TextFrame {
                    frame_id,
                    text: String::new(),
                    role: TextRole::Assistant,
                    attachment_ids: None,
                    task_id: None,
                    prompt_ids: None,
                    origin: None,
                })
            };
            self.turns[turn_idx].steps[step_idx].frames.push(frame);
            let idx = self.turns[turn_idx].steps[step_idx].frames.len() - 1;
            ops.push(TranscriptOperation::FrameUpsert {
                turn_id: turn_id.clone(),
                step_id: step_id.clone(),
                frame: self.turns[turn_idx].steps[step_idx].frames[idx].clone(),
            });
            idx
        };

        let offset = match &self.turns[turn_idx].steps[step_idx].frames[frame_idx] {
            TranscriptFrame::Text(frame) => frame.text.encode_utf16().count() as u64,
            TranscriptFrame::Thinking(frame) => frame.text.encode_utf16().count() as u64,
            _ => 0,
        };
        match &mut self.turns[turn_idx].steps[step_idx].frames[frame_idx] {
            TranscriptFrame::Text(frame) => frame.text.push_str(delta),
            TranscriptFrame::Thinking(frame) => frame.text.push_str(delta),
            _ => {}
        }
        let frame_id = match &self.turns[turn_idx].steps[step_idx].frames[frame_idx] {
            TranscriptFrame::Text(frame) => frame.frame_id.clone(),
            TranscriptFrame::Thinking(frame) => frame.frame_id.clone(),
            _ => String::new(),
        };
        ops.push(TranscriptOperation::Append {
            target: AppendTarget::Frame {
                turn_id,
                step_id,
                frame_id,
            },
            offset,
            text: delta.to_string(),
        });
        self.cursor = Some(Cursor {
            turn: turn_idx,
            step: step_idx,
            frame: Some(frame_idx),
        });
        ops
    }

    fn next_text_frame_id(&self, turn_idx: usize, step_idx: usize) -> String {
        let step = &self.turns[turn_idx].steps[step_idx];
        let prefix = format!("{}.f", step.step_id);
        let mut max = 0u64;
        for frame in &step.frames {
            let frame_id = match frame {
                TranscriptFrame::Text(frame) => Some(frame.frame_id.as_str()),
                TranscriptFrame::Thinking(frame) => Some(frame.frame_id.as_str()),
                _ => None,
            };
            if let Some(seq) = frame_id
                .and_then(|id| id.strip_prefix(&prefix))
                .and_then(|rest| rest.parse::<u64>().ok())
            {
                max = max.max(seq);
            }
        }
        format!("{}.f{}", step.step_id, max + 1)
    }

    fn tool_frame_op(
        &self,
        turn_idx: usize,
        step_idx: usize,
        frame_idx: usize,
    ) -> TranscriptOperation {
        let step = &self.turns[turn_idx].steps[step_idx];
        TranscriptOperation::FrameUpsert {
            turn_id: step.turn_id.clone(),
            step_id: step.step_id.clone(),
            frame: step.frames[frame_idx].clone(),
        }
    }

    fn merge_task(&mut self, incoming: TranscriptTask) -> TranscriptTask {
        let position = self.tasks.iter().position(|task| {
            task.task_id == incoming.task_id
                || (incoming.agent_id.is_some() && task.agent_id == incoming.agent_id)
        });
        if let Some(position) = position {
            let existing = &mut self.tasks[position];

            if existing.task_id != incoming.task_id
                && incoming.agent_id.as_deref() == Some(existing.task_id.as_str())
            {
                existing.task_id = incoming.task_id.clone();
            }
            apply_task_fields(existing, &incoming);
            return existing.clone();
        }
        self.tasks.push(incoming.clone());
        incoming
    }
}

fn apply_task_fields(existing: &mut TranscriptTask, incoming: &TranscriptTask) {
    if !is_terminal_task_state(existing.state) {
        existing.state = incoming.state;
    } else if !is_terminal_task_state(incoming.state) && incoming.state != existing.state {
        tracing::debug!(
            task_id = %existing.task_id,
            kept = ?existing.state,
            ignored = ?incoming.state,
            "ignoring a non-terminal task state for an already-settled task",
        );
    }
    if incoming.kind != TaskKind::Other {
        existing.kind = incoming.kind;
    }
    if !incoming.output_tail.is_empty() {
        existing.output_tail = incoming.output_tail.clone();
    }
    if incoming.description.is_some() {
        existing.description = incoming.description.clone();
    }
    if incoming.agent_id.is_some() {
        existing.agent_id = incoming.agent_id.clone();
    }
    if incoming.started_at.is_some() {
        existing.started_at = incoming.started_at.clone();
    }
    if incoming.ended_at.is_some() {
        existing.ended_at = incoming.ended_at.clone();
    }
    if incoming.result_summary.is_some() {
        existing.result_summary = incoming.result_summary.clone();
    }
    if incoming.error.is_some() {
        existing.error = incoming.error.clone();
    }
    if incoming.state_reason.is_some() {
        existing.state_reason = incoming.state_reason.clone();
    }
    if incoming.usage.is_some() {
        existing.usage = incoming.usage.clone();
    }
    if incoming.model.is_some() {
        existing.model = incoming.model.clone();
    }
    if incoming.thinking_effort.is_some() {
        existing.thinking_effort = incoming.thinking_effort.clone();
    }
}

fn is_terminal_task_state(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::Failed | TaskState::Killed | TaskState::Lost
    )
}

fn rpc_usage_as_step_usage(usage: kimi_core::types::TokenUsage) -> StepUsage {
    StepUsage {
        input_other: usage.input_other,
        output: usage.output,
        input_cache_read: usage.input_cache_read,
        input_cache_creation: usage.input_cache_creation,
    }
}

impl Default for TranscriptProjector {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn turn_header(turn: &TranscriptTurn) -> TurnHeader {
    TurnHeader {
        kind: TurnKind::Turn,
        turn_id: turn.turn_id.clone(),
        trigger_prompt_id: turn.trigger_prompt_id.clone(),
        ordinal: turn.ordinal,
        state: turn.state,
        origin: turn.origin.clone(),
        prompt: turn.prompt.clone(),
        attachment_ids: turn.attachment_ids.clone(),
        started_at: turn.started_at.clone(),
        ended_at: turn.ended_at.clone(),
        usage: turn.usage.clone(),
        duration_ms: turn.duration_ms,
        error: turn.error.clone(),
        wedged: turn.wedged,
    }
}

pub(crate) fn step_header(step: &TranscriptStep) -> StepHeader {
    StepHeader {
        kind: StepKind::Step,
        step_id: step.step_id.clone(),
        turn_id: step.turn_id.clone(),
        ordinal: step.ordinal,
        state: step.state,
        started_at: step.started_at.clone(),
        ended_at: step.ended_at.clone(),
        usage: step.usage.clone(),
        finish_reason: step.finish_reason.clone(),
        timing: step.timing.clone(),
        retry: step.retry.clone(),
        end_reason: step.end_reason.clone(),
        end_message: step.end_message.clone(),
    }
}

fn turn_key(raw: &str) -> String {
    let digits = raw
        .rsplit_once(|c: char| !c.is_ascii_digit())
        .map_or(raw, |(_, digits)| digits);
    match digits.parse::<u64>() {
        Ok(number) => turn_key_u64(number),
        Err(_) => raw.to_string(),
    }
}

fn turn_key_u64(raw: u64) -> String {
    format!("t{raw}")
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn epoch_ms_iso(epoch_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .map(|instant| instant.to_rfc3339())
        .unwrap_or_else(now_iso)
}

fn progress_from_update(update: &Value) -> ToolFrameProgress {
    if let Some(object) = update.as_object() {
        let structured = ["kind", "text", "percent", "customKind", "customData"]
            .iter()
            .any(|key| object.contains_key(*key));
        if structured {
            let kind = match object.get("kind").and_then(|v| v.as_str()) {
                Some("stdout") => ToolProgressKind::Stdout,
                Some("stderr") => ToolProgressKind::Stderr,
                Some("progress") => ToolProgressKind::Progress,
                Some("status") => ToolProgressKind::Status,
                _ => ToolProgressKind::Custom,
            };
            return ToolFrameProgress {
                kind,
                text: object
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                percent: object.get("percent").and_then(|v| v.as_f64()),
                custom_kind: object
                    .get("customKind")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                custom_data: object.get("customData").cloned(),
            };
        }
    }
    ToolFrameProgress {
        kind: ToolProgressKind::Custom,
        text: Some(update.to_string()),
        percent: None,
        custom_kind: None,
        custom_data: None,
    }
}

fn task_from_event(value: &Value) -> Option<TranscriptTask> {
    let info = value.get("info").cloned().unwrap_or(Value::Null);
    let task = value.get("task").cloned().unwrap_or(Value::Null);
    let field = |keys: &[&str]| {
        str_field(&info, keys)
            .or_else(|| str_field(&task, keys))
            .or_else(|| str_field(value, keys))
    };
    let time = |keys: &[&str]| {
        time_field(&info, keys)
            .or_else(|| time_field(&task, keys))
            .or_else(|| time_field(value, keys))
    };
    let task_id = field(&["taskId", "id", "task_id"])?;
    Some(TranscriptTask {
        task_id,
        kind: task_kind(field(&["kind"]).as_deref()),
        state: task_state(field(&["state", "status"]).as_deref()),
        detached: info
            .get("detached")
            .or_else(|| task.get("detached"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        description: field(&["description"]),
        agent_id: field(&["agentId", "agent_id"]),
        output_tail: field(&["outputTail", "output_tail", "output"])
            .or_else(|| field(&["output_preview", "outputPreview"]))
            .unwrap_or_default(),
        started_at: time(&["startedAt", "started_at"]),
        ended_at: time(&["endedAt", "ended_at", "finished_at"]),
        result_summary: field(&["result_summary", "resultSummary"]),
        error: field(&["error"]),
        state_reason: field(&["stopReason", "state_reason", "stateReason"]),
        usage: None,
        model: field(&["model"]),
        thinking_effort: field(&["thinking_effort", "thinkingEffort"]),
    })
}

fn task_kind(raw: Option<&str>) -> TaskKind {
    match raw {
        Some("shell") | Some("process") => TaskKind::Shell,
        Some("subagent") | Some("agent") => TaskKind::Subagent,
        Some("tool") => TaskKind::Tool,
        _ => TaskKind::Other,
    }
}

fn task_state(raw: Option<&str>) -> TaskState {
    match raw {
        Some("running") => TaskState::Running,
        Some("completed") => TaskState::Completed,
        Some("failed") => TaskState::Failed,
        Some("timed_out") | Some("timedout") => TaskState::TimedOut,
        Some("killed") => TaskState::Killed,
        Some("lost") => TaskState::Lost,
        _ => TaskState::Running,
    }
}

fn str_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|v| v.as_str()).map(str::to_string))
}

fn time_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match value.get(*key) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => number.as_i64().map(epoch_ms_iso),
        _ => None,
    })
}

fn attachment_from_block(block: &Value) -> Option<TranscriptAttachment> {
    let kind = block.get("type").and_then(|v| v.as_str())?;
    let url_source = || {
        block
            .get("url")
            .and_then(|v| v.as_str())
            .map(|url| AttachmentSource::Url {
                url: url.to_string(),
            })
    };

    let daemon_file_source = || {
        block
            .get("url")
            .and_then(|v| v.as_str())
            .and_then(crate::llm::media_resolver::parse_daemon_file_url)
            .map(|file_id| AttachmentSource::File {
                file_id: file_id.to_string(),
            })
    };
    let (media_type, source) = match kind {
        "media_ref" => {
            let file_id = block.get("file_id").and_then(|v| v.as_str())?;
            (
                media_kind_label(block.get("kind").and_then(|v| v.as_str())),
                Some(AttachmentSource::SessionMedia {
                    file_id: file_id.to_string(),
                }),
            )
        }
        "image" => (
            block
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or("image")
                .to_string(),
            None,
        ),
        "image_url" => (
            "image".to_string(),
            daemon_file_source().or_else(url_source),
        ),
        "audio_url" => (
            "audio".to_string(),
            daemon_file_source().or_else(url_source),
        ),
        "video_url" => (
            "video".to_string(),
            daemon_file_source().or_else(url_source),
        ),
        _ => return None,
    };
    Some(TranscriptAttachment {
        attachment_id: format!("att-{:016x}", stable_hash(&block.to_string())),
        media_type,
        name: None,
        size: None,
        source,
        placeholder: None,
    })
}

fn media_kind_label(kind: Option<&str>) -> String {
    match kind {
        Some("video") => "video".to_string(),
        Some("audio") => "audio".to_string(),
        _ => "image".to_string(),
    }
}

fn stable_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests;
