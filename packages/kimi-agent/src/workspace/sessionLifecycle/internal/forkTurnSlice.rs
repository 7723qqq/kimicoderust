use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::agent::context_memory::hook_parts::is_user_prompt_submit_hook_part;
use crate::agent::prompt::prompt_metadata_text::{
    prompt_metadata_text_from_content_parts, prompt_metadata_text_from_text,
};
use crate::agent::replay_builder::fold::match_wire_turn_input_records;
use crate::features::fileHistory::fileHistoryOps::FILE_HISTORY_RECORD_PREFIX;
use crate::session::session_metadata::session_metadata::SessionLastTurnReason;
use crate::human::agent::origin::{
    annotate_bundled_skill_parts, is_skill_activation_part,
};
use crate::wire::record::{WireAsyncError, WireRecord};
use kimi_core::primitives::errors::{Error2, Error2Options};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastTurnReason {
    Completed,
    Cancelled,
    Failed,
}

impl LastTurnReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

impl From<LastTurnReason> for SessionLastTurnReason {
    fn from(reason: LastTurnReason) -> Self {
        match reason {
            LastTurnReason::Completed => Self::Completed,
            LastTurnReason::Cancelled => Self::Cancelled,
            LastTurnReason::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MainTurnSlice {
    pub records: Vec<WireRecord>,
    pub cutoff_time: Option<f64>,
    pub last_prompt: Option<String>,
    pub last_turn_reason: Option<LastTurnReason>,
}

pub fn assert_fork_turn_index(turn_index: Option<f64>) -> Result<(), WireAsyncError> {
    let Some(turn_index) = turn_index else {
        return Ok(());
    };
    if turn_index.is_finite()
        && turn_index.fract() == 0.0
        && turn_index >= 0.0
        && turn_index <= 9_007_199_254_740_991.0
    {
        return Ok(());
    }
    let mut details = Map::new();
    details.insert("turnIndex".to_string(), turn_index.into());
    Err(Box::new(Error2::with_options(
        "request.invalid",
        "forkSession turnIndex must be a non-negative safe integer",
        Error2Options {
            details: Some(details),
            ..Default::default()
        },
    )))
}

pub fn slice_records_before_pending_work(
    records: &[WireRecord],
    turn_id: Option<i64>,
    queued_prompt_ids: &[String],
) -> Result<Vec<WireRecord>, WireAsyncError> {
    let turn_end = match turn_id {
        None => -1,
        Some(turn_id) => records
            .iter()
            .position(|record| {
                matches!(record_type(record), Some("agent.turn.started" | "turn.prompt"))
                    && number_field(record, "turnId") == Some(turn_id as f64)
            })
            .map(|index| index as i64)
            .unwrap_or(-1),
    };
    if turn_id.is_some() && turn_end == -1 {
        let turn_id = turn_id.unwrap();
        let mut details = Map::new();
        details.insert("turnId".to_string(), turn_id.into());
        return Err(Box::new(Error2::with_options(
            "internal",
            format!("The active turn {turn_id} boundary was not found in the session wire"),
            Error2Options {
                details: Some(details),
                ..Default::default()
            },
        )));
    }
    let queued = queued_prompt_ids.iter().collect::<HashSet<_>>();
    let queue_end = if queued.is_empty() {
        -1
    } else {
        records
            .iter()
            .position(|record| {
                if record_type(record) != Some("agent.input.submitted") {
                    return false;
                }
                let entry = as_record(record.get("entry"));
                let meta = entry.and_then(|entry| as_record(entry.get("meta")));
                let prompt_id = meta
                    .and_then(|meta| meta.get("promptId"))
                    .filter(|prompt_id| !prompt_id.is_null())
                    .or_else(|| record.get("id"));
                prompt_id
                    .and_then(Value::as_str)
                    .is_some_and(|prompt_id| queued.iter().any(|queued_id| queued_id.as_str() == prompt_id))
            })
            .map(|index| index as i64)
            .unwrap_or(-1)
    };
    let end = match (turn_end, queue_end) {
        (-1, end) => end,
        (end, -1) => end,
        (turn_end, queue_end) => turn_end.min(queue_end),
    };
    Ok(if end == -1 {
        records.to_vec()
    } else {
        records[..end as usize].to_vec()
    })
}

pub fn slice_main_records_at_latest_turn(
    records: &[WireRecord],
    source_session_id: &str,
) -> Result<MainTurnSlice, WireAsyncError> {
    let turns = persistent_turn_starts(records);
    let legacy_turns = context_only_turn_starts(records);
    let latest = turns.last();
    let latest_legacy = legacy_turns.last();
    if let Some(latest_legacy) = latest_legacy
        && latest.is_none_or(|latest| latest_legacy.start > latest.start)
    {
        return slice_main_records_at_legacy_turn(
            records,
            source_session_id,
            latest_legacy.turn_index,
        );
    }
    if let Some(latest) = latest {
        return slice_main_records_at_turn(records, source_session_id, latest.id);
    }
    Ok(MainTurnSlice::default())
}

pub fn slice_main_records_for_running_fork(
    records: &[WireRecord],
    source_session_id: &str,
    active_turn_id: Option<i64>,
    queued_prompt_ids: &[String],
) -> Result<MainTurnSlice, WireAsyncError> {
    let completed = slice_records_before_pending_work(records, active_turn_id, queued_prompt_ids)?;
    let latest = slice_main_records_at_latest_turn(&completed, source_session_id)?;
    let Some(active_turn_id) = active_turn_id else {
        return Ok(latest);
    };
    let Some(prompt_index) = records.iter().position(|record| {
        record_type(record) == Some("turn.prompt")
            && number_field(record, "turnId") == Some(active_turn_id as f64)
    }) else {
        return Ok(latest);
    };
    let prompt = &records[prompt_index];
    let record_values = records.iter().cloned().map(Value::Object).collect::<Vec<_>>();
    let match_ = match_wire_turn_input_records(&record_values, true)
        .into_iter()
        .find(|candidate| {
            candidate.input_index == Some(prompt_index) && candidate.turn_id == Some(active_turn_id)
        });
    let append = match match_ {
        Some(match_) => records[match_.append_index].clone(),
        None => user_message_record_from_prompt(prompt),
    };
    let mut output = latest.records.clone();
    output.push(prompt.clone());
    output.push(append);
    output.push(
        json!({
            "type": "turn.ended",
            "agentId": "main",
            "turnId": active_turn_id,
            "reason": "cancelled",
            "time": chrono::Utc::now().timestamp_millis(),
        })
        .as_object()
        .cloned()
        .unwrap(),
    );
    Ok(MainTurnSlice {
        records: output,
        cutoff_time: latest.cutoff_time,
        last_prompt: prompt_metadata_from_turn_record(prompt),
        last_turn_reason: Some(LastTurnReason::Cancelled),
    })
}

fn user_message_record_from_prompt(prompt: &WireRecord) -> WireRecord {
    let mut message = Map::new();
    message.insert("role".to_string(), "user".into());
    if let Some(prompt_id) = prompt.get("promptId").and_then(Value::as_str) {
        message.insert("id".to_string(), prompt_id.into());
    }
    if let Some(input) = prompt.get("input") {
        message.insert("content".to_string(), input.clone());
    }
    if let Some(origin) = prompt.get("origin") {
        message.insert("origin".to_string(), origin.clone());
    }
    let mut output = Map::new();
    output.insert("type".to_string(), "context.append_message".into());
    output.insert("message".to_string(), Value::Object(message));
    if let Some(time) = prompt.get("time") {
        output.insert("time".to_string(), time.clone());
    }
    output
}

pub fn slice_main_records_at_turn(
    records: &[WireRecord],
    source_session_id: &str,
    turn_index: i64,
) -> Result<MainTurnSlice, WireAsyncError> {
    let turns = persistent_turn_starts(records);
    let legacy_turns = context_only_turn_starts(records);
    let selected = turns.iter().find(|turn| turn.id == turn_index);
    let Some(selected) = selected else {
        if legacy_turns.iter().any(|turn| turn.turn_index == turn_index) {
            return slice_main_records_at_legacy_turn(records, source_session_id, turn_index);
        }
        let mut details = Map::new();
        details.insert("turnIndex".to_string(), turn_index.into());
        details.insert(
            "availableTurns".to_string(),
            (turns.len() + legacy_turns.len()).into(),
        );
        return Err(Box::new(Error2::with_options(
            "request.invalid",
            format!("Turn {turn_index} was not found in session \"{source_session_id}\""),
            Error2Options {
                details: Some(details),
                ..Default::default()
            },
        )));
    };
    let next = turns.iter().find(|turn| turn.start > selected.start);
    let next_legacy = legacy_turns.iter().find(|turn| turn.start > selected.start);
    let end = next
        .map(|turn| turn.start)
        .unwrap_or(records.len())
        .min(next_legacy.map(|turn| turn.start).unwrap_or(records.len()));
    let retained_turn_ids = turns
        .iter()
        .filter(|turn| turn.start < end)
        .map(|turn| turn.id)
        .collect::<HashSet<_>>();
    let persistent_by_prompt_index = turns
        .iter()
        .map(|turn| (turn.prompt_index, turn.id))
        .collect::<HashMap<_, _>>();
    let visible_turn_count = records[..end]
        .iter()
        .filter(|record| is_user_visible_turn_record(record))
        .count() as i64;
    let retained_turn_inputs = turn_input_indices_through(records, visible_turn_count - 1);
    let retained = records[..end]
        .iter()
        .enumerate()
        .filter(|(index, record)| {
            if record_type(record).is_some_and(|type_| type_.starts_with(FILE_HISTORY_RECORD_PREFIX)) {
                return false;
            }
            if !matches!(record_type(record), Some("turn.prompt" | "turn.steer")) {
                return true;
            }
            if let Some(turn_id) = number_field(record, "turnId") {
                return retained_turn_ids.contains(&(turn_id as i64));
            }
            match persistent_by_prompt_index.get(index) {
                Some(turn_id) => retained_turn_ids.contains(turn_id),
                None => retained_turn_inputs.contains(index),
            }
        })
        .map(|(_, record)| record.clone())
        .collect::<Vec<_>>();
    let cutoff_time = retained.iter().filter_map(record_time).reduce(f64::max);
    Ok(MainTurnSlice {
        last_prompt: prompt_metadata_from_turn_record(&records[selected.prompt_index]),
        last_turn_reason: last_turn_reason_of(&retained),
        records: retained,
        cutoff_time,
    })
}

fn slice_main_records_at_legacy_turn(
    records: &[WireRecord],
    source_session_id: &str,
    turn_index: i64,
) -> Result<MainTurnSlice, WireAsyncError> {
    let turn_starts = records
        .iter()
        .enumerate()
        .filter(|(_, record)| is_user_visible_turn_record(record))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let Some(start) = turn_starts.get(turn_index as usize).copied() else {
        let mut details = Map::new();
        details.insert("turnIndex".to_string(), turn_index.into());
        details.insert("availableTurns".to_string(), turn_starts.len().into());
        return Err(Box::new(Error2::with_options(
            "request.invalid",
            format!("Turn {turn_index} was not found in session \"{source_session_id}\""),
            Error2Options {
                details: Some(details),
                ..Default::default()
            },
        )));
    };
    let next_persistent = persistent_turn_starts(records)
        .into_iter()
        .find(|turn| turn.start > start);
    let end = turn_starts
        .get(turn_index as usize + 1)
        .copied()
        .unwrap_or(records.len())
        .min(next_persistent.map(|turn| turn.start).unwrap_or(records.len()));
    let retained_turn_inputs = turn_input_indices_through(records, turn_index);
    let retained = records[..end]
        .iter()
        .enumerate()
        .filter(|(index, record)| {
            !record_type(record).is_some_and(|type_| type_.starts_with(FILE_HISTORY_RECORD_PREFIX))
                && (!is_user_visible_turn_input_record(record)
                    || retained_turn_inputs.contains(index))
        })
        .map(|(_, record)| record.clone())
        .collect::<Vec<_>>();
    let cutoff_time = retained.iter().filter_map(record_time).reduce(f64::max);
    let last_prompt = prompt_metadata_from_turn_record(&records[start]);
    Ok(MainTurnSlice {
        last_turn_reason: last_turn_reason_of(&retained),
        records: retained,
        cutoff_time,
        last_prompt,
    })
}

#[derive(Debug, Clone)]
struct LegacyTurnStart {
    start: usize,
    turn_index: i64,
}

#[derive(Debug, Clone)]
struct PersistentTurnStart {
    id: i64,
    start: usize,
    prompt_index: usize,
}

fn context_only_turn_starts(records: &[WireRecord]) -> Vec<LegacyTurnStart> {
    let record_values = records.iter().cloned().map(Value::Object).collect::<Vec<_>>();
    match_wire_turn_input_records(&record_values, false)
        .into_iter()
        .filter(|match_| match_.input_index.is_none())
        .map(|match_| LegacyTurnStart {
            start: match_.append_index,
            turn_index: match_.visible_turn_index as i64,
        })
        .collect()
}

fn persistent_turn_starts(records: &[WireRecord]) -> Vec<PersistentTurnStart> {
    let mut turns: Vec<PersistentTurnStart> = Vec::new();
    let mut human_starts = HashMap::<i64, usize>::new();
    let mut terminal_turn_ids = HashSet::new();
    let mut cancelled_turn_ids = HashSet::new();
    let mut next_turn_id = 0;
    for (index, record) in records.iter().enumerate() {
        match record_type(record) {
            Some("agent.turn.started") => {
                if let Some(turn_id) = number_field(record, "turnId").map(|id| id as i64) {
                    human_starts
                        .entry(turn_id)
                        .and_modify(|start| *start = (*start).min(index))
                        .or_insert(index);
                    if let Some(existing) = turns.iter_mut().find(|turn| turn.id == turn_id) {
                        existing.start = existing.start.min(index);
                    }
                }
            }
            Some("turn.ended" | "agent.turn.ended") => {
                if let Some(turn_id) = number_field(record, "turnId").map(|id| id as i64) {
                    terminal_turn_ids.insert(turn_id);
                }
            }
            Some("turn.cancel") => {
                if record.contains_key("target") {
                    if let Some(turn_id) = number_field(record, "turnId").map(|id| id as i64) {
                        if turn_id >= next_turn_id {
                            cancelled_turn_ids.insert(turn_id);
                            while cancelled_turn_ids.remove(&next_turn_id) {
                                next_turn_id += 1;
                            }
                        }
                    }
                }
            }
            Some("turn.prompt") => {
                let id = number_field(record, "turnId")
                    .map(|id| id as i64)
                    .unwrap_or(next_turn_id);
                next_turn_id = next_turn_id.max(id + 1);
                while cancelled_turn_ids.remove(&next_turn_id) {
                    next_turn_id += 1;
                }
                turns.push(PersistentTurnStart {
                    id,
                    start: index.min(human_starts.get(&id).copied().unwrap_or(index)),
                    prompt_index: index,
                });
            }
            _ => {}
        }
    }
    let retained_turn_inputs = turn_input_indices_through(records, i64::MAX);
    turns
        .into_iter()
        .filter(|turn| {
            retained_turn_inputs.contains(&turn.prompt_index) || terminal_turn_ids.contains(&turn.id)
        })
        .collect()
}

pub fn slice_subagent_records_at_time(
    records: &[WireRecord],
    cutoff_time: Option<f64>,
) -> Vec<WireRecord> {
    let Some(cutoff_time) = cutoff_time else {
        return Vec::new();
    };
    let end = records
        .iter()
        .position(|record| record_time(record).is_some_and(|time| time > cutoff_time))
        .unwrap_or(records.len());
    records[..end].to_vec()
}

fn last_turn_reason_of(records: &[WireRecord]) -> Option<LastTurnReason> {
    for record in records.iter().rev() {
        if record_type(record) == Some("turn.ended") {
            match record.get("reason").and_then(Value::as_str) {
                Some("completed") => return Some(LastTurnReason::Completed),
                Some("cancelled") => return Some(LastTurnReason::Cancelled),
                Some("failed" | "blocked") => return Some(LastTurnReason::Failed),
                _ => {}
            }
        }
        if record_type(record) == Some("agent.turn.ended") {
            match record.get("outcome").and_then(Value::as_str) {
                Some("done") => return Some(LastTurnReason::Completed),
                Some("aborted") => return Some(LastTurnReason::Cancelled),
                Some("failed") => return Some(LastTurnReason::Failed),
                _ => {}
            }
        }
    }
    None
}

fn is_user_visible_turn_record(record: &WireRecord) -> bool {
    if record_type(record) != Some("context.append_message") {
        return false;
    }
    let Some(message) = as_record(record.get("message")) else {
        return false;
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let origin = as_record(message.get("origin"));
    match origin.and_then(|origin| origin.get("kind")) {
        None => true,
        Some(Value::String(kind)) if kind == "user" => true,
        Some(Value::String(kind))
            if matches!(kind.as_str(), "skill_activation" | "plugin_command") =>
        {
            origin
                .and_then(|origin| origin.get("trigger"))
                .and_then(Value::as_str)
                == Some("user-slash")
        }
        Some(Value::String(kind)) if kind == "shell_command" => {
            origin.and_then(|origin| origin.get("phase")).and_then(Value::as_str) == Some("input")
        }
        _ => false,
    }
}

fn is_user_visible_turn_input_record(record: &WireRecord) -> bool {
    if !matches!(record_type(record), Some("turn.prompt" | "turn.steer")) {
        return false;
    }
    let origin = as_record(record.get("origin"));
    match origin_kind(origin) {
        Some("user") => true,
        Some("skill_activation" | "plugin_command") => {
            origin.and_then(|origin| origin.get("trigger")).and_then(Value::as_str)
                == Some("user-slash")
        }
        Some("shell_command") => {
            origin.and_then(|origin| origin.get("phase")).and_then(Value::as_str) == Some("input")
        }
        _ => false,
    }
}

fn turn_input_indices_through(records: &[WireRecord], turn_index: i64) -> HashSet<usize> {
    let record_values = records.iter().cloned().map(Value::Object).collect::<Vec<_>>();
    match_wire_turn_input_records(&record_values, false)
        .into_iter()
        .filter(|match_| {
            match_.visible_turn_index as i64 <= turn_index && match_.input_index.is_some()
        })
        .map(|match_| match_.input_index.unwrap())
        .collect()
}

fn record_time(record: &WireRecord) -> Option<f64> {
    if let Some(time) = record.get("time").and_then(Value::as_f64).filter(|time| time.is_finite()) {
        return Some(time);
    }
    if record_type(record) == Some("metadata") {
        return record
            .get("created_at")
            .and_then(Value::as_f64)
            .filter(|time| time.is_finite());
    }
    None
}

fn prompt_metadata_from_turn_record(record: &WireRecord) -> Option<String> {
    if record_type(record) == Some("turn.prompt") {
        return prompt_metadata_from_input(record.get("input"), as_record(record.get("origin")));
    }
    if record_type(record) != Some("context.append_message") {
        return None;
    }
    let message = as_record(record.get("message"))?;
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    prompt_metadata_from_input(message.get("content"), as_record(message.get("origin")))
}

fn prompt_metadata_from_input(
    content: Option<&Value>,
    origin: Option<&Map<String, Value>>,
) -> Option<String> {
    match origin_kind(origin) {
        Some("skill_activation") => {
            let name = origin?.get("skillName")?.as_str()?;
            return prompt_metadata_text_from_content_parts(
                &[kimi_core::types::ContentPart::Text {
                    text: slash_command_text(&format!("/{name}"), origin?.get("skillArgs")),
                    meta: None,
                }],
                origin?.get("clientMetadata"),
            );
        }
        Some("plugin_command") => {
            let plugin_id = origin?.get("pluginId")?.as_str()?;
            let command_name = origin?.get("commandName")?.as_str()?;
            return prompt_metadata_text_from_text(&slash_command_text(
                &format!("/{plugin_id}:{command_name}"),
                origin?.get("commandArgs"),
            ));
        }
        _ => {}
    }
    let content_values = content?.as_array()?;
    let content = content_values
        .iter()
        .map(|part| serde_json::from_value::<kimi_core::types::ContentPart>(part.clone()))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let activations = origin.and_then(|origin| origin.get("skillActivations"));
    let bundled: Vec<crate::human::agent::origin::BundledSkillActivation> = if origin_kind(origin) == Some("user") {
        activations
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|value| serde_json::from_value(value.clone()).ok())
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let parts = annotate_bundled_skill_parts(&content, &bundled)
        .into_iter()
        .filter(|part| !is_skill_activation_part(part) && !is_user_prompt_submit_hook_part(part))
        .collect::<Vec<_>>();
    prompt_metadata_text_from_content_parts(
        &parts,
        if origin_kind(origin) == Some("user") {
            origin.and_then(|origin| origin.get("clientMetadata"))
        } else {
            None
        },
    )
}

fn slash_command_text(command: &str, args: Option<&Value>) -> String {
    let trimmed = args.and_then(Value::as_str).map(str::trim);
    match trimmed {
        Some(args) if !args.is_empty() => format!("{command} {args}"),
        _ => command.to_string(),
    }
}

fn as_record(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn record_type(record: &WireRecord) -> Option<&str> {
    record.get("type").and_then(Value::as_str)
}

fn number_field(record: &WireRecord, key: &str) -> Option<f64> {
    record.get(key).and_then(Value::as_f64)
}

fn origin_kind(origin: Option<&Map<String, Value>>) -> Option<&str> {
    origin.and_then(|origin| origin.get("kind")).and_then(Value::as_str)
}
