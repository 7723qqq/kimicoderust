use std::collections::HashSet;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::fileHistory::{
    FileBackupEntry, FileHistoryCheckpointPhase, FileHistoryCheckpointRecord, FileHistoryState,
};

pub const FILE_HISTORY_TURN_WINDOW: usize = 5;
pub const FILE_HISTORY_RECORD_PREFIX: &str = "file_history.";
pub const FILE_HISTORY_STATE_KEY: &str = "fileHistory";
pub const FILE_HISTORY_TRACKED_TYPE: &str = "file_history.tracked";
pub const FILE_HISTORY_CHECKPOINTED_TYPE: &str = "file_history.checkpoint";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistoryTracked {
    pub agent_id: String,
    pub turn_id: u64,
    pub path: String,
    pub entry: FileBackupEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistoryCheckpointed {
    pub agent_id: String,
    pub turn_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<FileHistoryCheckpointPhase>,
    pub entries: IndexMap<String, FileBackupEntry>,
}

pub fn checkpoint_phase_of(
    record: &FileHistoryCheckpointRecord,
) -> FileHistoryCheckpointPhase {
    record.phase.unwrap_or(FileHistoryCheckpointPhase::Start)
}

pub fn displaced_checkpoints(
    checkpoints: &[FileHistoryCheckpointRecord],
    completing_turn_id: Option<u64>,
) -> Vec<FileHistoryCheckpointRecord> {
    let mut completed_ids: Vec<u64> = checkpoints
        .iter()
        .filter(|checkpoint| checkpoint_phase_of(checkpoint) == FileHistoryCheckpointPhase::End)
        .map(|checkpoint| checkpoint.turn_id)
        .chain(completing_turn_id)
        .collect();
    completed_ids.sort_unstable_by(|left, right| right.cmp(left));
    completed_ids.dedup();
    if completed_ids.len() <= FILE_HISTORY_TURN_WINDOW {
        return Vec::new();
    }
    let keep: HashSet<u64> = completed_ids
        .iter()
        .take(FILE_HISTORY_TURN_WINDOW)
        .copied()
        .collect();
    let newest = completed_ids[0];
    checkpoints
        .iter()
        .filter(|checkpoint| !keep.contains(&checkpoint.turn_id) && checkpoint.turn_id <= newest)
        .cloned()
        .collect()
}

pub fn apply_checkpointed(state: &mut FileHistoryState, event: &FileHistoryCheckpointed) {
    let phase = event.phase.unwrap_or(FileHistoryCheckpointPhase::Start);
    if let Some(existing) = state.checkpoints.iter_mut().find(|checkpoint| {
        checkpoint.turn_id == event.turn_id && checkpoint_phase_of(checkpoint) == phase
    }) {
        existing.entries = event.entries.clone();
        return;
    }
    state.checkpoints.push(FileHistoryCheckpointRecord {
        turn_id: event.turn_id,
        phase: Some(phase),
        entries: event.entries.clone(),
    });
    let displaced = displaced_checkpoints(&state.checkpoints, None);
    if displaced.is_empty() {
        return;
    }
    state.checkpoints.retain(|checkpoint| {
        !displaced.iter().any(|candidate| {
            candidate.turn_id == checkpoint.turn_id
                && checkpoint_phase_of(candidate) == checkpoint_phase_of(checkpoint)
        })
    });
    let kept: HashSet<String> = state
        .checkpoints
        .iter()
        .flat_map(|checkpoint| checkpoint.entries.keys().cloned())
        .collect();
    state.tracked.retain(|path| kept.contains(path));
}

pub fn apply_tracked(state: &mut FileHistoryState, event: &FileHistoryTracked) {
    if !state.tracked.contains(&event.path) {
        state.tracked.push(event.path.clone());
    }
    let index = state
        .checkpoints
        .iter()
        .position(|checkpoint| {
            checkpoint.turn_id == event.turn_id
                && checkpoint_phase_of(checkpoint) == FileHistoryCheckpointPhase::Start
        })
        .unwrap_or_else(|| {
            state.checkpoints.push(FileHistoryCheckpointRecord {
                turn_id: event.turn_id,
                phase: Some(FileHistoryCheckpointPhase::Start),
                entries: IndexMap::new(),
            });
            state.checkpoints.len() - 1
        });
    state.checkpoints[index]
        .entries
        .entry(event.path.clone())
        .or_insert_with(|| event.entry.clone());
}
