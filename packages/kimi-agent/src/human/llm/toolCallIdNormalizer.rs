use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::human::llm::message::{StreamIndex, ToolCall};

#[derive(Debug, Clone, Default)]
pub struct ToolCallIdSeedMessage {
    pub role: Option<String>,
    pub tool_calls: Option<Vec<ToolCallIdSeed>>,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolCallIdSeed {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallIdRemap {
    pub raw: String,
    pub assigned: String,
}

#[derive(Clone, Default)]
pub struct ToolCallIdNormalizer {
    seen: Arc<Mutex<HashSet<String>>>,
    seeded: bool,
}

impl ToolCallIdNormalizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed_from(&mut self, messages: &[ToolCallIdSeedMessage]) {
        if self.seeded {
            return;
        }
        self.seeded = true;
        let mut seen = self.seen.lock();
        for message in messages {
            for call in message.tool_calls.as_deref().unwrap_or(&[]) {
                seen.insert(call.id.clone());
            }
            if let Some(tool_call_id) = &message.tool_call_id {
                seen.insert(tool_call_id.clone());
            }
        }
    }

    pub fn begin_response(&self) -> ToolCallIdResponseNormalizer {
        ToolCallIdResponseNormalizer {
            seen: self.seen.clone(),
            assigned_by_index: HashMap::new(),
            occurrences_by_raw_id: HashMap::new(),
            claimed: Vec::new(),
            remapped: Vec::new(),
        }
    }
}

pub struct ToolCallIdResponseNormalizer {
    seen: Arc<Mutex<HashSet<String>>>,
    assigned_by_index: HashMap<StreamIndex, String>,
    occurrences_by_raw_id: HashMap<String, Vec<String>>,
    claimed: Vec<String>,
    pub remapped: Vec<ToolCallIdRemap>,
}

impl ToolCallIdResponseNormalizer {
    pub fn remap_streamed_id(
        &mut self,
        raw_id: &str,
        stream_index: Option<StreamIndex>,
    ) -> String {
        if let Some(index) = &stream_index
            && let Some(existing) = self.assigned_by_index.get(index)
        {
            return existing.clone();
        }
        let occurrence = self
            .occurrences_by_raw_id
            .get(raw_id)
            .map_or(0, Vec::len);
        let assigned = self.claim(raw_id, occurrence);
        self.occurrences_by_raw_id
            .entry(raw_id.to_string())
            .or_default()
            .push(assigned.clone());
        if let Some(index) = stream_index {
            self.assigned_by_index.insert(index, assigned.clone());
        }
        assigned
    }

    pub fn remap_finalized_calls(&mut self, tool_calls: Vec<ToolCall>) -> Vec<ToolCall> {
        if tool_calls.is_empty() {
            return tool_calls;
        }
        let mut counts = HashMap::<String, usize>::new();
        let mut changed = false;
        let result = tool_calls
            .iter()
            .map(|call| {
                let occurrence = counts.get(&call.id).copied().unwrap_or(0);
                counts.insert(call.id.clone(), occurrence + 1);
                let assigned = self
                    .occurrences_by_raw_id
                    .get(&call.id)
                    .and_then(|occurrences| occurrences.get(occurrence).cloned())
                    .unwrap_or_else(|| self.claim(&call.id, occurrence));
                if assigned == call.id {
                    return call.clone();
                }
                changed = true;
                ToolCall {
                    id: assigned,
                    raw_id: call.raw_id.clone().or_else(|| Some(call.id.clone())),
                    ..call.clone()
                }
            })
            .collect::<Vec<_>>();
        if changed { result } else { tool_calls }
    }

    pub fn rollback(&mut self) {
        let mut seen = self.seen.lock();
        for id in &self.claimed {
            seen.remove(id);
        }
    }

    fn claim(&mut self, raw_id: &str, occurrence: usize) -> String {
        if occurrence == 0 && !self.seen.lock().contains(raw_id) {
            self.seen.lock().insert(raw_id.to_string());
            self.claimed.push(raw_id.to_string());
            return raw_id.to_string();
        }
        let mut n = (occurrence + 1).max(2);
        let mut candidate = format!("{raw_id}__{n}");
        while self.seen.lock().contains(&candidate) {
            n += 1;
            candidate = format!("{raw_id}__{n}");
        }
        self.seen.lock().insert(candidate.clone());
        self.claimed.push(candidate.clone());
        self.remapped.push(ToolCallIdRemap {
            raw: raw_id.to_string(),
            assigned: candidate.clone(),
        });
        candidate
    }
}
