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
        block: None,
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
        block: None,
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
    use kimi_core::types::TokenUsage;

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

    let ops = projector.apply_event(&EngineEvent::ToolCallDelta {
        agent_id: "main".into(),
        turn_id: "turn-0".into(),
        tool_call_id: "c1".into(),
        name: None,
        arguments_part: Some("{\"path\"".into()),
        block: None,
    });
    match &ops[0] {
        TranscriptOperation::FrameUpsert {
            frame: TranscriptFrame::Tool(tool),
            ..
        } => assert_eq!(tool.input_text.as_deref(), Some("{\"path\"")),
        other => panic!("expected tool frame upsert, got {other:?}"),
    }

    let ops = projector.apply_event(&EngineEvent::LlmStepEnd {
        agent_id: "main".into(),
        turn_id: "t0".into(),
        step: 1,
        usage: Some(TokenUsage {
            input_other: 100,
            output: 20,
            input_cache_read: 5,
            input_cache_creation: 1,
            raw: None,
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

            let timing = step.timing.as_ref().expect("timing recorded");
            assert_eq!(timing.llm_first_token_latency_ms, Some(110));
            assert_eq!(timing.llm_stream_duration_ms, Some(100));
            assert_eq!(timing.llm_request_build_ms, Some(10));
            assert_eq!(timing.llm_server_first_token_ms, Some(100));
        }
        other => panic!("expected step upsert, got {other:?}"),
    }

    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "task.started",
        "info": {
            "taskId": "task_1",
            "kind": "process",
            "status": "running",
            "description": "bg",
            "startedAt": 1_000
        }
    })));
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "task.output",
        "taskId": "task_1",
        "chunk": "line1\n",
        "outputSizeBytes": 6
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
    let tasks = projector.snapshot().tasks;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].kind, TaskKind::Shell);
    assert_eq!(tasks[0].state, TaskState::Running);
    assert_eq!(tasks[0].description.as_deref(), Some("bg"));
    assert_eq!(
        tasks[0].started_at.as_deref(),
        Some("1970-01-01T00:00:01+00:00")
    );

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

    assert_eq!(
        attachments[0].source,
        Some(AttachmentSource::SessionMedia {
            file_id: "f_att1".to_string()
        })
    );
    assert_eq!(attachments[0].media_type, "image");

    assert_eq!(
        attachments[1].source,
        Some(AttachmentSource::File {
            file_id: "f_att2".to_string()
        })
    );

    assert_eq!(
        attachments[2].source,
        Some(AttachmentSource::Url {
            url: "https://example.test/remote.png".to_string()
        })
    );

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

    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.message.created",
        "message": {
            "id": "msg-u1",
            "role": "user",
            "content": [{ "type": "text", "text": "hi" }],
            "created_at": "2026-09-20T00:00:01.000Z",
        },
    })));

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

    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "agent.status.updated",
        "phase": { "kind": "running" }
    })));
    assert!(ops.is_empty());

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
        usage: Some(kimi_core::types::TokenUsage {
            input_other: 10,
            output: 4,
            input_cache_read: 3,
            input_cache_creation: 2,
            raw: None,
        }),
    });
    match &ops[0] {
        TranscriptOperation::TaskUpsert { task } => {
            assert_eq!(task.state, TaskState::Completed);
            assert_eq!(task.result_summary.as_deref(), Some("done"));

            let usage = task.usage.as_ref().expect("usage restored");
            assert_eq!(usage.input_other, 10);
            assert_eq!(usage.output, 4);
            assert_eq!(usage.input_cache_read, 3);
            assert_eq!(usage.input_cache_creation, 2);
        }
        other => panic!("expected task upsert, got {other:?}"),
    }

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
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.spawned",
        "subagent_id": "agent-7",
        "run_in_background": true
    })));
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "task.started",
        "info": {
            "taskId": "task-77",
            "kind": "agent",
            "agentId": "agent-7",
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

#[test]
fn a_late_task_registration_cannot_resurrect_a_settled_task() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.spawned",
        "subagent_id": "agent-7",
        "run_in_background": true
    })));

    projector.apply_event(&EngineEvent::from_json(json!({
        "type": "subagent.completed",
        "subagent_id": "agent-7",
        "result_summary": "done"
    })));
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "task.started",
        "info": {
            "taskId": "task-77",
            "kind": "agent",
            "agentId": "agent-7",
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

    assert_eq!(tasks[0].description.as_deref(), Some("worker"));
    assert_eq!(tasks[0].task_id, "task-77");
}

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
        "type": "task.terminated",
        "info": {
            "taskId": "agent-8",
            "kind": "agent",
            "agentId": "agent-8",
            "status": "completed"
        },
        "outputTail": "partial"
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
fn legacy_task_records_still_project_after_the_v2_rename() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.task.created",
        "task": {
            "id": "task-legacy",
            "kind": "bash",
            "status": "running",
            "description": "bg"
        }
    })));
    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.task.progress",
        "task_id": "task-legacy",
        "output_chunk": "old\n",
        "stream": "stdout"
    })));
    assert!(
        matches!(
            &ops[0],
            TranscriptOperation::Append {
                target: AppendTarget::Task { task_id },
                text,
                ..
            } if task_id == "task-legacy" && text == "old\n"
        ),
        "an older journal's task.progress still appends: {ops:?}"
    );
    projector.apply_event(&EngineEvent::Custom(json!({
        "type": "event.task.completed",
        "task_id": "task-legacy",
        "status": "completed",
        "output_preview": "old\n"
    })));

    let tasks = projector.snapshot().tasks;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, TaskState::Completed);
    assert_eq!(tasks[0].output_tail, "old\n");
}

#[test]
fn cold_snapshot_settles_members_to_lost_by_the_spawning_turn() {
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

    let live = projector.snapshot().tasks;
    assert!(live.iter().all(|t| t.state == TaskState::Running));

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

    let ops = projector.apply_event(&delta("历史"));
    match ops
        .iter()
        .find(|op| matches!(op, TranscriptOperation::Append { .. }))
        .expect("third chunk must append")
    {
        TranscriptOperation::Append { offset, .. } => assert_eq!(*offset, 5),
        other => panic!("expected append, got {other:?}"),
    }

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

#[test]
fn turn_ended_closes_the_turn_and_its_steps() {
    let mut projector = TranscriptProjector::new();

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

    projector.apply_event(&EngineEvent::AssistantDelta {
        agent_id: "main".into(),
        turn_id: "turn-4".into(),
        delta: "on it".into(),
        block: None,
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

    let ops = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "llm.step.begin",
        "model": "kimi-k2",
    })));
    assert!(
        ops.is_empty(),
        "a closed turn collects no steer frames; got {ops:?}"
    );

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

#[test]
fn a_new_turn_started_closes_the_previous_running_turn_as_wedged() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-1".into(),
        prompt: Some("first".into()),
    });

    let ops = projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-2".into(),
        prompt: Some("second".into()),
    });
    assert_eq!(ops.len(), 2, "wedged close plus the new turn: {ops:?}");
    let TranscriptOperation::TurnUpsert { turn: closed } = &ops[0] else {
        panic!("expected the wedged close first, got {:?}", ops[0]);
    };
    assert_eq!(closed.turn_id, "t1");
    assert_eq!(closed.state, TurnState::Failed);
    assert_eq!(closed.wedged, Some(true));
    assert!(closed.ended_at.is_some());
    let TranscriptOperation::TurnUpsert { turn: opened } = &ops[1] else {
        panic!("expected the new turn's upsert, got {:?}", ops[1]);
    };
    assert_eq!(opened.turn_id, "t2");
    assert_eq!(opened.state, TurnState::Running);
    assert_eq!(opened.wedged, None);

    let snapshot = projector.snapshot();
    let first = &snapshot.items[0];
    let TranscriptItem::Turn(first) = first else {
        panic!("expected a turn item");
    };
    assert_eq!(first.state, TurnState::Failed);
    assert_eq!(first.wedged, Some(true));
}

#[test]
fn a_cold_replay_restores_started_at_from_the_journal_record() {
    let mut projector = TranscriptProjector::new();
    let epoch_ms = 1_700_000_000_000i64;
    projector.apply_event_at(
        &EngineEvent::TurnStarted {
            agent_id: "main".into(),
            turn_id: "turn-3".into(),
            prompt: Some("cold".into()),
        },
        Some(epoch_ms),
    );

    let snapshot = projector.snapshot();
    let TranscriptItem::Turn(turn) = &snapshot.items[0] else {
        panic!("expected a turn item");
    };
    assert_eq!(
        turn.started_at.as_deref(),
        Some("2023-11-14T22:13:20+00:00"),
        "startedAt comes from the journal record, got {:?}",
        turn.started_at
    );
}

#[test]
fn a_cold_rebuild_marks_the_unclosed_tail_wedged_only_when_it_is_not_live() {
    let replay = |active: Option<i64>| {
        let mut projector = TranscriptProjector::new();
        projector.apply_event_at(
            &EngineEvent::TurnStarted {
                agent_id: "main".into(),
                turn_id: "turn-1".into(),
                prompt: Some("first".into()),
            },
            Some(1_000),
        );
        projector.apply_event_at(
            &EngineEvent::TurnStarted {
                agent_id: "main".into(),
                turn_id: "turn-2".into(),
                prompt: Some("second".into()),
            },
            Some(2_000),
        );
        projector.mark_wedged_turns(active);
        projector.snapshot()
    };

    let snapshot = replay(None);
    for item in &snapshot.items {
        let TranscriptItem::Turn(turn) = item else {
            panic!("expected a turn item");
        };
        assert_eq!(turn.state, TurnState::Failed, "{}: {turn:?}", turn.turn_id);
        assert_eq!(turn.wedged, Some(true), "{}: {turn:?}", turn.turn_id);
    }

    let snapshot = replay(Some(2));
    let TranscriptItem::Turn(first) = &snapshot.items[0] else {
        panic!("expected a turn item");
    };
    assert_eq!(first.state, TurnState::Failed);
    assert_eq!(first.wedged, Some(true));
    let TranscriptItem::Turn(tail) = &snapshot.items[1] else {
        panic!("expected a turn item");
    };
    assert_eq!(tail.turn_id, "t2");
    assert_eq!(tail.state, TurnState::Running);
    assert_eq!(tail.wedged, None);
}

#[test]
fn unparseable_turn_keys_claim_monotonic_fallback_ordinals() {
    let mut projector = TranscriptProjector::new();
    projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-5".into(),
        prompt: None,
    });
    projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "foreign".into(),
        prompt: None,
    });
    projector.apply_event(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "another-foreign".into(),
        prompt: None,
    });

    let snapshot = projector.snapshot();
    let ordinals: Vec<i64> = snapshot
        .items
        .iter()
        .map(|item| match item {
            TranscriptItem::Turn(turn) => turn.ordinal,
            other => panic!("expected a turn item, got {other:?}"),
        })
        .collect();
    assert_eq!(
        ordinals,
        vec![5, 6, 7],
        "fallbacks claim max+1 monotonically"
    );
    let foreign_ids: Vec<&str> = snapshot
        .items
        .iter()
        .map(|item| match item {
            TranscriptItem::Turn(turn) => turn.turn_id.as_str(),
            other => panic!("expected a turn item, got {other:?}"),
        })
        .collect();
    assert_eq!(foreign_ids, vec!["t5", "foreign", "another-foreign"]);
}

#[test]
fn an_unclosed_model_started_swarm_stays_out_of_cold_and_live_modes() {
    let event = EngineEvent::Custom(json!({
        "type": "swarm_mode.enter",
        "agentId": "main",
        "trigger": "tool",
    }));
    let mut live = TranscriptProjector::new();
    live.apply_event(&event);
    let mut cold = TranscriptProjector::new();
    cold.apply_event_at(&event, Some(1_000));

    let live_modes = live.snapshot().meta.modes;
    let cold_modes = cold.snapshot().meta.modes;
    assert_eq!(cold_modes, live_modes);
    assert_eq!(live_modes.and_then(|modes| modes.swarm), None);
}

#[test]
fn queued_and_started_fold_the_prompt_status() {
    let mut projector = TranscriptProjector::new();
    let queued = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "prompt.queued",
        "promptId": "prompt-q",
        "content": [{ "type": "text", "text": "second" }],
        "queueLength": 1,
    })));
    let TranscriptOperation::PromptUpsert { prompt } = &queued[0] else {
        panic!("expected a prompt upsert, got {queued:?}");
    };
    assert_eq!(prompt.status, TranscriptPromptStatus::Queued);
    assert_eq!(
        prompt.content,
        Some(json!([{ "type": "text", "text": "second" }]))
    );

    let started = projector.apply_event(&EngineEvent::Custom(json!({
        "type": "prompt.started",
        "promptId": "prompt-q",
    })));
    let TranscriptOperation::PromptUpsert { prompt } = &started[0] else {
        panic!("expected a prompt upsert, got {started:?}");
    };
    assert_eq!(prompt.status, TranscriptPromptStatus::Running);
    assert_eq!(
        prompt.content,
        Some(json!([{ "type": "text", "text": "second" }]))
    );
    assert!(prompt.finished_at.is_none());
}
