use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::compaction::{estimate_message_tokens, estimate_tokens};
use crate::native::event_store::migration::{MIGRATIONS, WireMigration, resolve_wire_migrations};
use crate::native::event_store::{WIRE_PROTOCOL_VERSION, compare_wire_versions};
use crate::turn_loop::types::LLMMessage;

pub const FOLD_REPLAY_TURN_ID: &str = "__replayTurnId";

pub const FOLD_RELEVANT_WIRE_TYPES: [&str; 26] = [
    "context.append_message",
    "context.append_loop_event",
    "context.undo",
    "context.clear",
    "context.apply_compaction",
    "full_compaction.begin",
    "full_compaction.cancel",
    "goal.create",
    "goal.update",
    "goal.clear",
    "goal.account_usage",
    "goal.continuation",
    "forked",
    "plan_mode.enter",
    "plan_mode.cancel",
    "plan_mode.exit",
    "spec_mode.enter",
    "spec_mode.cancel",
    "spec_mode.exit",
    "config.update",
    "permission.set_mode",
    "permission.record_approval_result",
    "tools.update_store",
    "turn.prompt",
    "turn.steer",
    "turn.cancel",
];

pub const FOLD_CARRYOVER_WIRE_TYPES: [&str; 13] = [
    "tools.update_store",
    "goal.create",
    "goal.update",
    "goal.clear",
    "goal.account_usage",
    "goal.continuation",
    "forked",
    "context.undo",
    "context.clear",
    "context.apply_compaction",
    "turn.prompt",
    "turn.steer",
    "turn.cancel",
];

pub fn is_fold_relevant_wire_type(event_type: &str) -> bool {
    FOLD_RELEVANT_WIRE_TYPES.contains(&event_type)
}

pub fn is_fold_carryover_wire_type(event_type: &str) -> bool {
    FOLD_CARRYOVER_WIRE_TYPES.contains(&event_type)
}

const TOOL_INTERRUPTED_ON_RESUME_OUTPUT: &str = "Tool execution was interrupted before its result was recorded. Do not assume the tool completed successfully.";

const COMPACT_USER_MESSAGE_MAX_TOKENS: i64 = 20_000;

const GOAL_FORK_CLEARED_REMINDER: &str = "This fork does not have a current goal. Ignore earlier active-goal reminders from the source session. Handle requests normally unless the user starts a new goal.";

#[derive(Debug, Clone)]
pub struct WireReplayFold {
    pub replay: Vec<Value>,
    pub tool_store: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireTurnInputMatch {
    pub append_index: usize,
    pub input_index: Option<usize>,
    pub visible_turn_index: usize,
    pub turn_id: Option<i64>,
}

pub fn fold_wire_records(
    records: &[Value],
    matching_records: Option<&[Value]>,
) -> Result<WireReplayFold, String> {
    let matching = matching_records.unwrap_or(records);
    let mut turns_by_append: HashMap<usize, i64> = HashMap::new();
    for matched in match_wire_turn_input_records(matching, true) {
        if let Some(turn_id) = matched.turn_id {
            let matched_record = &matching[matched.append_index];
            if let Some(index) = records
                .iter()
                .position(|record| std::ptr::eq(record, matched_record))
            {
                turns_by_append.insert(index, turn_id);
            }
        }
    }
    let annotated: Vec<Value> = records
        .iter()
        .enumerate()
        .map(|(index, record)| {
            if record.get("type").and_then(Value::as_str) != Some("context.append_message") {
                return record.clone();
            }
            let mut record = record.clone();
            if let Some(turn_id) = turns_by_append.get(&index)
                && let Some(object) = record.as_object_mut()
            {
                object.insert(FOLD_REPLAY_TURN_ID.to_string(), json!(turn_id));
            }
            record
        })
        .collect();
    let mut fold = FoldState::default();
    for record in migrate_journal_records(&annotated)? {
        fold.apply(&record)?;
    }
    fold.finish();
    Ok(WireReplayFold {
        replay: fold.replay,
        tool_store: fold.tool_store,
    })
}

fn migrate_journal_records(records: &[Value]) -> Result<Vec<Value>, String> {
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let migrations: Vec<&WireMigration> =
        if records[0].get("type").and_then(Value::as_str) == Some("metadata") {
            let Some(version) = records[0].get("protocol_version").and_then(Value::as_str) else {
                return Err("Agent wire metadata is malformed".to_string());
            };
            if compare_wire_versions(version, WIRE_PROTOCOL_VERSION) >= 0 {
                Vec::new()
            } else {
                resolve_wire_migrations(version).map_err(|error| error.to_string())?
            }
        } else {
            MIGRATIONS
                .iter()
                .filter(|migration| migration.source_version == "1.4")
                .collect()
        };
    let mut migrated = Vec::with_capacity(records.len());
    for record in records {
        let Some(object) = record.as_object() else {
            migrated.push(record.clone());
            continue;
        };
        let Some(mut event_type) = object
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            migrated.push(record.clone());
            continue;
        };
        let mut payload = Value::Object(object.clone());
        if let Some(payload) = payload.as_object_mut() {
            payload.remove("type");
        }
        for migration in &migrations {
            migration.migrate_record(&mut event_type, &mut payload);
        }
        let mut merged = payload.as_object().cloned().unwrap_or_default();
        merged.insert("type".to_string(), json!(event_type));
        migrated.push(Value::Object(merged));
    }
    Ok(migrated)
}

pub fn match_wire_turn_input_records(
    records: &[Value],
    all_user_messages: bool,
) -> Vec<WireTurnInputMatch> {
    let mut input_indices: Vec<usize> = Vec::new();
    let mut appends: Vec<(usize, usize)> = Vec::new();
    let mut turn_ids: HashMap<usize, i64> = HashMap::new();
    let mut cancelled_turn_ids: HashSet<i64> = HashSet::new();
    let mut current_turn_id: Option<i64> = None;
    let mut next_turn_id: i64 = 0;
    let mut visible_turn_index: usize = 0;
    for (index, record) in records.iter().enumerate() {
        match record.get("type").and_then(Value::as_str) {
            Some("context.undo") | Some("context.clear") | Some("context.apply_compaction") => {
                current_turn_id = None;
            }
            Some("turn.cancel") => {
                let turn_id = record.get("turnId").and_then(Value::as_i64);
                if record.get("target").is_some()
                    && let Some(turn_id) = turn_id
                    && turn_id >= next_turn_id
                {
                    cancelled_turn_ids.insert(turn_id);
                    while cancelled_turn_ids.remove(&next_turn_id) {
                        next_turn_id += 1;
                    }
                }
                continue;
            }
            Some("turn.prompt") => {
                let turn_id = record
                    .get("turnId")
                    .and_then(Value::as_i64)
                    .unwrap_or(next_turn_id);
                current_turn_id = Some(turn_id);
                turn_ids.insert(index, turn_id);
                next_turn_id = next_turn_id.max(turn_id + 1);
                while cancelled_turn_ids.remove(&next_turn_id) {
                    next_turn_id += 1;
                }
            }
            Some("turn.steer") => {
                let turn_id = record
                    .get("turnId")
                    .and_then(Value::as_i64)
                    .or(current_turn_id);
                if let Some(turn_id) = turn_id {
                    turn_ids.insert(index, turn_id);
                }
            }
            _ => {}
        }
        let is_input = if all_user_messages {
            matches!(
                record.get("type").and_then(Value::as_str),
                Some("turn.prompt") | Some("turn.steer")
            )
        } else {
            is_visible_wire_turn_input(record)
        };
        if is_input {
            input_indices.push(index);
        }
        let is_append = if all_user_messages {
            record.get("type").and_then(Value::as_str) == Some("context.append_message")
                && wire_record_object(record.get("message"))
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str)
                    == Some("user")
        } else {
            is_visible_wire_turn_append(record)
        };
        if is_append {
            appends.push((index, visible_turn_index));
            visible_turn_index += 1;
        }
    }
    let mut unused = input_indices;
    let mut matches = Vec::with_capacity(appends.len());
    for (append_index, visible_index) in appends {
        let exact = unused.iter().position(|candidate| {
            (records[*candidate].get("type").and_then(Value::as_str) != Some("turn.prompt")
                || *candidate < append_index)
                && wire_turn_input_matches(
                    &records[*candidate],
                    &records[append_index],
                    true,
                    all_user_messages,
                )
        });
        let match_at = exact.or_else(|| {
            unused.iter().position(|candidate| {
                (records[*candidate].get("type").and_then(Value::as_str) != Some("turn.prompt")
                    || *candidate < append_index)
                    && wire_turn_input_matches(
                        &records[*candidate],
                        &records[append_index],
                        false,
                        all_user_messages,
                    )
            })
        });
        let input_index = match_at.map(|position| unused.remove(position));
        matches.push(WireTurnInputMatch {
            append_index,
            input_index,
            visible_turn_index: visible_index,
            turn_id: input_index.and_then(|index| turn_ids.get(&index).copied()),
        });
    }
    matches
}

fn is_visible_wire_turn_input(record: &Value) -> bool {
    if !matches!(
        record.get("type").and_then(Value::as_str),
        Some("turn.prompt") | Some("turn.steer")
    ) {
        return false;
    }
    let origin = wire_record_object(record.get("origin"));
    match origin
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
    {
        Some("user") => true,
        Some("skill_activation") | Some("plugin_command") => {
            origin
                .and_then(|origin| origin.get("trigger"))
                .and_then(Value::as_str)
                == Some("user-slash")
        }
        Some("shell_command") => {
            origin
                .and_then(|origin| origin.get("phase"))
                .and_then(Value::as_str)
                == Some("input")
        }
        _ => false,
    }
}

fn is_visible_wire_turn_append(record: &Value) -> bool {
    if record.get("type").and_then(Value::as_str) != Some("context.append_message") {
        return false;
    }
    let Some(message) = wire_record_object(record.get("message")) else {
        return false;
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let origin = wire_record_object(message.get("origin"));
    match origin
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
    {
        None | Some("user") => true,
        Some("skill_activation") | Some("plugin_command") => {
            origin
                .and_then(|origin| origin.get("trigger"))
                .and_then(Value::as_str)
                == Some("user-slash")
        }
        Some("shell_command") => {
            origin
                .and_then(|origin| origin.get("phase"))
                .and_then(Value::as_str)
                == Some("input")
        }
        _ => false,
    }
}

fn wire_turn_input_matches(
    input_record: &Value,
    turn_record: &Value,
    compare_content: bool,
    any_origin: bool,
) -> bool {
    if !matches!(
        input_record.get("type").and_then(Value::as_str),
        Some("turn.prompt") | Some("turn.steer")
    ) {
        return false;
    }
    if turn_record.get("type").and_then(Value::as_str) != Some("context.append_message") {
        return false;
    }
    let Some(message) = wire_record_object(turn_record.get("message")) else {
        return false;
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if !any_origin {
        let input_kind = wire_record_object(input_record.get("origin"))
            .and_then(|origin| origin.get("kind"))
            .and_then(Value::as_str);
        let Some(input_kind) = input_kind else {
            return false;
        };
        let message_origin = wire_record_object(message.get("origin"));
        let message_kind_value = message_origin.and_then(|origin| origin.get("kind"));
        if message_kind_value.is_some_and(|kind| !kind.is_string()) {
            return false;
        }
        let message_kind = message_kind_value.and_then(Value::as_str);
        if input_kind == "user" {
            if message_kind.is_some_and(|kind| kind != "user") {
                return false;
            }
        } else if message_kind != Some(input_kind) {
            return false;
        }
    }
    let message_id = message.get("id").and_then(Value::as_str);
    let steer_message_id = if input_record.get("type").and_then(Value::as_str) == Some("turn.steer")
    {
        input_record.get("messageId").and_then(Value::as_str)
    } else {
        None
    };
    if let (Some(steer_message_id), Some(message_id)) = (steer_message_id, message_id) {
        return steer_message_id == message_id;
    }
    let prompt_id = if input_record.get("type").and_then(Value::as_str) == Some("turn.prompt") {
        input_record.get("promptId").and_then(Value::as_str)
    } else {
        None
    };
    if let (Some(prompt_id), Some(message_id)) = (prompt_id, message_id) {
        return prompt_id == message_id;
    }
    !compare_content
        || match (input_record.get("input"), message.get("content")) {
            (None, None) => true,
            (Some(input), Some(content)) => {
                serde_json::to_string(input).ok() == serde_json::to_string(content).ok()
            }
            _ => false,
        }
}

fn wire_record_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

#[derive(Debug, Clone, Copy)]
struct OpenStep {
    history_index: usize,
    replay_index: usize,
}

#[derive(Debug, Clone)]
struct FoldGoalState {
    goal_id: String,
    objective: String,
    completion_criterion: Option<String>,
    status: String,
    turns_used: i64,
    tokens_used: i64,
    input_tokens_used: i64,
    output_tokens_used: i64,
    wall_clock_ms: i64,
    token_budget: Option<i64>,
    turn_budget: Option<i64>,
    wall_clock_budget_ms: Option<i64>,
    terminal_reason: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Default)]
struct FoldState {
    replay: Vec<Value>,
    tool_store: Map<String, Value>,
    history: Vec<Value>,
    open_steps: HashMap<String, OpenStep>,
    pending_tool_result_ids: Vec<String>,
    deferred: Vec<(Value, Option<i64>)>,
    goal: Option<FoldGoalState>,
    next_turn_id: i64,
    cancelled_turn_ids: HashSet<i64>,
}

impl FoldState {
    fn apply(&mut self, record: &Value) -> Result<(), String> {
        let time = record
            .get("time")
            .filter(|time| !time.is_null())
            .cloned()
            .unwrap_or_else(|| json!(chrono::Utc::now().timestamp_millis()));
        match record.get("type").and_then(Value::as_str) {
            Some("context.append_message") => {
                let message = record.get("message").cloned().unwrap_or(Value::Null);
                let turn_id = record.get(FOLD_REPLAY_TURN_ID).and_then(Value::as_i64);
                self.append_message(message, &time, turn_id);
            }
            Some("context.append_loop_event") => {
                let event = record.get("event").cloned().unwrap_or(Value::Null);
                self.append_loop_event(&event, &time)?;
            }
            Some("context.undo") => {
                self.undo(record.get("count").and_then(Value::as_i64).unwrap_or(0));
            }
            Some("context.clear") => {
                self.clear_context();
            }
            Some("context.apply_compaction") => {
                self.apply_compaction(record);
            }
            Some("turn.prompt") => {
                self.update_turn_id(record);
            }
            Some("turn.steer") => {}
            Some("turn.cancel") => {
                let turn_id = record.get("turnId").and_then(Value::as_i64);
                if record.get("target").is_some()
                    && let Some(turn_id) = turn_id
                    && turn_id >= self.next_turn_id
                {
                    self.cancelled_turn_ids.insert(turn_id);
                    while self.cancelled_turn_ids.remove(&self.next_turn_id) {
                        self.next_turn_id += 1;
                    }
                }
            }
            Some("full_compaction.begin") => {
                let mut payload = json!({"type": "compaction"});
                if let Some(instruction) = read_string(record, "instruction") {
                    payload["instruction"] = json!(instruction);
                }
                self.push(payload, &time);
            }
            Some("full_compaction.cancel") => {
                self.patch_last_compaction(json!("cancelled"));
            }
            Some("goal.create") => {
                self.create_goal(record, &time);
            }
            Some("goal.update") => {
                self.update_goal(record, &time);
            }
            Some("goal.clear") => {
                self.goal = None;
            }
            Some("forked") => {
                self.apply_forked(&time);
            }
            Some("plan_mode.enter") => {
                self.push(json!({"type": "plan_updated", "enabled": true}), &time);
            }
            Some("plan_mode.cancel") | Some("plan_mode.exit") => {
                self.push(json!({"type": "plan_updated", "enabled": false}), &time);
            }
            Some("spec_mode.enter") => {
                self.push(json!({"type": "spec_updated", "enabled": true}), &time);
            }
            Some("spec_mode.cancel") | Some("spec_mode.exit") => {
                self.push(json!({"type": "spec_updated", "enabled": false}), &time);
            }
            Some("config.update") => {
                self.update_config(record, &time);
            }
            Some("permission.set_mode") => {
                let mut payload = json!({"type": "permission_updated"});
                if let Some(mode) = record.get("mode") {
                    payload["mode"] = mode.clone();
                }
                self.push(payload, &time);
            }
            Some("permission.record_approval_result") => {
                self.record_approval_result(record, &time);
            }
            Some("tools.update_store") => {
                let key = record
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let value = record.get("value").cloned().unwrap_or(Value::Null);
                self.tool_store.insert(key, value);
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(&mut self) {
        let now = json!(chrono::Utc::now().timestamp_millis());
        self.close_pending_tool_results(&now);
    }

    fn push(&mut self, mut payload: Value, time: &Value) {
        if let Some(object) = payload.as_object_mut() {
            object.insert("time".to_string(), time.clone());
        }
        self.replay.push(payload);
    }

    fn update_turn_id(&mut self, record: &Value) {
        let turn_id = record
            .get("turnId")
            .and_then(Value::as_i64)
            .unwrap_or(self.next_turn_id);
        self.next_turn_id = self.next_turn_id.max(turn_id + 1);
        while self.cancelled_turn_ids.remove(&self.next_turn_id) {
            self.next_turn_id += 1;
        }
    }

    fn push_history_entry(&mut self, message: Value, time: &Value, turn_id: Option<i64>) {
        self.push_history_entry_indexed(message, time, turn_id);
    }

    fn push_history_entry_indexed(
        &mut self,
        message: Value,
        time: &Value,
        turn_id: Option<i64>,
    ) -> (usize, usize) {
        self.history.push(message.clone());
        let mut payload = json!({"type": "message", "message": message.clone()});
        if message.get("role").and_then(Value::as_str) == Some("user")
            && let Some(turn_id) = turn_id
            && let Some(object) = payload.as_object_mut()
        {
            object.insert("turnId".to_string(), json!(turn_id));
        }
        self.push(payload, time);
        (self.history.len() - 1, self.replay.len() - 1)
    }

    fn append_message(&mut self, message: Value, time: &Value, turn_id: Option<i64>) {
        if !self.pending_tool_result_ids.is_empty() {
            self.deferred.push((message, turn_id));
            return;
        }
        self.push_history_entry(message, time, turn_id);
    }

    fn flush_deferred_messages(&mut self, time: &Value) {
        if !self.pending_tool_result_ids.is_empty() || self.deferred.is_empty() {
            return;
        }
        let deferred = std::mem::take(&mut self.deferred);
        for (message, turn_id) in deferred {
            self.push_history_entry(message, time, turn_id);
        }
    }

    fn close_pending_tool_results(&mut self, time: &Value) {
        if self.pending_tool_result_ids.is_empty() {
            return;
        }
        let ids = std::mem::take(&mut self.pending_tool_result_ids);
        for tool_call_id in ids {
            self.push_history_entry(
                json!({
                    "role": "tool",
                    "toolCallId": tool_call_id,
                    "content": TOOL_INTERRUPTED_ON_RESUME_OUTPUT,
                    "isError": true,
                }),
                time,
                None,
            );
        }
        self.flush_deferred_messages(time);
    }

    fn append_loop_event(&mut self, event: &Value, time: &Value) -> Result<(), String> {
        match event.get("type").and_then(Value::as_str) {
            Some("step.begin") => {
                self.close_pending_tool_results(time);
                let uuid = read_string(event, "uuid").unwrap_or_default();
                let (history_index, replay_index) = self.push_history_entry_indexed(
                    json!({"role": "assistant", "content": [], "toolCalls": []}),
                    time,
                    None,
                );
                self.open_steps.insert(
                    uuid,
                    OpenStep {
                        history_index,
                        replay_index,
                    },
                );
            }
            Some("step.end") => {
                let uuid = read_string(event, "uuid").unwrap_or_default();
                self.open_steps.remove(&uuid);
                self.flush_deferred_messages(time);
            }
            Some("content.part") => {
                let step_uuid = read_string(event, "stepUuid").unwrap_or_default();
                let Some(step) = self.open_steps.get(&step_uuid).copied() else {
                    return Err(format!(
                        "Received content_part for unknown step_uuid '{step_uuid}' (no open step_begin)"
                    ));
                };
                let part = event.get("part").cloned().unwrap_or(Value::Null);
                push_message_array(
                    &mut self.history[step.history_index],
                    false,
                    "content",
                    part.clone(),
                );
                push_message_array(&mut self.replay[step.replay_index], true, "content", part);
            }
            Some("tool.call") => {
                let step_uuid = read_string(event, "stepUuid").unwrap_or_default();
                let Some(step) = self.open_steps.get(&step_uuid).copied() else {
                    return Err(format!(
                        "Received tool_call for unknown step_uuid '{step_uuid}' (no open step_begin)"
                    ));
                };
                let tool_call_id = read_string(event, "toolCallId").unwrap_or_default();
                let arguments = match event.get("args") {
                    None => Value::Null,
                    Some(args) => Value::String(serde_json::to_string(args).unwrap_or_default()),
                };
                let mut call = json!({
                    "type": "function",
                    "id": tool_call_id,
                    "name": read_string(event, "name").unwrap_or_default(),
                    "arguments": arguments,
                });
                if let Some(extras) = event.get("extras") {
                    call["extras"] = extras.clone();
                }
                push_message_array(
                    &mut self.history[step.history_index],
                    false,
                    "toolCalls",
                    call.clone(),
                );
                push_message_array(&mut self.replay[step.replay_index], true, "toolCalls", call);
                if let Some(display) = event.get("display") {
                    set_tool_call_display(
                        &mut self.history[step.history_index],
                        false,
                        &tool_call_id,
                        display.clone(),
                    );
                    set_tool_call_display(
                        &mut self.replay[step.replay_index],
                        true,
                        &tool_call_id,
                        display.clone(),
                    );
                }
                if !self.pending_tool_result_ids.contains(&tool_call_id) {
                    self.pending_tool_result_ids.push(tool_call_id);
                }
            }
            Some("tool.result") => {
                let tool_call_id = read_string(event, "toolCallId").unwrap_or_default();
                if !self.pending_tool_result_ids.contains(&tool_call_id) {
                    return Ok(());
                }
                let result = event.get("result").cloned().unwrap_or(Value::Null);
                let output = result.get("output").cloned().unwrap_or(Value::Null);
                let content = match output {
                    Value::String(text) => Value::String(text),
                    other => other,
                };
                let mut message = json!({
                    "role": "tool",
                    "toolCallId": tool_call_id,
                    "content": content,
                });
                if let Some(is_error) = result.get("isError") {
                    message["isError"] = is_error.clone();
                }
                if let Some(note) = result.get("note") {
                    message["note"] = note.clone();
                }
                self.push_history_entry(message, time, None);
                self.pending_tool_result_ids
                    .retain(|id| id != &tool_call_id);
                self.flush_deferred_messages(time);
            }
            _ => {}
        }
        Ok(())
    }

    fn undo(&mut self, count: i64) {
        if count <= 0 || self.history.is_empty() {
            return;
        }
        let mut removed: Vec<Value> = Vec::new();
        let mut removed_user_count = 0;
        let mut index = self.history.len();
        while index > 0 {
            index -= 1;
            let message = &self.history[index];
            if message
                .get("origin")
                .and_then(|origin| origin.get("kind"))
                .and_then(Value::as_str)
                == Some("injection")
            {
                continue;
            }
            if message
                .get("origin")
                .and_then(|origin| origin.get("kind"))
                .and_then(Value::as_str)
                == Some("compaction_summary")
            {
                break;
            }
            let message = self.history.remove(index);
            if is_real_user_input(&message) {
                removed_user_count += 1;
            }
            removed.push(message);
            if removed_user_count >= count {
                break;
            }
        }
        for removed in removed {
            let index = self.replay.iter().rposition(|record| {
                record.get("type").and_then(Value::as_str) == Some("message")
                    && record.get("message") == Some(&removed)
            });
            if let Some(index) = index {
                self.replay.remove(index);
            }
        }
        self.open_steps.clear();
        self.pending_tool_result_ids.clear();
        self.deferred.clear();
    }

    fn clear_context(&mut self) {
        self.history.clear();
        self.open_steps.clear();
        self.pending_tool_result_ids.clear();
        self.deferred.clear();
    }

    fn apply_compaction(&mut self, record: &Value) {
        let summary = read_compaction_summary(record);
        let context_summary =
            read_string(record, "contextSummary").unwrap_or_else(|| summary.clone());
        let compacted_count_value = read_number_json(record, "compactedCount")
            .or_else(|| read_number_json(record, "count"))
            .unwrap_or_else(|| json!(0));
        let compacted_count_number = compacted_count_value.as_f64().unwrap_or(0.0);
        let compacted_count = compacted_count_value
            .as_i64()
            .or_else(|| {
                compacted_count_value
                    .as_f64()
                    .map(|number| number.trunc() as i64)
            })
            .unwrap_or(0);
        let kept_tail = self.select_legacy_kept_tail(record);
        let tokens_after = read_number_json(record, "tokensAfter").unwrap_or_else(|| {
            json!(
                estimate_tokens(&context_summary) as i64
                    + kept_tail.iter().map(estimate_message).sum::<i64>()
            )
        });
        let mut result = json!({
            "summary": summary,
            "contextSummary": context_summary,
            "compactedCount": compacted_count_value,
            "tokensBefore": read_number_json(record, "tokensBefore").unwrap_or_else(|| json!(0)),
            "tokensAfter": tokens_after,
            "keptUserMessageCount": read_number_json(record, "keptUserMessageCount").unwrap_or_else(|| json!(kept_tail.len() as i64)),
        });
        if let Some(value) = read_number_json(record, "keptHeadUserMessageCount") {
            result["keptHeadUserMessageCount"] = value;
        }
        if let Some(value) = read_number_json(record, "droppedCount") {
            result["droppedCount"] = value;
        }
        self.patch_last_compaction(result.clone());
        let summary_message = json!({
            "role": "user",
            "content": [{"type": "text", "text": result["contextSummary"].clone()}],
            "toolCalls": [],
            "origin": {"kind": "compaction_summary"},
        });
        self.history = if read_number(record, "keptUserMessageCount").is_none()
            && compacted_count_number < self.history.len() as f64
        {
            let start = if compacted_count < 0 {
                self.history.len().saturating_sub(
                    usize::try_from(compacted_count.unsigned_abs()).unwrap_or(usize::MAX),
                )
            } else {
                (compacted_count as usize).min(self.history.len())
            };
            let mut history = vec![summary_message];
            history.extend(self.history[start..].iter().cloned());
            history
        } else {
            vec![summary_message]
        };
        self.open_steps.clear();
        self.pending_tool_result_ids.clear();
        self.deferred.clear();
    }

    fn patch_last_compaction(&mut self, result: Value) {
        if let Some(last) = self.replay.last_mut()
            && last.get("type").and_then(Value::as_str) == Some("compaction")
        {
            last["result"] = result;
        }
    }

    fn select_legacy_kept_tail(&self, record: &Value) -> Vec<Value> {
        if read_number(record, "tokensAfter").is_some()
            && read_number(record, "keptUserMessageCount").is_some()
        {
            return Vec::new();
        }
        let compactable: Vec<&Value> = self
            .history
            .iter()
            .filter(|m| is_real_user_input(m))
            .collect();
        let mut selected: Vec<Value> = Vec::new();
        let mut remaining = COMPACT_USER_MESSAGE_MAX_TOKENS;
        for message in compactable.into_iter().rev() {
            if remaining <= 0 {
                break;
            }
            let tokens = estimate_message(message);
            selected.insert(0, message.clone());
            if tokens > remaining {
                break;
            }
            remaining -= tokens;
        }
        selected
    }

    fn create_goal(&mut self, record: &Value, time: &Value) {
        let now = time
            .as_i64()
            .or_else(|| time.as_f64().map(|value| value.trunc() as i64))
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let state = FoldGoalState {
            goal_id: read_string(record, "goalId").unwrap_or_default(),
            objective: read_string(record, "objective").unwrap_or_default(),
            completion_criterion: read_string(record, "completionCriterion"),
            status: "active".to_string(),
            turns_used: 0,
            tokens_used: 0,
            input_tokens_used: read_number(record, "inputTokensUsed").unwrap_or(0),
            output_tokens_used: read_number(record, "outputTokensUsed").unwrap_or(0),
            wall_clock_ms: 0,
            token_budget: None,
            turn_budget: None,
            wall_clock_budget_ms: None,
            terminal_reason: None,
            created_at: read_number(record, "createdAt").unwrap_or(now),
            updated_at: read_number(record, "updatedAt").unwrap_or(now),
        };
        let snapshot = goal_snapshot(&state);
        self.goal = Some(state);
        self.push(
            json!({
                "type": "goal_updated",
                "snapshot": snapshot,
                "change": {"kind": "created"},
            }),
            time,
        );
    }

    fn update_goal(&mut self, record: &Value, time: &Value) {
        let now = time
            .as_i64()
            .or_else(|| time.as_f64().map(|value| value.trunc() as i64))
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let Some(state) = self.goal.as_mut() else {
            return;
        };
        let status = read_string(record, "status");
        let reason = read_string(record, "reason");
        if let Some(objective) = read_string(record, "objective") {
            state.objective = objective;
        }
        if let Some(status) = status.as_deref() {
            state.status = status.to_string();
            state.terminal_reason = if status == "active" {
                None
            } else {
                reason.clone()
            };
        }
        if let Some(value) = read_number(record, "turnsUsed") {
            state.turns_used = value;
        }
        if let Some(value) = read_number(record, "tokensUsed") {
            state.tokens_used = value;
        }
        if let Some(value) = read_number(record, "wallClockMs") {
            state.wall_clock_ms = value;
        }
        if let Some(value) = read_number(record, "inputTokensUsed") {
            state.input_tokens_used = value;
        }
        if let Some(value) = read_number(record, "outputTokensUsed") {
            state.output_tokens_used = value;
        }
        state.updated_at = read_number(record, "updatedAt").unwrap_or(now);
        if let Some(limits) = wire_record_object(record.get("budgetLimits")) {
            state.token_budget = limits.get("tokenBudget").and_then(Value::as_i64);
            state.turn_budget = limits.get("turnBudget").and_then(Value::as_i64);
            state.wall_clock_budget_ms = limits.get("wallClockBudgetMs").and_then(Value::as_i64);
        }
        let Some(status) = status else {
            return;
        };
        let snapshot = goal_snapshot(state);
        let actor = record.get("actor").cloned();
        let mut change = if status == "complete" {
            json!({
                "kind": "completion",
                "status": status,
                "stats": {
                    "turnsUsed": state.turns_used,
                    "tokensUsed": state.tokens_used,
                    "wallClockMs": state.wall_clock_ms,
                },
            })
        } else {
            json!({
                "kind": "lifecycle",
                "status": status,
            })
        };
        if let Some(reason) = reason {
            change["reason"] = json!(reason);
        }
        if let Some(actor) = actor {
            change["actor"] = actor;
        }
        self.push(
            json!({
                "type": "goal_updated",
                "snapshot": snapshot,
                "change": change,
            }),
            time,
        );
    }

    fn apply_forked(&mut self, time: &Value) {
        if self.goal.is_none() {
            return;
        }
        self.goal = None;
        self.append_message(
            json!({
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": format!("<system-reminder>\n{GOAL_FORK_CLEARED_REMINDER}\n</system-reminder>"),
                }],
                "toolCalls": [],
                "origin": {"kind": "system_trigger", "name": "goal_fork_cleared"},
            }),
            time,
            None,
        );
    }

    fn update_config(&mut self, record: &Value, time: &Value) {
        let mut config = Map::new();
        if let Some(value) = read_string(record, "modelAlias") {
            config.insert("modelAlias".to_string(), json!(value));
        }
        if let Some(value) = read_string(record, "profileName") {
            config.insert("profileName".to_string(), json!(value));
        }
        if let Some(value) =
            read_string(record, "thinkingEffort").or_else(|| read_string(record, "thinkingLevel"))
        {
            config.insert("thinkingLevel".to_string(), json!(value));
        }
        if let Some(value) = read_string(record, "systemPrompt") {
            config.insert("systemPrompt".to_string(), json!(value));
        }
        self.push(
            json!({"type": "config_updated", "config": Value::Object(config)}),
            time,
        );
    }

    fn record_approval_result(&mut self, record: &Value, time: &Value) {
        let mut approval = Map::new();
        for key in ["turnId", "toolCallId", "toolName", "action"] {
            if let Some(value) = record.get(key) {
                approval.insert(key.to_string(), value.clone());
            }
        }
        if let Some(value) = read_string(record, "sessionApprovalRule") {
            approval.insert("sessionApprovalRule".to_string(), json!(value));
        }
        if let Some(result) = record.get("result") {
            approval.insert("result".to_string(), result.clone());
        }
        self.push(
            json!({"type": "approval_result", "record": Value::Object(approval)}),
            time,
        );
    }
}

fn goal_snapshot(state: &FoldGoalState) -> Value {
    let mut snapshot = Map::new();
    snapshot.insert("goalId".to_string(), json!(state.goal_id.clone()));
    snapshot.insert("objective".to_string(), json!(state.objective.clone()));
    if let Some(criterion) = &state.completion_criterion {
        snapshot.insert("completionCriterion".to_string(), json!(criterion));
    }
    snapshot.insert("status".to_string(), json!(state.status.clone()));
    snapshot.insert("turnsUsed".to_string(), json!(state.turns_used));
    snapshot.insert("tokensUsed".to_string(), json!(state.tokens_used));
    snapshot.insert(
        "inputTokensUsed".to_string(),
        json!(state.input_tokens_used),
    );
    snapshot.insert(
        "outputTokensUsed".to_string(),
        json!(state.output_tokens_used),
    );
    snapshot.insert("wallClockMs".to_string(), json!(state.wall_clock_ms));
    snapshot.insert("budget".to_string(), goal_budget_report(state));
    if let Some(reason) = &state.terminal_reason {
        snapshot.insert("terminalReason".to_string(), json!(reason));
    }
    snapshot.insert("createdAt".to_string(), json!(state.created_at));
    snapshot.insert("updatedAt".to_string(), json!(state.updated_at));
    Value::Object(snapshot)
}

fn goal_budget_report(state: &FoldGoalState) -> Value {
    let token_reached = state
        .token_budget
        .is_some_and(|budget| state.tokens_used >= budget);
    let turn_reached = state
        .turn_budget
        .is_some_and(|budget| state.turns_used >= budget);
    let wall_reached = state
        .wall_clock_budget_ms
        .is_some_and(|budget| state.wall_clock_ms >= budget);
    json!({
        "inputTokensUsed": state.input_tokens_used,
        "outputTokensUsed": state.output_tokens_used,
        "tokenBudget": state.token_budget,
        "turnBudget": state.turn_budget,
        "wallClockBudgetMs": state.wall_clock_budget_ms,
        "remainingTokens": state.token_budget.map(|budget| (budget - state.tokens_used).max(0)),
        "remainingTurns": state.turn_budget.map(|budget| (budget - state.turns_used).max(0)),
        "remainingWallClockMs": state.wall_clock_budget_ms.map(|budget| (budget - state.wall_clock_ms).max(0)),
        "tokenBudgetReached": token_reached,
        "turnBudgetReached": turn_reached,
        "wallClockBudgetReached": wall_reached,
        "overBudget": token_reached || turn_reached || wall_reached,
    })
}

fn is_real_user_input(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(origin) = wire_record_object(message.get("origin")) else {
        return true;
    };
    match origin.get("kind").and_then(Value::as_str) {
        Some("user") => true,
        Some("skill_activation") | Some("plugin_command") => {
            origin.get("trigger").and_then(Value::as_str) == Some("user-slash")
        }
        _ => false,
    }
}

fn read_string(record: &Value, key: &str) -> Option<String> {
    record.get(key).and_then(Value::as_str).map(str::to_string)
}

fn read_number(record: &Value, key: &str) -> Option<i64> {
    let value = record.get(key)?;
    if let Some(integer) = value.as_i64() {
        return Some(integer);
    }
    value.as_f64().map(|number| number.trunc() as i64)
}

fn read_number_json(record: &Value, key: &str) -> Option<Value> {
    record.get(key).filter(|value| value.is_number()).cloned()
}

fn read_compaction_summary(record: &Value) -> String {
    if let Some(summary) = record.get("summary").and_then(Value::as_str) {
        return summary.to_string();
    }
    if let Some(summary) = record.get("contextSummary").and_then(Value::as_str) {
        return summary.to_string();
    }
    if let Some(content) = record
        .get("summary")
        .and_then(Value::as_object)
        .and_then(|summary| summary.get("content"))
        .and_then(Value::as_array)
    {
        let mut text = String::new();
        for part in content {
            if let Some(part) = part.as_object()
                && part.get("type").and_then(Value::as_str) == Some("text")
                && let Some(fragment) = part.get("text").and_then(Value::as_str)
            {
                text.push_str(fragment);
            }
        }
        return text;
    }
    String::new()
}

fn estimate_message(message: &Value) -> i64 {
    serde_json::from_value::<LLMMessage>(message.clone())
        .map(|message| estimate_message_tokens(&message) as i64)
        .unwrap_or(0)
}

fn message_object(entry: &mut Value, in_replay: bool) -> Option<&mut Map<String, Value>> {
    let target = if in_replay {
        entry.get_mut("message")?
    } else {
        entry
    };
    target.as_object_mut()
}

fn push_message_array(entry: &mut Value, in_replay: bool, key: &str, item: Value) {
    if let Some(message) = message_object(entry, in_replay)
        && let Some(array) = message.get_mut(key).and_then(Value::as_array_mut)
    {
        array.push(item);
    }
}

fn set_tool_call_display(entry: &mut Value, in_replay: bool, tool_call_id: &str, display: Value) {
    let Some(message) = message_object(entry, in_replay) else {
        return;
    };
    let displays = message
        .entry("toolCallDisplays".to_string())
        .or_insert_with(|| json!({}));
    if let Some(displays) = displays.as_object_mut() {
        displays.insert(tool_call_id.to_string(), display);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(records: Vec<Value>) -> WireReplayFold {
        fold_wire_records(&records, None).expect("fold must succeed")
    }

    fn types(fold: &WireReplayFold) -> Vec<String> {
        fold.replay
            .iter()
            .map(|record| {
                record
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn messages_and_steps_replay_in_order_with_turn_ids() {
        let fold = fold(vec![
            json!({"type":"turn.prompt","turnId":0,"input":[{"type":"text","text":"hi"}],"origin":{"kind":"user"},"time":1}),
            json!({"type":"context.append_message","message":{"id":"m1","role":"user","content":[{"type":"text","text":"hi"}],"origin":{"kind":"user"}},"time":2}),
            json!({"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"s1"},"time":3}),
            json!({"type":"context.append_loop_event","event":{"type":"content.part","stepUuid":"s1","part":{"type":"text","text":"ok"}},"time":4}),
            json!({"type":"context.append_loop_event","event":{"type":"tool.call","stepUuid":"s1","toolCallId":"c1","name":"Read","args":{"path":"a"}},"time":5}),
            json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s1"},"time":6}),
        ]);
        assert_eq!(types(&fold), vec!["message", "message", "message"]);
        assert_eq!(fold.replay[0]["turnId"], json!(0));
        assert_eq!(fold.replay[1]["message"]["content"][0]["text"], json!("ok"));
        assert_eq!(fold.replay[1]["message"]["toolCalls"][0]["id"], json!("c1"));
        assert_eq!(
            fold.replay[1]["message"]["toolCalls"][0]["arguments"],
            json!("{\"path\":\"a\"}")
        );
        assert_eq!(fold.replay[2]["message"]["role"], json!("tool"));
        assert_eq!(fold.replay[2]["message"]["toolCallId"], json!("c1"));
        assert_eq!(fold.replay[2]["message"]["isError"], json!(true));
        assert_eq!(
            fold.replay[2]["message"]["content"],
            json!(TOOL_INTERRUPTED_ON_RESUME_OUTPUT)
        );
    }

    #[test]
    fn messages_are_deferred_while_a_tool_result_is_pending() {
        let fold = fold(vec![
            json!({"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"s1"},"time":1}),
            json!({"type":"context.append_loop_event","event":{"type":"tool.call","stepUuid":"s1","toolCallId":"c1","name":"Read","args":null},"time":2}),
            json!({"type":"context.append_message","message":{"id":"m2","role":"user","content":[{"type":"text","text":"next"}],"origin":{"kind":"user"}},"time":3}),
            json!({"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"c1","result":{"output":"done","isError":false}},"time":4}),
        ]);
        assert_eq!(types(&fold), vec!["message", "message", "message"]);
        assert_eq!(fold.replay[1]["message"]["role"], json!("tool"));
        assert_eq!(fold.replay[1]["message"]["content"], json!("done"));
        assert_eq!(fold.replay[1]["message"]["isError"], json!(false));
        assert_eq!(fold.replay[2]["message"]["id"], json!("m2"));
    }

    #[test]
    fn undo_removes_the_last_user_turn_and_its_replay_records() {
        let fold = fold(vec![
            json!({"type":"context.append_message","message":{"id":"u1","role":"user","content":[{"type":"text","text":"one"}],"origin":{"kind":"user"}},"time":1}),
            json!({"type":"context.append_message","message":{"id":"i1","role":"user","content":[{"type":"text","text":"injected"}],"origin":{"kind":"injection"}},"time":2}),
            json!({"type":"context.append_message","message":{"id":"u2","role":"user","content":[{"type":"text","text":"two"}],"origin":{"kind":"user"}},"time":3}),
            json!({"type":"context.undo","count":1,"time":4}),
        ]);
        assert_eq!(types(&fold), vec!["message", "message"]);
        assert_eq!(fold.replay[0]["message"]["id"], json!("u1"));
        assert_eq!(fold.replay[1]["message"]["id"], json!("i1"));
    }

    #[test]
    fn compaction_records_patch_the_last_compaction_and_rebuild_history() {
        let fold = fold(vec![
            json!({"type":"full_compaction.begin","instruction":"summarize","time":1}),
            json!({"type":"context.apply_compaction","summary":"sum","compactedCount":0,"tokensBefore":100,"tokensAfter":10,"keptUserMessageCount":0,"time":2}),
        ]);
        assert_eq!(types(&fold), vec!["compaction"]);
        let record = &fold.replay[0];
        assert_eq!(record["instruction"], json!("summarize"));
        assert_eq!(record["result"]["summary"], json!("sum"));
        assert_eq!(record["result"]["contextSummary"], json!("sum"));
        assert_eq!(record["result"]["tokensBefore"], json!(100));
        assert_eq!(record["result"]["tokensAfter"], json!(10));
        assert_eq!(record["result"]["keptUserMessageCount"], json!(0));
    }

    #[test]
    fn cancelled_compaction_is_patched_in_place() {
        let fold = fold(vec![
            json!({"type":"full_compaction.begin","instruction":"summarize","time":1}),
            json!({"type":"full_compaction.cancel","time":2}),
        ]);
        assert_eq!(fold.replay[0]["result"], json!("cancelled"));
    }

    #[test]
    fn goal_plan_config_permission_approval_and_tool_store_records() {
        let fold = fold(vec![
            json!({"type":"goal.create","goalId":"g1","objective":"ship","time":1}),
            json!({"type":"goal.update","status":"complete","reason":"done","turnsUsed":2,"tokensUsed":10,"wallClockMs":5,"budgetLimits":{"tokenBudget":10,"turnBudget":3},"time":2}),
            json!({"type":"plan_mode.enter","time":3}),
            json!({"type":"spec_mode.exit","time":4}),
            json!({"type":"config.update","modelAlias":"k2","thinkingEffort":"high","time":5}),
            json!({"type":"permission.set_mode","mode":"yolo","time":6}),
            json!({"type":"permission.record_approval_result","turnId":1,"toolCallId":"c1","toolName":"Bash","action":"allow","result":"approved","time":7}),
            json!({"type":"tools.update_store","key":"k","value":{"a":1},"time":8}),
        ]);
        assert_eq!(
            types(&fold),
            vec![
                "goal_updated",
                "goal_updated",
                "plan_updated",
                "spec_updated",
                "config_updated",
                "permission_updated",
                "approval_result"
            ]
        );
        assert_eq!(fold.replay[0]["change"]["kind"], json!("created"));
        assert_eq!(fold.replay[0]["snapshot"]["status"], json!("active"));
        assert_eq!(fold.replay[1]["change"]["kind"], json!("completion"));
        assert_eq!(fold.replay[1]["change"]["reason"], json!("done"));
        assert_eq!(fold.replay[1]["change"]["stats"]["turnsUsed"], json!(2));
        assert_eq!(fold.replay[1]["snapshot"]["terminalReason"], json!("done"));
        assert_eq!(
            fold.replay[1]["snapshot"]["budget"]["overBudget"],
            json!(true)
        );
        assert_eq!(
            fold.replay[1]["snapshot"]["budget"]["remainingTokens"],
            json!(0)
        );
        assert_eq!(fold.replay[2]["enabled"], json!(true));
        assert_eq!(fold.replay[3]["enabled"], json!(false));
        assert_eq!(fold.replay[4]["config"]["modelAlias"], json!("k2"));
        assert_eq!(fold.replay[4]["config"]["thinkingLevel"], json!("high"));
        assert_eq!(fold.replay[5]["mode"], json!("yolo"));
        assert_eq!(fold.replay[6]["record"]["toolName"], json!("Bash"));
        assert_eq!(fold.replay[6]["record"]["result"], json!("approved"));
        assert_eq!(fold.tool_store["k"], json!({"a": 1}));
    }

    #[test]
    fn forked_clears_the_goal_and_reminds_once() {
        let fold = fold(vec![
            json!({"type":"goal.create","goalId":"g1","objective":"ship","time":1}),
            json!({"type":"forked","time":2}),
            json!({"type":"goal.update","status":"complete","time":3}),
        ]);
        assert_eq!(types(&fold), vec!["goal_updated", "message"]);
        assert_eq!(
            fold.replay[1]["message"]["origin"]["name"],
            json!("goal_fork_cleared")
        );
        assert!(
            fold.replay[1]["message"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(GOAL_FORK_CLEARED_REMINDER)
        );
    }

    #[test]
    fn cancelled_turn_ids_are_skipped_by_later_prompts() {
        let records = vec![
            json!({"type":"turn.cancel","turnId":0,"target":"pending","time":1}),
            json!({"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"origin":{"kind":"user"},"time":2}),
            json!({"type":"context.append_message","message":{"id":"m1","role":"user","content":[{"type":"text","text":"hi"}],"origin":{"kind":"user"}},"time":3}),
        ];
        let fold = fold(records);
        assert_eq!(fold.replay[0]["turnId"], json!(1));
    }

    #[test]
    fn visible_turn_inputs_honor_origin_and_slash_triggers() {
        let records = vec![
            json!({"type":"turn.prompt","input":[{"type":"text","text":"a"}],"origin":{"kind":"user"},"time":1}),
            json!({"type":"turn.prompt","input":[{"type":"text","text":"b"}],"origin":{"kind":"skill_activation","trigger":"user-slash"},"time":2}),
            json!({"type":"turn.prompt","input":[{"type":"text","text":"c"}],"origin":{"kind":"skill_activation","trigger":"model"},"time":3}),
            json!({"type":"context.append_message","message":{"id":"m1","role":"user","content":[{"type":"text","text":"a"}],"origin":{"kind":"user"}},"time":4}),
        ];
        let matches = match_wire_turn_input_records(&records, false);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].append_index, 3);
        assert_eq!(matches[0].input_index, Some(0));
        assert_eq!(matches[0].turn_id, Some(0));
    }

    #[test]
    fn fold_wire_type_whitelists_match_v2() {
        assert_eq!(FOLD_RELEVANT_WIRE_TYPES.len(), 26);
        assert_eq!(FOLD_CARRYOVER_WIRE_TYPES.len(), 13);
        for event_type in FOLD_CARRYOVER_WIRE_TYPES {
            assert!(is_fold_relevant_wire_type(event_type), "{event_type}");
        }
        assert!(is_fold_carryover_wire_type("goal.account_usage"));
        assert!(!is_fold_carryover_wire_type("config.update"));
        assert!(!is_fold_relevant_wire_type("turn.ended"));
        assert!(is_fold_relevant_wire_type("context.append_loop_event"));
    }

    #[test]
    fn compaction_numbers_pass_floats_through_and_keep_integers() {
        let fold = fold(vec![
            json!({"type":"full_compaction.begin","instruction":"x","time":1}),
            json!({"type":"context.apply_compaction","summary":"s","compactedCount":0,"tokensBefore":12.5,"tokensAfter":3.25,"keptUserMessageCount":1,"keptHeadUserMessageCount":2,"droppedCount":3,"time":2}),
        ]);
        let result = &fold.replay[0]["result"];
        assert_eq!(result["tokensBefore"], json!(12.5));
        assert_eq!(result["tokensAfter"], json!(3.25));
        assert_eq!(result["keptUserMessageCount"], json!(1));
        assert_eq!(result["keptHeadUserMessageCount"], json!(2));
        assert_eq!(result["droppedCount"], json!(3));
    }
}
