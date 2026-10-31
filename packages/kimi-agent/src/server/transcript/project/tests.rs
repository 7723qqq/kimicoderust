use super::*;
use serde_json::json;

#[test]
fn folds_a_full_turn_into_transcript_ops() {
    let mut projector = TranscriptProjector::new();

    let ops = projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-0".into(),
        prompt: Some("hello".into()),
    });
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], TranscriptOperation::TurnUpsert { .. }));

    let ops = projector.apply_event(&EngineEvent::LlmStepBegin {
        agent_id: "main".into(),
        turn_id: "0".into(),
        step: 1,
    });
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], TranscriptOperation::StepUpsert { .. }));

    let ops = projector.apply_event(&EngineEvent::AssistantDelta {
        agent_id: "main".into(),
        turn_id: "turn-0".into(),
        delta: "Hello".into(),
    });
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], TranscriptOperation::FrameUpsert { .. }));
    match &ops[1] {
        TranscriptOperation::Append { offset, text, .. } => {
            assert_eq!(*offset, 0);
            assert_eq!(text, "Hello");
        }
        other => panic!("expected append, got {other:?}"),
    }

    let ops = projector.apply_event(&EngineEvent::AssistantDelta {
        agent_id: "main".into(),
        turn_id: "turn-0".into(),
        delta: " world".into(),
    });
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        TranscriptOperation::Append { offset, text, .. } => {
            assert_eq!(*offset, "Hello".len() as u64);
            assert_eq!(text, " world");
        }
        other => panic!("expected append, got {other:?}"),
    }

    let ops = projector.apply_event(&EngineEvent::ToolCallStarted {
        agent_id: "main".into(),
        turn_id: "turn-0".into(),
        tool_call_id: "c1".into(),
        name: "Read".into(),
        args: json!({ "path": "a.txt" }),
    });
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], TranscriptOperation::FrameUpsert { .. }));

    let ops = projector.apply_event(&EngineEvent::ToolCallCompleted {
        agent_id: "main".into(),
        turn_id: "turn-0".into(),
        tool_call_id: "c1".into(),
        result: json!("file contents"),
    });
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        TranscriptOperation::FrameUpsert { frame, .. } => match frame {
            TranscriptFrame::Tool(tool) => {
                assert_eq!(tool.state, ToolFrameState::Done);
                assert_eq!(tool.output, Some(json!("file contents")));
            }
            other => panic!("expected tool frame, got {other:?}"),
        },
        other => panic!("expected frame upsert, got {other:?}"),
    }

    let ops = projector.apply_event(&EngineEvent::TurnEnded {
        agent_id: "main".into(),
        turn_id: "turn-0".into(),
        reason: "completed".into(),
    });
    // The turn closes and its still-running step settles with it: the
    // client stamps a thinking block's `durationMs` only once the step
    // it belongs to has stopped running.
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], TranscriptOperation::TurnUpsert { .. }));
    assert!(matches!(ops[1], TranscriptOperation::StepUpsert { .. }));

    let snapshot = projector.snapshot();
    assert_eq!(snapshot.items.len(), 1);
    match &snapshot.items[0] {
        TranscriptItem::Turn(turn) => {
            assert_eq!(turn.turn_id, "t0");
            assert_eq!(turn.state, TurnState::Completed);
            assert_eq!(turn.prompt.as_deref(), Some("hello"));
            let text = match &turn.steps[0].frames[0] {
                TranscriptFrame::Text(frame) => &frame.text,
                other => panic!("expected text frame, got {other:?}"),
            };
            assert_eq!(text, "Hello world");
        }
        other => panic!("expected turn item, got {other:?}"),
    }
}

#[test]
fn step_end_tool_delta_and_task_progress_ops() {
    use crate::rpc::types::TokenUsage;

    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-0".into(),
        prompt: None,
    });
    projector.apply_event(&EngineEvent::LlmStepBegin {
        agent_id: "main".into(),
        turn_id: "t0".into(),
        step: 1,
    });
    projector.apply_event(&EngineEvent::ToolCallStarted {
        agent_id: "main".into(),
        turn_id: "turn-0".into(),
        tool_call_id: "c1".into(),
        name: "Read".into(),
        args: json!({}),
    });

    // A streamed tool-argument delta lands in the frame's inputText.
    let ops = projector.apply_event(&EngineEvent::ToolCallDelta {
        agent_id: "main".into(),
        turn_id: "turn-0".into(),
        tool_call_id: "c1".into(),
        name: None,
        arguments_part: Some("{\"path\"".into()),
    });
    match &ops[0] {
        TranscriptOperation::FrameUpsert {
            frame: TranscriptFrame::Tool(tool),
            ..
        } => assert_eq!(tool.input_text.as_deref(), Some("{\"path\"")),
        other => panic!("expected tool frame upsert, got {other:?}"),
    }

    // The step end carries usage.
    let ops = projector.apply_event(&EngineEvent::LlmStepEnd {
        agent_id: "main".into(),
        turn_id: "t0".into(),
        step: 1,
        usage: Some(TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            input_cache_read: 5,
            input_cache_creation: 1,
        }),
        timing: Some(crate::llm::LlmTiming::from_marks(
            0,
            Some(10),
            Some(110),
            210,
        )),
    });
    match &ops[0] {
        TranscriptOperation::StepUpsert { step, .. } => {
            let usage = step.usage.as_ref().expect("usage recorded");
            assert_eq!(usage.output, 20);
            assert_eq!(usage.input_cache_read, 5);
            assert_eq!(step.state, StepState::Completed);
            // Upstream #3938: the timing rides the event into the step.
            let timing = step.timing.as_ref().expect("timing recorded");
            assert_eq!(timing.llm_first_token_latency_ms, Some(110));
            assert_eq!(timing.llm_stream_duration_ms, Some(100));
            assert_eq!(timing.llm_request_build_ms, Some(10));
            assert_eq!(timing.llm_server_first_token_ms, Some(100));
        }
        other => panic!("expected step upsert, got {other:?}"),
    }

    // A background-task progress chunk appends to the task's output tail.
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.task.created",
        "task": { "id": "task_1", "kind": "bash", "status": "running", "description": "bg" }
    })));
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.task.progress",
        "task_id": "task_1",
        "output_chunk": "line1\n",
        "stream": "stdout"
    })));
    match &ops[0] {
        TranscriptOperation::Append {
            target: AppendTarget::Task { task_id },
            offset,
            text,
        } => {
            assert_eq!(task_id, "task_1");
            assert_eq!(*offset, 0);
            assert_eq!(text, "line1\n");
        }
        other => panic!("expected task append, got {other:?}"),
    }

    // `hook.result` becomes a marker.
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "hook.result",
        "hook": "PreToolUse"
    })));
    match &ops[0] {
        TranscriptOperation::MarkerUpsert { item, .. } => assert_eq!(item.marker, "hook"),
        other => panic!("expected marker upsert, got {other:?}"),
    }
}

#[test]
fn interaction_and_prompt_ops() {
    let mut projector = TranscriptProjector::new();

    // A user message becomes a prompt; an assistant message does not.
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.message.created",
        "message": {
            "id": "m1",
            "role": "user",
            "content": [{ "type": "text", "text": "hi" }],
            "created_at": "2026-01-01T00:00:00Z"
        }
    })));
    assert!(matches!(ops[0], TranscriptOperation::PromptUpsert { .. }));
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.message.created",
        "message": { "id": "m2", "role": "assistant", "content": [] }
    })));
    assert!(ops.is_empty(), "assistant messages are not prompts");

    // Question requested -> pending; answered -> answered.
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.question.requested",
        "question_id": "q1",
        "tool_call_id": "c1",
        "questions": []
    })));
    match &ops[0] {
        TranscriptOperation::InteractionUpsert { interaction } => {
            assert_eq!(interaction.interaction_kind, InteractionKind::Question);
            assert_eq!(interaction.state, InteractionState::Pending);
        }
        other => panic!("expected interaction upsert, got {other:?}"),
    }
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.question.answered",
        "question_id": "q1"
    })));
    match &ops[0] {
        TranscriptOperation::InteractionUpsert { interaction } => {
            assert_eq!(interaction.state, InteractionState::Answered);
        }
        other => panic!("expected interaction upsert, got {other:?}"),
    }

    // Approval requested -> resolved(allow) -> approved.
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.approval.requested",
        "approval_id": "a1",
        "tool_call_id": "c2"
    })));
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.approval.resolved",
        "approval_id": "a1",
        "decision": "allow"
    })));
    match &ops[0] {
        TranscriptOperation::InteractionUpsert { interaction } => {
            assert_eq!(interaction.state, InteractionState::Approved);
        }
        other => panic!("expected interaction upsert, got {other:?}"),
    }

    let snapshot = projector.snapshot();
    assert_eq!(snapshot.interactions.len(), 2);
    assert_eq!(snapshot.prompts.len(), 1);
}

/// v2 `displayPaths` reaching the protocol surface (ROADMAP #17 ②): a
/// stored media reference folds into an attachment that names its file,
/// so a client can resolve the saved path through the daemon's file API
/// instead of receiving an identity-less URL or nothing at all.
#[test]
fn a_stored_media_reference_folds_into_a_file_sourced_attachment() {
    let mut projector = TranscriptProjector::new();
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.message.created",
        "message": {
            "id": "msg-u2",
            "role": "user",
            "created_at": "t",
            "content": [
                { "type": "text", "text": "see" },
                { "type": "media_ref", "file_id": "f_att1", "kind": "image" },
                { "type": "image_url", "url": "kimi-file://f_att2" },
                { "type": "image_url", "url": "https://example.test/remote.png" },
            ]
        }
    })));

    let attachments: Vec<&TranscriptAttachment> = ops
        .iter()
        .filter_map(|op| match op {
            TranscriptOperation::AttachmentUpsert { attachment } => Some(attachment),
            _ => None,
        })
        .collect();
    assert_eq!(attachments.len(), 3);

    // The stored reference is the session-owned canonical copy.
    assert_eq!(
        attachments[0].source,
        Some(AttachmentSource::SessionMedia {
            file_id: "f_att1".to_string()
        })
    );
    assert_eq!(attachments[0].media_type, "image");
    // A daemon URL is a file identity, not a fetchable URL.
    assert_eq!(
        attachments[1].source,
        Some(AttachmentSource::File {
            file_id: "f_att2".to_string()
        })
    );
    // A remote URL stays a URL.
    assert_eq!(
        attachments[2].source,
        Some(AttachmentSource::Url {
            url: "https://example.test/remote.png".to_string()
        })
    );

    // The turn the prompt opened names its attachments (v2's cold-fold
    // `turn.attachmentIds`): the link a client follows to the saved path.
    let snapshot = projector.snapshot();
    let turn = snapshot
        .items
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Turn(turn) if turn.turn_id == "t2" => Some(turn),
            _ => None,
        })
        .expect("the message id names the turn");
    assert_eq!(
        turn.attachment_ids.as_deref(),
        Some(
            &[
                attachments[0].attachment_id.clone(),
                attachments[1].attachment_id.clone(),
                attachments[2].attachment_id.clone(),
            ][..]
        ),
        "every media part of the opening prompt is an attachment, URL-sourced ones included"
    );
}

/// v2 #3764 on the v1 transcript surface: the contract's
/// `transcriptPromptSchema` carries `clientMetadata`, and the
/// submission's metadata object rides it as the one-element array the
/// contract shapes. The turn's announcement upserts the same prompt
/// and carries the metadata over rather than dropping it.
#[test]
fn the_prompt_entity_carries_the_submissions_client_metadata() {
    let mut projector = TranscriptProjector::new();
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "prompt.submitted",
        "promptId": "prompt-1",
        "userMessageId": "msg-prompt-1",
        "status": "running",
        "content": [{ "type": "text", "text": "hi" }],
        "createdAt": "2026-09-20T00:00:00.000Z",
        "metadata": { "surface": "web", "threadId": "abc" },
    })));
    match &ops[0] {
        TranscriptOperation::PromptUpsert { prompt } => {
            assert_eq!(prompt.prompt_id, "prompt-1");
            assert_eq!(
                prompt.client_metadata,
                Some(vec![json!({ "surface": "web", "threadId": "abc" })]),
            );
        }
        other => panic!("expected a prompt upsert, got {other:?}"),
    }

    // The announcement upserts the same prompt and keeps the metadata.
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.message.created",
        "message": {
            "id": "msg-u1",
            "role": "user",
            "content": [{ "type": "text", "text": "hi" }],
            "created_at": "2026-09-20T00:00:01.000Z",
        },
    })));
    // The announcement's prompt id is the message id, which differs
    // from the submission's prompt id — the two entities are distinct,
    // and only the submission's carries the metadata.
    let prompts: Vec<&TranscriptPrompt> = ops
        .iter()
        .filter_map(|op| match op {
            TranscriptOperation::PromptUpsert { prompt } => Some(prompt),
            _ => None,
        })
        .collect();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].prompt_id, "msg-u1");
    assert_eq!(prompts[0].client_metadata, None);

    // A submission whose prompt id IS the message id (the steer path
    // reuses the prompt id) keeps its metadata across the announcement.
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "prompt.submitted",
        "promptId": "msg-u2",
        "status": "running",
        "content": [{ "type": "text", "text": "hi" }],
        "createdAt": "2026-09-20T00:00:00.000Z",
        "metadata": { "surface": "web" },
    })));
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.message.created",
        "message": {
            "id": "msg-u2",
            "role": "user",
            "content": [{ "type": "text", "text": "hi" }],
            "created_at": "2026-09-20T00:00:01.000Z",
        },
    })));
    match &ops[0] {
        TranscriptOperation::PromptUpsert { prompt } => {
            assert_eq!(
                prompt.client_metadata,
                Some(vec![json!({ "surface": "web" })]),
                "the announcement must not drop the submission's metadata"
            );
        }
        other => panic!("expected a prompt upsert, got {other:?}"),
    }
}

#[test]
fn agent_meta_subagent_and_attachment_ops() {
    let mut projector = TranscriptProjector::new();

    // The engine status payload merges model/effort/context/permission.
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "agent.status.updated",
        "model": "k3",
        "thinkingEffort": "high",
        "contextTokens": 1234,
        "permission": "yolo"
    })));
    match &ops[0] {
        TranscriptOperation::MetaMerge { meta } => {
            let agent = meta.agent.as_ref().expect("agent meta");
            assert_eq!(agent.model.as_deref(), Some("k3"));
            assert_eq!(agent.context_tokens, Some(1234));
            assert_eq!(agent.permission, Some(AgentPermission::Yolo));
        }
        other => panic!("expected meta.merge, got {other:?}"),
    }
    // A phase-only activity payload carries no mergeable scalar.
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "agent.status.updated",
        "phase": { "kind": "running" }
    })));
    assert!(ops.is_empty());

    // `work_changed` merges the activity state. The engine publishes the
    // typed variant on the production path, so that spelling is the one
    // that has to work; the JSON form stays for replay-shaped input.
    let ops = projector.apply_event(&EngineEvent::SessionWorkChanged {
        busy: true,
        main_turn_active: true,
        pending_interaction: "none".into(),
        last_turn_reason: None,
    });
    match &ops[0] {
        TranscriptOperation::MetaMerge { meta } => {
            assert_eq!(meta.activity, Some(ActivityMeta::Turn));
        }
        other => panic!("expected meta.merge, got {other:?}"),
    }
    let ops = projector.apply_event(&EngineEvent::SessionWorkChanged {
        busy: false,
        main_turn_active: false,
        pending_interaction: "none".into(),
        last_turn_reason: Some("completed".into()),
    });
    match &ops[0] {
        TranscriptOperation::MetaMerge { meta } => {
            assert_eq!(
                meta.activity,
                Some(ActivityMeta::Idle),
                "a finished turn must clear the activity flag the client gates on"
            );
        }
        other => panic!("expected meta.merge, got {other:?}"),
    }
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.session.work_changed",
        "busy": true
    })));
    match &ops[0] {
        TranscriptOperation::MetaMerge { meta } => {
            assert_eq!(meta.activity, Some(ActivityMeta::Turn));
        }
        other => panic!("expected meta.merge, got {other:?}"),
    }

    // Subagent lifecycle maps onto a transcript task.
    let ops = projector.apply_event(&EngineEvent::SubagentSpawned {
        subagent_id: "sub-1".into(),
        subagent_name: Some("research".into()),
        parent_tool_call_id: Some("call-1".into()),
        description: None,
        run_in_background: false,
        swarm_index: None,
    });
    match &ops[0] {
        TranscriptOperation::TaskUpsert { task } => {
            assert_eq!(task.task_id, "sub-1");
            assert_eq!(task.kind, TaskKind::Subagent);
            assert_eq!(task.state, TaskState::Running);
        }
        other => panic!("expected task upsert, got {other:?}"),
    }
    let ops = projector.apply_event(&EngineEvent::SubagentCompleted {
        subagent_id: "sub-1".into(),
        result_summary: Some("done".into()),
        usage: Some(crate::rpc::types::TokenUsage {
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
            input_cache_read: 3,
            input_cache_creation: 2,
        }),
    });
    match &ops[0] {
        TranscriptOperation::TaskUpsert { task } => {
            assert_eq!(task.state, TaskState::Completed);
            assert_eq!(task.result_summary.as_deref(), Some("done"));
            // #3970: the durable record's four counters restore onto the
            // folded task.
            let usage = task.usage.as_ref().expect("usage restored");
            assert_eq!(usage.input_other, 10);
            assert_eq!(usage.output, 4);
            assert_eq!(usage.input_cache_read, 3);
            assert_eq!(usage.input_cache_creation, 2);
        }
        other => panic!("expected task upsert, got {other:?}"),
    }

    // A user message with an image_url block yields a prompt + attachment.
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.message.created",
        "message": {
            "id": "m1",
            "role": "user",
            "created_at": "t",
            "content": [
                { "type": "text", "text": "see" },
                { "type": "image_url", "url": "https://example.test/a.png" }
            ]
        }
    })));
    assert!(
        ops.iter()
            .any(|op| matches!(op, TranscriptOperation::PromptUpsert { .. }))
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, TranscriptOperation::AttachmentUpsert { .. }))
    );

    let snapshot = projector.snapshot();
    assert_eq!(snapshot.attachments.len(), 1);
    assert_eq!(snapshot.tasks.len(), 1);
    assert!(snapshot.meta.agent.is_some());
}

#[test]
fn wire_form_subagent_lifecycle_folds_with_usage() {
    // The durable wire record (snake_case JSON) must parse into the typed
    // variant and fold — previously it fell through to `Custom` and was
    // dropped wholesale, so no task ever appeared.
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.spawned",
        "subagent_id": "sub-9",
        "subagent_name": "tower-worker",
        "parent_tool_call_id": "call-42",
        "run_in_background": true
    })));
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.completed",
        "subagent_id": "sub-9",
        "result_summary": "ok",
        "usage": {
            "input_tokens": 7,
            "output_tokens": 2,
            "total_tokens": 9,
            "input_cache_read": 1,
            "input_cache_creation": 0
        }
    })));
    let tasks = projector.snapshot().tasks;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, TaskState::Completed);
    assert_eq!(tasks[0].usage.as_ref().map(|u| u.input_other), Some(7));
}

#[test]
fn a_task_registered_after_the_spawn_adopts_the_placeholder() {
    // #3970 (Tower worker path): spawn first, task registration later —
    // the registered id must replace the agent-keyed placeholder, and the
    // terminal record (still keyed by agent id) must reach it.
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.spawned",
        "subagent_id": "agent-7",
        "run_in_background": true
    })));
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.task.created",
        "task": {
            "id": "task-77",
            "kind": "subagent",
            "agent_id": "agent-7",
            "status": "running",
            "description": "worker"
        }
    })));
    assert_eq!(projector.snapshot().tasks.len(), 1, "no duplicate task");
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.completed",
        "subagent_id": "agent-7",
        "result_summary": "done"
    })));
    let tasks = projector.snapshot().tasks;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].task_id, "task-77");
    assert_eq!(tasks[0].state, TaskState::Completed);
    assert_eq!(tasks[0].agent_id.as_deref(), Some("agent-7"));
}

/// The journal writers interleave: `TaskRunner` starts the subagent future
/// before it publishes the registration, so a fast subagent's terminal event
/// can be persisted *ahead* of `event.task.created(status = "running")`. A
/// last-writer-wins merge turned that ordering into a task stuck at
/// `running` forever.
#[test]
fn a_late_task_registration_cannot_resurrect_a_settled_task() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.spawned",
        "subagent_id": "agent-7",
        "run_in_background": true
    })));
    // Terminal first, registration second — the ordering the spawn-then-
    // publish path produces.
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.completed",
        "subagent_id": "agent-7",
        "result_summary": "done"
    })));
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.task.created",
        "task": {
            "id": "task-77",
            "kind": "subagent",
            "agent_id": "agent-7",
            "status": "running",
            "description": "worker"
        }
    })));

    let tasks = projector.snapshot().tasks;
    assert_eq!(tasks.len(), 1, "still one task");
    assert_eq!(
        tasks[0].state,
        TaskState::Completed,
        "the late registration must not reopen a finished task"
    );
    // The registration's own fields still land — only its state is refused.
    assert_eq!(tasks[0].description.as_deref(), Some("worker"));
    assert_eq!(tasks[0].task_id, "task-77");
}

/// A failure is a terminal outcome too: the generic task-completion event
/// that follows a failed subagent must not report it as completed.
#[test]
fn a_settled_failure_is_not_overwritten_by_a_completion() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.spawned",
        "subagent_id": "agent-8"
    })));
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.failed",
        "subagent_id": "agent-8",
        "error": "boom"
    })));
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.task.completed",
        "task": {
            "id": "agent-8",
            "kind": "subagent",
            "agent_id": "agent-8",
            "status": "completed",
            "output": "partial"
        }
    })));

    let tasks = projector.snapshot().tasks;
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].state,
        TaskState::Failed,
        "a failed subagent stays failed whatever the generic task event says"
    );
}

#[test]
fn cold_snapshot_settles_members_to_lost_by_the_spawning_turn() {
    // #3970: a member left running is lost unless the turn that spawned
    // it is itself still running in the rebuild.
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-1".into(),
        prompt: None,
    });
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.spawned",
        "subagent_id": "agent-a"
    })));
    projector.apply_event(&EngineEvent::TurnEnded {
        agent_id: "main".into(),
        turn_id: "turn-1".into(),
        reason: "completed".into(),
    });
    projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-2".into(),
        prompt: None,
    });
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.spawned",
        "subagent_id": "agent-b"
    })));

    // Live: both members legitimately stay running (background members
    // outlive their spawning turn and settle on their own events).
    let live = projector.snapshot().tasks;
    assert!(live.iter().all(|t| t.state == TaskState::Running));

    // Cold: agent-a's spawning turn ended → lost; agent-b's turn (t2)
    // still runs → survives.
    let cold = projector.cold_snapshot_tasks();
    let a = cold
        .iter()
        .find(|t| t.agent_id.as_deref() == Some("agent-a"))
        .unwrap();
    let b = cold
        .iter()
        .find(|t| t.agent_id.as_deref() == Some("agent-b"))
        .unwrap();
    assert_eq!(a.state, TaskState::Lost);
    assert_eq!(b.state, TaskState::Running);
}

/// Reproduces the production defect: the server path publishes
/// `event.assistant.delta` without ever emitting a typed `TurnStarted`, so
/// the projector had no cursor and dropped every streamed chunk. The delta
/// is self-describing (it carries `turn_id`, as v2's payload carries
/// `turnId`), so the text now reaches the transcript lane.
#[test]
fn custom_assistant_delta_streams_without_a_typed_turn_start() {
    let mut projector = TranscriptProjector::new();

    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.assistant.delta",
        "turn_id": 1,
        "message_id": "msg-a1-1",
        "content_index": 0,
        "delta": "hello",
    })));

    assert!(
        ops.iter()
            .any(|op| matches!(op, TranscriptOperation::Append { .. })),
        "a streamed assistant delta must reach the transcript lane; got {ops:?}"
    );
    match &ops[0] {
        TranscriptOperation::FrameUpsert { frame, .. } => assert!(
            matches!(frame, TranscriptFrame::Text(_)),
            "a text delta must open a text frame, got {frame:?}"
        ),
        other => panic!("expected frame.upsert first, got {other:?}"),
    }
}

/// A thinking chunk rides its own event name — v2 keeps `thinking.delta`
/// distinct from `assistant.delta` rather than flagging the answer stream
/// — and opens a Thinking frame, the distinction the web client reads to
/// render a reasoning block.
#[test]
fn custom_thinking_delta_opens_a_thinking_frame() {
    let mut projector = TranscriptProjector::new();

    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.thinking.delta",
        "turn_id": 1,
        "message_id": "msg-a1-1",
        "content_index": 0,
        "delta": "reasoning",
    })));

    match ops.first() {
        Some(TranscriptOperation::FrameUpsert { frame, .. }) => assert!(
            matches!(frame, TranscriptFrame::Thinking(_)),
            "a thinking delta must open a thinking frame, got {frame:?}"
        ),
        other => panic!("expected frame.upsert, got {other:?}"),
    }
}

/// The append offset counts UTF-16 code units, the unit JS counts: the
/// client rejects an offset past `text.length` as a gap and re-fetches the
/// transcript instead of splicing — so a byte offset makes every non-ASCII
/// answer arrive in one block rather than streaming, and a char count
/// still slices an astral character mid-surrogate-pair.
#[test]
fn append_offsets_count_utf16_units() {
    let mut projector = TranscriptProjector::new();

    let delta = |text: &str| {
        EngineEvent::Custom(json!({
            "type": "event.assistant.delta",
            "turn_id": 1,
            "message_id": "msg-a1-1",
            "content_index": 0,
            "delta": text,
        }))
    };

    projector.apply_event(&delta("长城"));
    let ops = projector.apply_event(&delta("的建造"));

    let append = ops
        .iter()
        .find(|op| matches!(op, TranscriptOperation::Append { .. }))
        .expect("second chunk must append");
    match append {
        TranscriptOperation::Append { offset, .. } => assert_eq!(
            *offset, 2,
            "offset must count the two characters already in the frame, \
                 not their UTF-8 byte length (6)"
        ),
        other => panic!("expected append, got {other:?}"),
    }

    // A third chunk continues from the code-unit count, so the client's
    // splice matches with no gap.
    let ops = projector.apply_event(&delta("历史"));
    match ops
        .iter()
        .find(|op| matches!(op, TranscriptOperation::Append { .. }))
        .expect("third chunk must append")
    {
        TranscriptOperation::Append { offset, .. } => assert_eq!(*offset, 5),
        other => panic!("expected append, got {other:?}"),
    }

    // An astral character is one char but two code units: the offset must
    // report 2 after a lone emoji, or the client's slice starts inside the
    // surrogate pair and the text renders corrupted.
    let mut emoji = TranscriptProjector::new();
    emoji.apply_event(&delta("😀"));
    let ops = emoji.apply_event(&delta("好"));
    match ops
        .iter()
        .find(|op| matches!(op, TranscriptOperation::Append { .. }))
        .expect("chunk after an emoji must append")
    {
        TranscriptOperation::Append { offset, .. } => assert_eq!(
            *offset, 2,
            "one emoji is two UTF-16 code units, not one char"
        ),
        other => panic!("expected append, got {other:?}"),
    }
}

/// A turn opened by a streamed delta is closed by the typed `turn.ended`
/// — the event v2 assigns the turn state to — and its step settles with
/// it: the client only stamps a thinking block's `durationMs` once the
/// step it belongs to has stopped running.
#[test]
fn turn_ended_closes_the_turn_and_its_steps() {
    let mut projector = TranscriptProjector::new();

    // A turn opened by deltas, the way the server path opens one.
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.assistant.delta",
        "turn_id": 4,
        "delta": "answer",
    })));
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.thinking.delta",
        "turn_id": 4,
        "delta": "reasoning",
    })));

    let ops = projector.apply_event(&EngineEvent::TurnEnded {
        agent_id: "main".into(),
        turn_id: "turn-4".into(),
        reason: "completed".into(),
    });

    let turn_op = ops
        .iter()
        .find(|op| matches!(op, TranscriptOperation::TurnUpsert { .. }))
        .expect("the finished turn must be republished");
    match turn_op {
        TranscriptOperation::TurnUpsert { turn } => {
            assert_eq!(turn.state, TurnState::Completed);
            assert!(turn.ended_at.is_some(), "a closed turn carries its end");
        }
        other => panic!("expected turn.upsert, got {other:?}"),
    }
    assert!(
        ops.iter()
            .any(|op| matches!(op, TranscriptOperation::StepUpsert { .. })),
        "the turn's steps must settle too: {ops:?}"
    );

    // The snapshot agrees, which is what the client reads.
    let snapshot = projector.snapshot();
    let turn = snapshot
        .items
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Turn(turn) => Some(turn),
            _ => None,
        })
        .expect("a turn exists");
    assert_eq!(turn.state, TurnState::Completed);
    assert!(
        turn.steps.iter().all(|s| s.state == StepState::Completed),
        "a clean turn's steps must settle completed, not interrupted: {:?}",
        turn.steps.iter().map(|s| s.state).collect::<Vec<_>>()
    );
}

/// `prompt.completed` owns no turn state here: v2 keeps the two entities on
/// separate schedules (`onPromptCompleted`, coreEventMap.ts:1356), so the
/// turn stays running until the typed `turn.ended` names it. Folding the
/// turn here would close it on the prompt's schedule instead.
#[test]
fn prompt_completed_does_not_close_the_turn() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.assistant.delta",
        "turn_id": 4,
        "delta": "answer",
    })));

    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "prompt.completed",
        "agentId": "main",
        "promptId": "prompt-1",
        "reason": "completed",
        "finishedAt": "2026-09-22T00:00:00Z",
    })));
    // The arm settles the prompt entity — v2 `onPromptCompleted`
    // (coreEventMap.ts:1356) emits exactly that one upsert — and folds
    // no turn state.
    match ops.as_slice() {
        [TranscriptOperation::PromptUpsert { prompt }] => {
            assert_eq!(prompt.status, TranscriptPromptStatus::Completed);
            assert!(
                prompt.finished_at.is_some(),
                "prompt.completed must settle finishedAt"
            );
        }
        other => panic!("expected exactly one prompt.upsert, got {other:?}"),
    }

    let snapshot = projector.snapshot();
    let turn = snapshot
        .items
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Turn(turn) => Some(turn),
            _ => None,
        })
        .expect("a turn exists");
    assert_eq!(
        turn.state,
        TurnState::Running,
        "the turn closes on turn.ended, not on prompt.completed"
    );
}

/// `turn.ended` closes the turn it names and leaves every other turn
/// alone — v2's turn state records a single `lastEnded` turn id, never a
/// broadcast over the running turns.
#[test]
fn turn_ended_closes_only_the_turn_it_names() {
    let mut projector = TranscriptProjector::new();
    for turn_id in [4, 5] {
        projector.apply_event(&EngineEvent::Custom(json!({
            "type": "event.assistant.delta",
            "turn_id": turn_id,
            "delta": "answer",
        })));
    }

    projector.apply_event(&EngineEvent::TurnEnded {
        agent_id: "main".into(),
        turn_id: "turn-5".into(),
        reason: "completed".into(),
    });

    let snapshot = projector.snapshot();
    let states: Vec<TurnState> = snapshot
        .items
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Turn(turn) => Some(turn.state),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![TurnState::Running, TurnState::Completed],
        "only the named turn may close; got {states:?}"
    );
}

/// A failed turn reports failure rather than success, with v2's reason
/// vocabulary on the typed event.
#[test]
fn a_failed_reason_closes_the_turn_as_failed() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.assistant.delta",
        "turn_id": 7,
        "delta": "partial",
    })));

    let ops = projector.apply_event(&EngineEvent::TurnEnded {
        agent_id: "main".into(),
        turn_id: "turn-7".into(),
        reason: "failed".into(),
    });

    match ops.first() {
        Some(TranscriptOperation::TurnUpsert { turn }) => {
            assert_eq!(turn.state, TurnState::Failed);
        }
        other => panic!("expected turn.upsert, got {other:?}"),
    }
}

/// v2's `turnEndReasonSchema` (turnOps.ts:97) carries `blocked` alongside
/// the three the fork's loop produces; a blocked turn must report failed
/// rather than silently reporting as completed. See `mapTurnEndState`
/// (coreEventMap.ts:1608) and the same fold in the cold rebuild
/// (`transcript/src/history/foldFacts.ts`, `mapTurnEndReason`).
#[test]
fn a_blocked_reason_closes_the_turn_as_failed() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.assistant.delta",
        "turn_id": 8,
        "delta": "partial",
    })));

    let ops = projector.apply_event(&EngineEvent::TurnEnded {
        agent_id: "main".into(),
        turn_id: "turn-8".into(),
        reason: "blocked".into(),
    });

    match ops.first() {
        Some(TranscriptOperation::TurnUpsert { turn }) => {
            assert_eq!(turn.state, TurnState::Failed);
        }
        other => panic!("expected turn.upsert, got {other:?}"),
    }
}

/// `turn.steer` buffers without emitting, then lands as a user frame at
/// the head of the next step — v2 `onTurnSteered` (coreEventMap.ts:1413)
/// buffered into `pendingSteers` and drained by `onStepStarted` (:568).
#[test]
fn turn_steer_buffers_until_the_next_step_begin() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-4".into(),
        prompt: Some("first".into()),
    });

    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "turn.steer",
        "agentId": "main",
        "origin": { "kind": "user" },
        "input": [{ "type": "text", "text": "also do X" }],
        "promptIds": ["prompt-2"],
    })));
    assert!(
        ops.is_empty(),
        "a steer buffers, emitting no op until a step begins; got {ops:?}"
    );

    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "llm.step.begin",
        "model": "kimi-k2",
    })));
    // The turn has no step yet: the flush opens one (StepUpsert) and the
    // user frame lands at its head (FrameUpsert).
    assert_eq!(ops.len(), 2, "step upsert + steer frame; got {ops:?}");
    assert!(matches!(ops[0], TranscriptOperation::StepUpsert { .. }));
    match &ops[1] {
        TranscriptOperation::FrameUpsert { frame, .. } => match frame {
            TranscriptFrame::Text(frame) => {
                assert_eq!(frame.role, TextRole::User);
                assert_eq!(frame.text, "also do X");
                assert_eq!(frame.prompt_ids, Some(vec!["prompt-2".to_string()]));
            }
            other => panic!("expected a text frame, got {other:?}"),
        },
        other => panic!("expected frame.upsert, got {other:?}"),
    }

    // The flushed user frame sits under the cursor: the next delta opens
    // a fresh assistant frame instead of appending to it.
    projector.apply_event(&EngineEvent::AssistantDelta {
        agent_id: "main".into(),
        turn_id: "turn-4".into(),
        delta: "on it".into(),
    });
    let snapshot = projector.snapshot();
    let turn = snapshot
        .items
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Turn(turn) => Some(turn),
            _ => None,
        })
        .expect("the turn exists");
    let frames = &turn.steps[0].frames;
    assert_eq!(frames.len(), 2, "steer frame then assistant frame");
    match (&frames[0], &frames[1]) {
        (TranscriptFrame::Text(user), TranscriptFrame::Text(assistant)) => {
            assert_eq!(user.role, TextRole::User);
            assert_eq!(assistant.role, TextRole::Assistant);
            assert_eq!(assistant.text, "on it");
        }
        other => panic!("expected two text frames, got {other:?}"),
    }
}

/// The flush is gated on a running turn (ROADMAP item 34): a steer
/// buffered after its turn ended survives the following step begin and
/// lands in the next running turn instead of a closed one.
#[test]
fn the_steer_flush_waits_for_a_running_turn() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-4".into(),
        prompt: Some("first".into()),
    });
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "llm.step.begin",
        "model": "kimi-k2",
    })));
    projector.apply_event(&EngineEvent::TurnEnded {
        agent_id: "main".into(),
        turn_id: "turn-4".into(),
        reason: "completed".into(),
    });

    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "turn.steer",
        "origin": { "kind": "user" },
        "input": [{ "type": "text", "text": "after the end" }],
        "promptIds": ["prompt-3"],
    })));
    assert!(ops.is_empty());

    // The cursor still points at the ended turn: no flush there.
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "llm.step.begin",
        "model": "kimi-k2",
    })));
    assert!(
        ops.is_empty(),
        "a closed turn collects no steer frames; got {ops:?}"
    );

    // The next running turn picks the buffered steer up.
    projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-5".into(),
        prompt: Some("second".into()),
    });
    let ops = projector.apply_event(&EngineEvent::LlmStepBegin {
        agent_id: "main".into(),
        turn_id: "5".into(),
        step: 1,
    });
    let frame_op = ops
        .iter()
        .find_map(|op| match op {
            TranscriptOperation::FrameUpsert { frame, .. } => Some(frame),
            _ => None,
        })
        .expect("the buffered steer lands in the running turn");
    match frame_op {
        TranscriptFrame::Text(frame) => {
            assert_eq!(frame.role, TextRole::User);
            assert_eq!(frame.text, "after the end");
        }
        other => panic!("expected a text frame, got {other:?}"),
    }

    let snapshot = projector.snapshot();
    let closed = snapshot
        .items
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Turn(turn) if turn.turn_id == "t4" => Some(turn),
            _ => None,
        })
        .expect("turn t4 exists");
    assert!(
        closed
            .steps
            .iter()
            .all(|step| step.frames.iter().all(|frame| {
                !matches!(frame, TranscriptFrame::Text(text) if text.role == TextRole::User)
            })),
        "the closed turn must not collect the steer frame"
    );
}

/// Only a user or skill-activation steer with an array input folds —
/// v2's `onTurnSteered` guard (coreEventMap.ts:1416).
#[test]
fn steer_folds_only_user_and_skill_activation_array_inputs() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-6".into(),
        prompt: Some("first".into()),
    });
    for payload in [
        json!({
            "type": "turn.steer",
            "origin": { "kind": "injection" },
            "input": [{ "type": "text", "text": "not the user" }],
        }),
        json!({
            "type": "turn.steer",
            "origin": { "kind": "user" },
            "input": "not an array",
        }),
        json!({ "type": "turn.steer", "input": [] }),
    ] {
        let ops = projector.apply_event(&EngineEvent::Custom(payload));
        assert!(ops.is_empty());
    }

    let ops = projector.apply_event(&EngineEvent::LlmStepBegin {
        agent_id: "main".into(),
        turn_id: "6".into(),
        step: 1,
    });
    assert_eq!(
        ops.len(),
        1,
        "only the step upsert — nothing was buffered; got {ops:?}"
    );
    assert!(matches!(ops[0], TranscriptOperation::StepUpsert { .. }));
}

/// v2 `onPromptSteered` (coreEventMap.ts:1384): the active prompt stays
/// open under the steer, each steered id settles as completed at the
/// steer's timestamp, and the submission's fields survive.
#[test]
fn steered_settles_the_steered_ids_and_keeps_the_active_open() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "prompt.submitted",
        "promptId": "prompt-1",
        "userMessageId": "msg-prompt-1",
        "status": "running",
        "content": [{ "type": "text", "text": "first" }],
        "createdAt": "2026-09-22T00:00:00Z",
        "metadata": { "note": "carry me" },
    })));

    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "prompt.steered",
        "activePromptId": "prompt-1",
        "promptIds": ["prompt-0"],
        "content": [{ "type": "text", "text": "follow-up" }],
        "steeredAt": "2026-09-22T00:00:01Z",
    })));
    assert_eq!(
        ops.len(),
        2,
        "active upsert + one per steered id; got {ops:?}"
    );

    let snapshot = projector.snapshot();
    assert_eq!(snapshot.prompts.len(), 2);
    let active = snapshot
        .prompts
        .iter()
        .find(|prompt| prompt.prompt_id == "prompt-1")
        .expect("the active prompt exists");
    assert_eq!(active.status, TranscriptPromptStatus::Running);
    assert_eq!(active.steered_at.as_deref(), Some("2026-09-22T00:00:01Z"));
    assert_eq!(
        active.created_at, "2026-09-22T00:00:00Z",
        "the steer must not move createdAt"
    );
    assert_eq!(
        active.client_metadata,
        Some(vec![json!({ "note": "carry me" })]),
        "the steer must not drop the client metadata"
    );
    let steered = snapshot
        .prompts
        .iter()
        .find(|prompt| prompt.prompt_id == "prompt-0")
        .expect("the steered prompt exists");
    assert_eq!(steered.status, TranscriptPromptStatus::Completed);
    assert_eq!(steered.finished_at.as_deref(), Some("2026-09-22T00:00:01Z"));
}

/// The route's `unwrap_or_default` can hand an empty `activePromptId`:
/// no entity may be created under the empty key, but the steered ids
/// still settle (ROADMAP item 34).
#[test]
fn steered_with_no_active_prompt_still_settles_the_ids() {
    let mut projector = TranscriptProjector::new();
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "prompt.steered",
        "activePromptId": "",
        "promptIds": ["prompt-3"],
        "content": [],
        "steeredAt": "2026-09-22T00:00:02Z",
    })));
    assert_eq!(ops.len(), 1, "only the steered id settles; got {ops:?}");

    let snapshot = projector.snapshot();
    assert_eq!(snapshot.prompts.len(), 1);
    assert_eq!(snapshot.prompts[0].prompt_id, "prompt-3");
    assert_eq!(
        snapshot.prompts[0].status,
        TranscriptPromptStatus::Completed
    );
}

/// v2 `onPromptAborted` (coreEventMap.ts:1371): abortedAt settles the
/// entity, createdAt falls back to it, and a later duplicate submission
/// must not resurrect a terminal prompt.
#[test]
fn prompt_aborted_settles_the_entity_and_survives_a_duplicate_submission() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "prompt.submitted",
        "promptId": "prompt-1",
        "userMessageId": "msg-prompt-1",
        "status": "running",
        "content": [{ "type": "text", "text": "first" }],
        "createdAt": "2026-09-22T00:00:00Z",
    })));
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "prompt.aborted",
        "promptId": "prompt-1",
        "abortedAt": "2026-09-22T00:00:03Z",
    })));
    assert_eq!(ops.len(), 1);

    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "prompt.submitted",
        "promptId": "prompt-1",
        "userMessageId": "msg-prompt-1",
        "status": "running",
        "content": [{ "type": "text", "text": "duplicate" }],
        "createdAt": "2026-09-22T00:00:04Z",
    })));

    let snapshot = projector.snapshot();
    let prompt = snapshot
        .prompts
        .iter()
        .find(|prompt| prompt.prompt_id == "prompt-1")
        .expect("the prompt exists");
    assert_eq!(
        prompt.status,
        TranscriptPromptStatus::Aborted,
        "a duplicate submission must not resurrect a terminal prompt"
    );
    assert_eq!(prompt.finished_at.as_deref(), Some("2026-09-22T00:00:03Z"));
    assert_eq!(
        prompt.created_at, "2026-09-22T00:00:00Z",
        "createdAt stays with the entity"
    );
}

/// `AgentStatusUpdated.usage` merges into the agent meta as the per-model
/// buckets plus their total and the current turn.
#[test]
fn agent_status_usage_merges_the_per_model_buckets() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "agent.status.updated",
        "model": "kimi-latest",
        "usage": {
            "byModel": {
                "kimi-latest": {
                    "inputOther": 10,
                    "output": 4,
                    "inputCacheRead": 2,
                    "inputCacheCreation": 1
                },
                "kimi-k2": {
                    "inputOther": 3,
                    "output": 1,
                    "inputCacheRead": 0,
                    "inputCacheCreation": 0
                }
            },
            "total": {
                "inputOther": 13,
                "output": 5,
                "inputCacheRead": 2,
                "inputCacheCreation": 1
            },
            "currentTurn": {
                "inputOther": 10,
                "output": 4,
                "inputCacheRead": 2,
                "inputCacheCreation": 1
            }
        }
    })));

    let snapshot = projector.snapshot();
    let agent = snapshot.meta.agent.expect("agent meta");
    let usage = agent.usage.expect("usage meta");
    let by_model = usage.by_model.expect("per-model buckets");
    assert_eq!(by_model["kimi-latest"].input_other, 10);
    assert_eq!(by_model["kimi-latest"].output, 4);
    assert_eq!(by_model["kimi-k2"].input_other, 3);
    assert_eq!(usage.total.expect("total").output, 5);
    assert_eq!(usage.current_turn.expect("current turn").output, 4);
}
