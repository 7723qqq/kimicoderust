use serde_json::{Value, json};

use crate::events::EngineEvent;
use crate::session::sqlite_store::SqliteSessionStore;
use crate::turn_loop::types::LLMMessage;

pub mod grade;
pub mod model;
pub mod ops;
pub mod project;

pub const DEFAULT_PAGE_SIZE: usize = 20;

#[derive(Clone)]
struct StepDraft {
    step_id: String,
    ordinal: usize,
    frames: Vec<Value>,
}

#[derive(Clone)]
struct ColdSteer {
    text: String,
    prompt_id: String,
}

#[derive(Clone)]
struct TurnDraft {
    turn_id: String,
    ordinal: usize,
    prompt: Option<String>,

    origin: Value,
    steps: Vec<StepDraft>,
}

impl TurnDraft {
    fn new(ordinal: usize, prompt: Option<String>, origin: Value) -> Self {
        Self {
            turn_id: format!("t{ordinal}"),
            ordinal,
            prompt,
            origin,
            steps: Vec::new(),
        }
    }

    fn to_value(&self) -> Value {
        let steps: Vec<Value> = self
            .steps
            .iter()
            .map(|step| {
                json!({
                    "kind": "step",
                    "stepId": step.step_id,
                    "turnId": self.turn_id,
                    "ordinal": step.ordinal,
                    "state": "completed",
                    "frames": step.frames,
                })
            })
            .collect();
        let mut turn = json!({
            "kind": "turn",
            "turnId": self.turn_id,
            "ordinal": self.ordinal,
            "state": "completed",
            "origin": self.origin,
            "steps": steps,
        });
        if let Some(prompt) = self.prompt.as_ref()
            && !prompt.is_empty()
        {
            turn["prompt"] = json!(prompt);
        }
        turn
    }
}

fn flush_steered_leftovers(
    turns: &mut Vec<TurnDraft>,
    next_ordinal: &mut usize,
    pending: &mut Vec<ColdSteer>,
) {
    if pending.is_empty() {
        return;
    }
    if turns.is_empty() {
        turns.push(TurnDraft::new(
            *next_ordinal,
            None,
            json!({ "kind": "user" }),
        ));
        *next_ordinal += 1;
    }
    let turn = turns.last_mut().expect("turn ensured above");
    if turn.steps.is_empty() {
        turn.steps.push(StepDraft {
            step_id: format!("{}.1", turn.turn_id),
            ordinal: 1,
            frames: Vec::new(),
        });
    }
    let step = turn.steps.last_mut().expect("step ensured above");
    for steer in pending.drain(..) {
        let frame_id = format!("{}.f{}", step.step_id, step.frames.len() + 1);
        step.frames.push(json!({
            "kind": "text",
            "frameId": frame_id,
            "role": "user",
            "text": steer.text,
            "promptIds": [steer.prompt_id],
            "origin": { "kind": "user" },
        }));
    }
}

pub fn build_items(history: &[LLMMessage]) -> Vec<Value> {
    let mut turns: Vec<TurnDraft> = Vec::new();
    let mut next_ordinal = 0usize;
    let mut pending: Vec<ColdSteer> = Vec::new();

    for message in history {
        match message.role.as_str() {
            "system" => continue,
            "user" => {
                if let Some(prompt_id) = message.prompt_id.as_deref() {
                    pending.push(ColdSteer {
                        text: message.content.clone(),
                        prompt_id: prompt_id.to_string(),
                    });
                    continue;
                }

                let is_injection = crate::injection::is_system_reminder(&message.content);
                let (prompt, origin) = if is_injection {
                    (None, json!({ "kind": "other" }))
                } else if message.content.is_empty() {
                    (None, json!({ "kind": "user" }))
                } else {
                    (Some(message.content.clone()), json!({ "kind": "user" }))
                };
                flush_steered_leftovers(&mut turns, &mut next_ordinal, &mut pending);
                turns.push(TurnDraft::new(next_ordinal, prompt, origin));
                next_ordinal += 1;
            }
            "assistant" => {
                if turns.is_empty() {
                    turns.push(TurnDraft::new(
                        next_ordinal,
                        None,
                        json!({ "kind": "other" }),
                    ));
                    next_ordinal += 1;
                }
                let turn = turns.last_mut().expect("turn ensured above");
                let step_ordinal = turn.steps.len() + 1;
                let step_id = format!("{}.{}", turn.turn_id, step_ordinal);
                let mut frames: Vec<Value> = Vec::new();
                let mut frame_count = 0usize;
                let mut next_frame_id = || {
                    frame_count += 1;
                    format!("{step_id}.f{frame_count}")
                };

                for steer in pending.drain(..) {
                    frames.push(json!({
                        "kind": "text",
                        "frameId": next_frame_id(),
                        "role": "user",
                        "text": steer.text,
                        "promptIds": [steer.prompt_id],
                        "origin": { "kind": "user" },
                    }));
                }
                if !message.content.is_empty() {
                    frames.push(json!({
                        "kind": "text",
                        "frameId": next_frame_id(),
                        "role": "assistant",
                        "text": message.content,
                    }));
                }
                for call in &message.tool_calls {
                    frames.push(json!({
                        "kind": "tool",
                        "frameId": format!("{step_id}.{}", call.id),
                        "toolCallId": call.id,
                        "name": call.name,
                        "state": "running",
                        "input": crate::llm::tool_args::parse_tool_arguments(call.arguments.as_deref()).data,
                    }));
                }
                turn.steps.push(StepDraft {
                    step_id,
                    ordinal: step_ordinal,
                    frames,
                });
            }
            "tool" => {
                let Some(turn) = turns.last_mut() else {
                    continue;
                };
                let Some(tool_call_id) = message.tool_call_id.as_deref() else {
                    continue;
                };
                patch_tool_frame(turn, tool_call_id, &message.content);
            }
            _ => {}
        }
    }

    flush_steered_leftovers(&mut turns, &mut next_ordinal, &mut pending);

    turns.iter().map(TurnDraft::to_value).collect()
}

pub fn cold_items(
    store: &SqliteSessionStore,
    session_id: &str,
    history: &[LLMMessage],
) -> Vec<Value> {
    let mut items = build_items(history);
    let axis = crate::session::wire_journal::turn_prompt_axis(store, session_id)
        .or_else(|| {
            let records = store.turn_prompt_wire_events(session_id).ok()?;
            let axis: Vec<(i64, String)> = records
                .iter()
                .filter_map(|record| {
                    let wire_id = record.payload.get("turnId")?.as_i64()?;
                    let text = record
                        .payload
                        .get("input")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<String>()
                        })
                        .unwrap_or_default();
                    Some((wire_id, text))
                })
                .collect();
            (!axis.is_empty()).then_some(axis)
        })
        .unwrap_or_default();
    if axis.is_empty() {
        return items;
    }
    realign_wire_turn_ids(&mut items, &axis);
    items
}

fn realign_wire_turn_ids(items: &mut [Value], axis: &[(i64, String)]) {
    let mut claimed = vec![false; axis.len()];
    let mut matched: Vec<Option<i64>> = Vec::with_capacity(items.len());
    for item in items.iter() {
        let prompt = item.get("prompt").and_then(Value::as_str).unwrap_or("");
        let claim = if prompt.is_empty() {
            None
        } else {
            axis.iter()
                .enumerate()
                .find(|(idx, (_, text))| !claimed[*idx] && text == prompt)
                .map(|(idx, _)| idx)
        };
        if let Some(idx) = claim {
            claimed[idx] = true;
        }
        matched.push(claim.map(|idx| axis[idx].0));
    }
    let kept: std::collections::HashSet<i64> = items
        .iter()
        .zip(&matched)
        .filter(|(_, wire)| wire.is_none())
        .filter_map(|(item, _)| item.get("ordinal").and_then(Value::as_i64))
        .collect();
    for (item, wire) in items.iter_mut().zip(&matched) {
        let Some(wire_id) = wire else {
            continue;
        };
        let ordinal = item.get("ordinal").and_then(Value::as_i64).unwrap_or(-1);
        if ordinal == *wire_id || kept.contains(wire_id) {
            continue;
        }
        let from = item
            .get("turnId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let to = format!("t{wire_id}");
        item["turnId"] = json!(to);
        item["ordinal"] = json!(wire_id);
        rename_nested_ids(item, &from, &to);
    }
}

fn rename_nested_ids(turn: &mut Value, from: &str, to: &str) {
    let Some(steps) = turn.get_mut("steps").and_then(Value::as_array_mut) else {
        return;
    };
    for step in steps {
        let old_step_id = step
            .get("stepId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let new_step_id = old_step_id
            .strip_prefix(&format!("{from}."))
            .map_or(old_step_id.clone(), |rest| format!("{to}.{rest}"));
        step["stepId"] = json!(new_step_id);
        step["turnId"] = json!(to);
        let Some(frames) = step.get_mut("frames").and_then(Value::as_array_mut) else {
            continue;
        };
        for frame in frames {
            let frame_id = frame
                .get("frameId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(rest) = frame_id.strip_prefix(&format!("{old_step_id}.")) {
                frame["frameId"] = json!(format!("{new_step_id}.{rest}"));
            }
        }
    }
}

pub fn cold_prompts(
    store: &SqliteSessionStore,
    session_id: &str,
    active_wire_turn_id: Option<i64>,
) -> Value {
    let records = crate::session::wire_journal::restorable_session_records(store, session_id);
    if !records.is_empty() {
        let mut projector = project::TranscriptProjector::new();
        for record in records {
            let Some(kind) = record.get("type").and_then(Value::as_str) else {
                continue;
            };
            if !matches!(
                kind,
                "prompt.submitted" | "prompt.completed" | "prompt.aborted" | "prompt.steered"
            ) {
                continue;
            }
            let at = record.get("time").and_then(Value::as_i64);
            projector.apply_event_at(&EngineEvent::from_json(record), at);
        }
        projector.mark_wedged_turns(active_wire_turn_id);
        let prompts = projector.snapshot().prompts;
        return serde_json::to_value(&prompts).unwrap_or(Value::Array(Vec::new()));
    }
    let Ok(records) = store.prompt_wire_events(session_id) else {
        return Value::Array(Vec::new());
    };
    let mut projector = project::TranscriptProjector::new();
    for record in records {
        projector.apply_event_at(
            &EngineEvent::from_json(record.payload),
            Some(record.created_at),
        );
    }
    projector.mark_wedged_turns(active_wire_turn_id);
    let prompts = projector.snapshot().prompts;
    serde_json::to_value(&prompts).unwrap_or(Value::Array(Vec::new()))
}

pub fn cold_tasks(
    store: &SqliteSessionStore,
    session_id: &str,
    active_wire_turn_id: Option<i64>,
) -> Value {
    if let Some(records) = crate::session::wire_journal::all_agent_records(store, session_id) {
        let mut projector = project::TranscriptProjector::new();
        for record in records {
            let Some(kind) = record.get("type").and_then(Value::as_str) else {
                continue;
            };
            if !matches!(
                kind,
                "subagent.spawned"
                    | "subagent.completed"
                    | "subagent.failed"
                    | "task.started"
                    | "task.terminated"
                    | "task.output"
                    | "event.task.created"
                    | "event.task.completed"
                    | "event.task.progress"
                    | "turn.started"
                    | "turn.ended"
            ) {
                continue;
            }
            let at = record.get("time").and_then(Value::as_i64);
            projector.apply_event_at(&EngineEvent::from_json(cold_fold_payload(record)), at);
        }
        projector.mark_wedged_turns(active_wire_turn_id);
        let tasks = projector.cold_snapshot_tasks();
        return serde_json::to_value(&tasks).unwrap_or(Value::Array(Vec::new()));
    }
    let Ok(records) = store.task_wire_events(session_id) else {
        return Value::Array(Vec::new());
    };
    let mut projector = project::TranscriptProjector::new();
    for record in records {
        projector.apply_event_at(
            &EngineEvent::from_json(cold_fold_payload(record.payload)),
            Some(record.created_at),
        );
    }
    projector.mark_wedged_turns(active_wire_turn_id);
    let tasks = projector.cold_snapshot_tasks();
    serde_json::to_value(&tasks).unwrap_or(Value::Array(Vec::new()))
}

const MAIN_AGENT_ID: &str = "main";

fn cold_fold_payload(payload: Value) -> Value {
    let mut value = payload;
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return value;
    };
    if !matches!(kind, "turn.started" | "turn.ended") {
        return value;
    }
    if let Some(object) = value.as_object_mut() {
        if let Some(turn_id) = object.remove("turnId") {
            let normalized = match turn_id {
                Value::String(text) => Value::String(text),
                other => Value::String(other.to_string()),
            };
            object.entry("turn_id").or_insert(normalized);
        }
        object
            .entry("agent_id")
            .or_insert_with(|| Value::String(MAIN_AGENT_ID.to_string()));
    }
    value
}

fn patch_tool_frame(turn: &mut TurnDraft, tool_call_id: &str, output: &str) {
    for step in turn.steps.iter_mut().rev() {
        for frame in step.frames.iter_mut().rev() {
            if frame.get("kind").and_then(|v| v.as_str()) == Some("tool")
                && frame.get("toolCallId").and_then(|v| v.as_str()) == Some(tool_call_id)
            {
                frame["state"] = json!("done");
                frame["output"] = json!(output);
                return;
            }
        }
    }
}

fn turn_ordinal(item: &Value) -> Option<i64> {
    item.get("ordinal").and_then(|v| v.as_i64())
}

pub fn paginate_turns(items: &[Value], query: &TurnPageQuery) -> (Vec<Value>, bool) {
    let page_size = query.page_size.max(1);
    let turns: Vec<&Value> = items
        .iter()
        .filter(|item| item.get("kind").and_then(|v| v.as_str()) == Some("turn"))
        .collect();
    if turns.is_empty() {
        return (Vec::new(), false);
    }

    if let Some(after) = query.after_turn.as_deref() {
        let after_ordinal = parse_turn_ordinal(after);
        let selected: Vec<&Value> = turns
            .into_iter()
            .filter(|item| match (turn_ordinal(item), after_ordinal) {
                (Some(value), Some(after)) => value > after,
                _ => false,
            })
            .take(page_size)
            .collect();
        let has_more = selected.len() == page_size;
        return (selected.into_iter().cloned().collect(), has_more);
    }

    if let Some(before) = query.before_turn.as_deref() {
        let before_ordinal = parse_turn_ordinal(before);
        let older: Vec<&Value> = turns
            .into_iter()
            .filter(|item| match (turn_ordinal(item), before_ordinal) {
                (Some(value), Some(before)) => value < before,
                _ => false,
            })
            .collect();
        let start = older.len().saturating_sub(page_size);
        let selected = older[start..].to_vec();
        let has_more = older.len() > selected.len();
        return (selected.into_iter().cloned().collect(), has_more);
    }

    let start = turns.len().saturating_sub(page_size);
    let selected = turns[start..].to_vec();
    let has_more = turns.len() > selected.len();
    (selected.into_iter().cloned().collect(), has_more)
}

fn parse_turn_ordinal(turn_id: &str) -> Option<i64> {
    turn_id
        .strip_prefix('t')
        .and_then(|rest| rest.parse::<i64>().ok())
}

pub fn project_user_messages(items: &[Value]) -> Vec<Value> {
    let mut messages = Vec::new();
    for item in items {
        if item.get("kind").and_then(|v| v.as_str()) != Some("turn") {
            continue;
        }
        let prompt = item.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        if prompt.is_empty() {
            continue;
        }
        messages.push(json!({
            "turn_id": item.get("turnId").cloned().unwrap_or(Value::Null),
            "ordinal": item.get("ordinal").cloned().unwrap_or(json!(0)),
            "state": item.get("state").cloned().unwrap_or(json!("completed")),
            "origin": item.get("origin").cloned().unwrap_or(json!({ "kind": "user" })),
            "prompt": prompt,
        }));
    }
    messages
}

const PLAN_SAVED_TO_MARKER: &str = "Plan saved to: ";
const PLAN_BODY_MARKERS: [&str; 2] = [
    "## Approved Plan:\n",
    "## Plan (auto-approved, not user-reviewed):\n",
];

pub fn project_plans(items: &[Value], tool_call_id: Option<&str>) -> Vec<Value> {
    let mut plans = Vec::new();
    for item in items {
        if item.get("kind").and_then(|v| v.as_str()) != Some("turn") {
            continue;
        }
        let turn_id = item.get("turnId").and_then(|v| v.as_str()).unwrap_or("");
        let Some(steps) = item.get("steps").and_then(|v| v.as_array()) else {
            continue;
        };
        for step in steps {
            let Some(frames) = step.get("frames").and_then(|v| v.as_array()) else {
                continue;
            };
            for frame in frames {
                if frame.get("kind").and_then(|v| v.as_str()) != Some("tool") {
                    continue;
                }
                if frame.get("name").and_then(|v| v.as_str()) != Some("ExitPlanMode") {
                    continue;
                }
                let frame_call_id = frame
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(only) = tool_call_id
                    && frame_call_id != only
                {
                    continue;
                }
                let output = frame.get("output").and_then(|v| v.as_str());
                if let Some((plan, path)) = parse_plan_from_output(output) {
                    let mut entry = json!({
                        "tool_call_id": frame_call_id,
                        "turn_id": turn_id,
                        "source": "output",
                        "plan": plan,
                    });
                    if let Some(path) = path {
                        entry["path"] = json!(path);
                    }
                    plans.push(entry);
                }
            }
        }
    }
    plans
}

fn parse_plan_from_output(output: Option<&str>) -> Option<(String, Option<String>)> {
    let output = output?;
    let mut path: Option<String> = None;
    for line in output.split('\n') {
        if let Some(rest) = line.strip_prefix(PLAN_SAVED_TO_MARKER) {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                path = Some(trimmed.to_string());
            }
            break;
        }
    }
    for marker in PLAN_BODY_MARKERS {
        if let Some(index) = output.find(marker) {
            let plan = &output[index + marker.len()..];
            if !plan.trim().is_empty() {
                return Some((plan.to_string(), path));
            }
        }
    }
    None
}

pub struct TurnPageQuery {
    pub before_turn: Option<String>,
    pub after_turn: Option<String>,
    pub page_size: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_loop::types::ToolCall;

    fn user(text: &str) -> LLMMessage {
        LLMMessage::new(kimi_core::types::Role::User, text)
    }

    fn assistant(text: &str, calls: Vec<ToolCall>) -> LLMMessage {
        let mut message = LLMMessage::new(kimi_core::types::Role::Assistant, text);
        message.tool_calls = calls;
        message
    }

    fn tool(call_id: &str, output: &str) -> LLMMessage {
        let mut message = LLMMessage::new(kimi_core::types::Role::Tool, output);
        message.tool_call_id = Some(call_id.to_string());
        message
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: Some(json!({ "path": "a.txt" }).to_string()),
            extras: None,
            raw_id: None,
            _stream_index: None,
        }
    }

    fn steer(text: &str, prompt_id: &str) -> LLMMessage {
        let mut message = LLMMessage::new(kimi_core::types::Role::User, text);
        message.prompt_id = Some(prompt_id.to_string());
        message
    }

    #[test]
    fn a_steered_follow_up_opens_no_turn_and_lands_at_the_step_head() {
        let history = vec![
            user("first"),
            steer("also do X", "prompt-2"),
            assistant("on it", Vec::new()),
        ];
        let items = build_items(&history);
        assert_eq!(items.len(), 1, "the steer opens no turn of its own");

        let steps = items[0]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        let frames = steps[0]["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 2, "steer frame then assistant frame");
        assert_eq!(frames[0]["role"], "user");
        assert_eq!(frames[0]["text"], "also do X");
        assert_eq!(frames[0]["promptIds"][0], "prompt-2");
        assert_eq!(frames[0]["origin"]["kind"], "user");
        assert_eq!(frames[1]["role"], "assistant");
        assert_eq!(frames[1]["text"], "on it");

        assert_eq!(items[0]["prompt"], "first");
    }

    #[test]
    fn a_new_turn_flushes_the_steers_left_behind() {
        let history = vec![
            user("first"),
            assistant("ok", Vec::new()),
            steer("mid", "prompt-4"),
            user("second"),
            assistant("done", Vec::new()),
        ];
        let items = build_items(&history);
        assert_eq!(items.len(), 2, "neither the steer nor the flush adds turns");

        let first_steps = items[0]["steps"].as_array().unwrap();
        let last_step = first_steps.last().unwrap();
        let frames = last_step["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 2, "assistant frame then the flushed steer");
        assert_eq!(frames[1]["role"], "user");
        assert_eq!(frames[1]["text"], "mid");
        assert_eq!(frames[1]["promptIds"][0], "prompt-4");

        assert_eq!(items[1]["prompt"], "second");
    }

    #[test]
    fn a_steer_before_any_turn_opens_a_user_placeholder() {
        let history = vec![steer("early", "prompt-5"), user("hello")];
        let items = build_items(&history);
        assert_eq!(items.len(), 2);

        assert_eq!(items[0]["origin"]["kind"], "user");
        assert!(
            items[0].get("prompt").is_none(),
            "the placeholder carries no prompt of its own"
        );
        let steps = items[0]["steps"].as_array().unwrap();
        let frames = steps[0]["frames"].as_array().unwrap();
        assert_eq!(frames[0]["role"], "user");
        assert_eq!(frames[0]["text"], "early");
        assert_eq!(frames[0]["promptIds"][0], "prompt-5");

        assert_eq!(items[1]["prompt"], "hello");
    }

    #[test]
    fn a_steer_at_the_end_of_history_lands_in_the_last_step() {
        let history = vec![
            user("first"),
            assistant("ok", Vec::new()),
            steer("one more", "prompt-6"),
        ];
        let items = build_items(&history);
        assert_eq!(items.len(), 1);

        let steps = items[0]["steps"].as_array().unwrap();
        let frames = steps.last().unwrap()["frames"].as_array().unwrap();
        let last = frames.last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(last["text"], "one more");
        assert_eq!(last["promptIds"][0], "prompt-6");
    }

    #[test]
    fn history_of_only_a_steer_opens_a_user_turn() {
        let items = build_items(&[steer("just a steer", "prompt-7")]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["origin"]["kind"], "user");
        let steps = items[0]["steps"].as_array().unwrap();
        let frames = steps[0]["frames"].as_array().unwrap();
        assert_eq!(frames[0]["text"], "just a steer");
    }

    #[test]
    fn cold_prompts_folds_the_persisted_lifecycle_journal() {
        use crate::native::event_store::RawWireEvent;
        use crate::session::sqlite_store::SqliteSessionStore;

        let store = SqliteSessionStore::in_memory().unwrap();
        store
            .create_session("sess-cold-prompts", Some("Cold"))
            .unwrap();
        let append = |event_type: &str, payload: Value| {
            store
                .append_wire_event(&RawWireEvent {
                    id: format!("evt-{event_type}"),
                    session_id: "sess-cold-prompts".into(),
                    event_type: event_type.into(),
                    payload,
                    is_checkpoint: false,
                    is_compaction: false,
                    created_at: 1_000,
                })
                .unwrap();
        };
        append(
            "prompt.submitted",
            json!({
                "type": "prompt.submitted",
                "promptId": "prompt-1",
                "userMessageId": "msg-prompt-1",
                "status": "running",
                "content": [{ "type": "text", "text": "first" }],
                "createdAt": "2026-09-22T00:00:00Z",
            }),
        );
        append(
            "event.message.created",
            json!({ "type": "event.message.created", "id": "msg-u1" }),
        );
        append(
            "prompt.aborted",
            json!({
                "type": "prompt.aborted",
                "promptId": "prompt-1",
                "abortedAt": "2026-09-22T00:00:03Z",
            }),
        );
        append(
            "turn.steer",
            json!({ "type": "turn.steer", "origin": { "kind": "user" } }),
        );

        let prompts = cold_prompts(&store, "sess-cold-prompts", None);
        let prompts = prompts.as_array().expect("prompts serialize to an array");
        assert_eq!(prompts.len(), 1, "only the prompt lifecycle folds");
        assert_eq!(prompts[0]["promptId"], "prompt-1");
        assert_eq!(prompts[0]["status"], "aborted");
        assert_eq!(prompts[0]["finishedAt"], "2026-09-22T00:00:03Z");
        assert_eq!(prompts[0]["createdAt"], "2026-09-22T00:00:00Z");

        store.create_session("sess-cold-empty", None).unwrap();
        assert_eq!(cold_prompts(&store, "sess-cold-empty", None), json!([]));
    }

    #[test]
    fn cold_tasks_restores_tasks_and_settles_lost_members() {
        use crate::native::event_store::RawWireEvent;
        use crate::session::sqlite_store::SqliteSessionStore;

        let store = SqliteSessionStore::in_memory().unwrap();
        store
            .create_session("sess-cold-tasks", Some("Cold"))
            .unwrap();
        let seq = std::cell::Cell::new(0u32);
        let append = |event_type: &str, payload: Value| {
            seq.set(seq.get() + 1);
            store
                .append_wire_event(&RawWireEvent {
                    id: format!("evt-{}", seq.get()),
                    session_id: "sess-cold-tasks".into(),
                    event_type: event_type.into(),
                    payload,
                    is_checkpoint: false,
                    is_compaction: false,
                    created_at: 1_000,
                })
                .unwrap();
        };
        append(
            "turn.started",
            json!({ "type": "turn.started", "turnId": 1, "origin": Value::Null }),
        );

        append(
            "subagent.spawned",
            json!({ "type": "subagent.spawned", "subagent_id": "agent-7", "run_in_background": true }),
        );
        append(
            "event.task.created",
            json!({
                "type": "event.task.created",
                "task": {
                    "id": "task-77",
                    "kind": "subagent",
                    "agent_id": "agent-7",
                    "status": "running",
                    "description": "worker"
                }
            }),
        );
        append(
            "subagent.completed",
            json!({
                "type": "subagent.completed",
                "subagent_id": "agent-7",
                "result_summary": "done",
                "usage": { "input_tokens": 7, "output_tokens": 2, "total_tokens": 9 }
            }),
        );

        append(
            "subagent.spawned",
            json!({ "type": "subagent.spawned", "subagent_id": "agent-8" }),
        );
        append(
            "turn.ended",
            json!({ "type": "turn.ended", "turnId": 1, "reason": "completed" }),
        );

        let tasks = cold_tasks(&store, "sess-cold-tasks", None);
        let tasks = tasks.as_array().expect("tasks serialize to an array");
        assert_eq!(tasks.len(), 2, "adoption keeps a single task per member");

        let adopted = tasks.iter().find(|t| t["taskId"] == "task-77").unwrap();
        assert_eq!(adopted["state"], "completed");
        assert_eq!(adopted["agentId"], "agent-7");
        assert_eq!(adopted["usage"]["inputOther"], 7);

        let lost = tasks.iter().find(|t| t["taskId"] == "agent-8").unwrap();
        assert_eq!(lost["state"], "lost");

        store
            .create_session("sess-cold-running", Some("Running"))
            .unwrap();
        let seq = std::cell::Cell::new(100u32);
        let append_running = |event_type: &str, payload: Value| {
            seq.set(seq.get() + 1);
            store
                .append_wire_event(&RawWireEvent {
                    id: format!("evt-run-{}", seq.get()),
                    session_id: "sess-cold-running".into(),
                    event_type: event_type.into(),
                    payload,
                    is_checkpoint: false,
                    is_compaction: false,
                    created_at: 1_000,
                })
                .unwrap();
        };
        append_running(
            "turn.started",
            json!({ "type": "turn.started", "turnId": 2, "origin": Value::Null }),
        );
        append_running(
            "subagent.spawned",
            json!({ "type": "subagent.spawned", "subagent_id": "agent-9" }),
        );
        let running = cold_tasks(&store, "sess-cold-running", Some(2));
        let running = running.as_array().expect("tasks serialize to an array");
        assert_eq!(running.len(), 1);
        assert_eq!(
            running[0]["state"], "running",
            "a member of a still-running turn is not lost"
        );

        let wedged = cold_tasks(&store, "sess-cold-running", None);
        let wedged = wedged.as_array().expect("tasks serialize to an array");
        assert_eq!(
            wedged[0]["state"], "lost",
            "a member of a wedged (dead, unclosed) turn settles to lost"
        );

        store.create_session("sess-cold-tasks-empty", None).unwrap();
        assert_eq!(cold_tasks(&store, "sess-cold-tasks-empty", None), json!([]));
    }

    #[test]
    fn injected_reminders_are_not_user_prompts() {
        let history = vec![
            user("hello"),
            assistant("hi", Vec::new()),
            LLMMessage::new(
                "user",
                crate::injection::wrap_system_reminder("Today's date is 2026-09-20."),
            ),
            assistant("acknowledged", Vec::new()),
        ];
        let items = build_items(&history);
        assert_eq!(items.len(), 2);

        assert_eq!(items[0]["prompt"], "hello");
        assert_eq!(items[0]["origin"]["kind"], "user");

        assert_eq!(items[1]["origin"]["kind"], "other");
        assert!(
            items[1].get("prompt").is_none(),
            "an injected reminder must not carry a prompt"
        );

        assert_eq!(items[1]["steps"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn groups_history_into_turns_steps_and_frames() {
        let history = vec![
            user("hello"),
            assistant("hi", vec![call("c1", "Read")]),
            tool("c1", "file contents"),
        ];
        let items = build_items(&history);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["kind"], "turn");
        assert_eq!(items[0]["turnId"], "t0");
        assert_eq!(items[0]["prompt"], "hello");
        let steps = items[0]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        let frames = steps[0]["frames"].as_array().unwrap();
        assert_eq!(frames[0]["kind"], "text");
        assert_eq!(frames[0]["text"], "hi");
        assert_eq!(frames[1]["kind"], "tool");
        assert_eq!(frames[1]["state"], "done");
        assert_eq!(frames[1]["output"], "file contents");
    }

    #[test]
    fn multiple_turns_keep_ordinals_and_paginate() {
        let mut history = Vec::new();
        for index in 0..5 {
            history.push(user(&format!("q{index}")));
            history.push(assistant(&format!("a{index}"), Vec::new()));
        }
        let items = build_items(&history);
        assert_eq!(items.len(), 5);

        let (page, has_more) = paginate_turns(
            &items,
            &TurnPageQuery {
                before_turn: None,
                after_turn: None,
                page_size: 2,
            },
        );
        assert!(has_more);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0]["turnId"], "t3");
        assert_eq!(page[1]["turnId"], "t4");

        let (older, older_more) = paginate_turns(
            &items,
            &TurnPageQuery {
                before_turn: Some("t3".to_string()),
                after_turn: None,
                page_size: 10,
            },
        );
        assert!(!older_more);
        assert_eq!(older.len(), 3);
        assert_eq!(older[0]["turnId"], "t0");
    }

    #[test]
    fn projects_user_messages_and_plans() {
        let plan_output = "Done\nPlan saved to: /tmp/plan.md\n## Approved Plan:\ndo the thing";
        let history = vec![
            user("start"),
            assistant("", vec![call("c1", "ExitPlanMode")]),
            tool("c1", plan_output),
        ];
        let items = build_items(&history);
        let messages = project_user_messages(&items);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["prompt"], "start");

        let plans = project_plans(&items, Some("c1"));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0]["path"], "/tmp/plan.md");
        assert!(plans[0]["plan"].as_str().unwrap().contains("do the thing"));

        assert!(project_plans(&items, Some("missing")).is_empty());
    }

    fn dual_path_fixture(turns: &[(u32, bool)]) -> (Vec<Value>, Vec<Value>) {
        use crate::native::event_store::RawWireEvent;
        use crate::session::sqlite_store::SqliteSessionStore;

        let session_id = "sess-cold-live-wire-axis";
        let store = SqliteSessionStore::in_memory().unwrap();
        store.create_session(session_id, Some("Wire axis")).unwrap();
        for (wire_id, closed) in turns {
            let prompt = format!("prompt {wire_id}");
            let answer = format!("answer {wire_id}");
            store
                .save_turn(
                    session_id,
                    &format!("turn-{wire_id}"),
                    *wire_id,
                    &[user(&prompt), assistant(&answer, Vec::new())],
                    None,
                    Some(&json!({ "kind": "user" })),
                )
                .unwrap();
            let append = |event_type: &str, payload: Value, suffix: &str| {
                store
                    .append_wire_event(&RawWireEvent {
                        id: format!("evt-{wire_id}-{suffix}"),
                        session_id: session_id.into(),
                        event_type: event_type.into(),
                        payload,
                        is_checkpoint: false,
                        is_compaction: false,
                        created_at: i64::from(*wire_id) * 1_000,
                    })
                    .unwrap();
            };
            append(
                "turn.prompt",
                json!({
                    "type": "turn.prompt",
                    "turnId": wire_id,
                    "input": [{ "type": "text", "text": prompt }],
                    "origin": { "kind": "user" },
                }),
                "prompt",
            );
            append(
                "llm.step.begin",
                json!({
                    "type": "llm.step.begin",
                    "agent_id": "main",
                    "turn_id": format!("turn-{wire_id}"),
                    "step": 1,
                }),
                "step",
            );
            append(
                "assistant.delta",
                json!({
                    "type": "assistant.delta",
                    "agent_id": "main",
                    "turn_id": format!("turn-{wire_id}"),
                    "delta": answer,
                }),
                "delta",
            );
            if *closed {
                append(
                    "turn.ended",
                    json!({
                        "type": "turn.ended",
                        "agent_id": "main",
                        "turn_id": format!("turn-{wire_id}"),
                        "reason": "completed",
                    }),
                    "ended",
                );
            }
        }

        let history = store.load_session_history(session_id).unwrap();
        let cold = cold_items(&store, session_id, &history);
        let record_count = store.count_wire_events(session_id).unwrap();
        let records = store.get_wire_events(session_id, 0, record_count).unwrap();
        let mut projector = project::TranscriptProjector::new();
        for record in records {
            projector.apply_event_at(
                &EngineEvent::from_json(record.payload),
                Some(record.created_at),
            );
        }
        let live = serde_json::to_value(projector.snapshot().items)
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        (cold, live)
    }

    fn wire_axis(items: &[Value]) -> Vec<Value> {
        items
            .iter()
            .map(|turn| {
                let steps = turn["steps"].as_array().unwrap();
                json!({
                    "turnId": turn["turnId"],
                    "ordinal": turn["ordinal"],
                    "steps": steps.iter().map(|step| {
                        let frames = step["frames"].as_array().unwrap();
                        json!({
                            "stepId": step["stepId"],
                            "turnId": step["turnId"],
                            "frameIds": frames.iter().map(|frame| frame["frameId"].clone()).collect::<Vec<_>>(),
                        })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect()
    }

    #[test]
    fn cold_and_live_paths_share_the_first_turn_wire_axis() {
        let (cold, live) = dual_path_fixture(&[(1, true)]);
        assert_eq!(live[0]["turnId"], "t1", "the live fixture uses wire id 1");
        assert_eq!(wire_axis(&cold), wire_axis(&live));
    }

    #[test]
    fn cold_and_live_paths_share_wire_axes_across_holes() {
        let (cold, live) = dual_path_fixture(&[(2, true), (5, true), (9, true)]);
        let live_ids: Vec<&str> = live
            .iter()
            .map(|turn| turn["turnId"].as_str().unwrap())
            .collect();
        assert_eq!(live_ids, ["t2", "t5", "t9"]);
        assert_eq!(wire_axis(&cold), wire_axis(&live));
    }

    #[test]
    fn cold_and_live_paths_share_an_unclosed_tail_wire_axis() {
        let (cold, live) = dual_path_fixture(&[(4, true), (8, false)]);
        assert_eq!(live.last().unwrap()["state"], "running");
        assert_eq!(wire_axis(&cold), wire_axis(&live));
    }

    #[test]
    fn cold_prompts_reads_prompt_records_from_the_journal() {
        use crate::events::EngineEvent;
        use crate::session::sqlite_store::SqliteSessionStore;
        use crate::session::wire_journal::WireJournal;

        let dir = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap());
        let workspace = store
            .create_workspace("/work/prompt-journal", None)
            .unwrap();
        store
            .create_session_with_workspace("sess-prompt-journal", None, Some(&workspace.id))
            .unwrap();
        let journal = WireJournal::new(dir.path(), store.clone());
        journal.append(
            "sess-prompt-journal",
            &EngineEvent::Custom(json!({
                "type": "prompt.submitted",
                "promptId": "p-1",
                "userMessageId": "m-1",
                "status": "running",
                "content": [{ "type": "text", "text": "hi" }],
                "createdAt": "2026-01-01T00:00:00Z",
            })),
            1000,
        );

        let prompts = cold_prompts(&store, "sess-prompt-journal", None);
        let prompts = prompts.as_array().unwrap();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(prompts[0]["promptId"], "p-1");
    }

    #[test]
    fn cold_tasks_reads_task_records_from_the_journal() {
        use crate::events::EngineEvent;
        use crate::session::sqlite_store::SqliteSessionStore;
        use crate::session::wire_journal::WireJournal;

        let dir = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap());
        let workspace = store.create_workspace("/work/task-journal", None).unwrap();
        store
            .create_session_with_workspace("sess-task-journal", None, Some(&workspace.id))
            .unwrap();
        let journal = WireJournal::new(dir.path(), store.clone());
        for (time, event) in [
            (
                1_000,
                json!({
                    "type": "task.started",
                    "info": {
                        "taskId": "task-1",
                        "kind": "process",
                        "status": "running",
                        "description": "job",
                        "startedAt": 1_000,
                        "endedAt": null,
                    },
                }),
            ),
            (
                1_001,
                json!({
                    "type": "task.output",
                    "taskId": "task-1",
                    "chunk": "hello\n",
                    "outputSizeBytes": 6,
                }),
            ),
            (
                1_002,
                json!({
                    "type": "task.terminated",
                    "info": {
                        "taskId": "task-1",
                        "kind": "process",
                        "status": "completed",
                        "description": "job",
                        "startedAt": 1_000,
                        "endedAt": 1_002,
                    },
                    "outputTail": "hello\n",
                }),
            ),
        ] {
            journal.append("sess-task-journal", &EngineEvent::Custom(event), time);
        }

        let tasks = cold_tasks(&store, "sess-task-journal", None);
        let tasks = tasks.as_array().unwrap();
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0]["taskId"], "task-1");
        assert_eq!(tasks[0]["kind"], "shell");
        assert_eq!(tasks[0]["state"], "completed");
        assert_eq!(tasks[0]["description"], "job");
        assert_eq!(tasks[0]["outputTail"], "hello\n");
    }

    #[test]
    fn cold_items_takes_the_turn_axis_from_the_journal_before_sqlite() {
        use crate::events::EngineEvent;
        use crate::session::sqlite_store::SqliteSessionStore;
        use crate::session::wire_journal::WireJournal;

        let dir = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap());
        let workspace = store.create_workspace("/work/journal-axis", None).unwrap();
        store
            .create_session_with_workspace("sess-journal-axis", None, Some(&workspace.id))
            .unwrap();
        let journal = WireJournal::new(dir.path(), store.clone());
        for (turn_id, prompt) in [(3i64, "first"), (7i64, "second")] {
            journal.append(
                "sess-journal-axis",
                &EngineEvent::Custom(json!({
                    "type": "turn.prompt",
                    "turnId": turn_id,
                    "input": [{ "type": "text", "text": prompt }],
                    "origin": { "kind": "user" },
                })),
                turn_id * 10,
            );
        }

        let history = store.load_session_history("sess-journal-axis").unwrap();
        assert_eq!(history.len(), 2, "{history:?}");
        assert_eq!(
            store.latest_wire_event_seq("sess-journal-axis").unwrap(),
            0,
            "the cold path must not need sqlite wire rows"
        );
        let items = cold_items(&store, "sess-journal-axis", &history);
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0]["turnId"], "t3");
        assert_eq!(items[1]["turnId"], "t7");
    }
}
