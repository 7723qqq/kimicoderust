use std::collections::HashSet;

use super::super::protocol::paths::{BROADCAST_NAME, TOWER_NAME};
use super::super::protocol::store::TowerSendResult;
use super::super::protocol::types::TowerState;
use super::super::towerOps::TowerInboxSent;

pub const TOWER_INBOX_STEER_VARIANT: &str = "tower_inbox_steer";
pub const STEER_SUBJECT_PREVIEW_MAX: usize = 120;

pub trait TowerDeliveryHost: Send + Sync {
    fn publish_tower_inbox_sent(&self, event: &TowerInboxSent);
    fn running_agent_ids(&self) -> Option<HashSet<String>>;
    fn agent_loop_running(&self, agent_id: &str) -> Option<bool>;
    fn steer_agent(&self, agent_id: &str, text: &str) -> Result<(), String>;
}

pub fn deliver_tower_message(
    host: &dyn TowerDeliveryHost,
    state: &TowerState,
    sent: &TowerSendResult,
) -> String {
    let item = &sent.item;
    if item.from != TOWER_NAME
        && (item.to == TOWER_NAME || item.to == BROADCAST_NAME)
    {
        host.publish_tower_inbox_sent(&TowerInboxSent {
            from: item.from.clone(),
            to: item.to.clone(),
            subject: item.subject.clone(),
            message_id: Some(item.message_id.clone()),
            file: Some(item.file.clone()),
            sent_at: Some(item.sent_at.clone()),
            mission_id: sent.mission_id.clone(),
        });
    }
    let entry = if item.to == TOWER_NAME || item.to == BROADCAST_NAME {
        None
    } else {
        state
            .roster
            .agents
            .iter()
            .find(|agent| agent.name == item.to)
    };
    let undelivered = match (entry, host.running_agent_ids()) {
        (Some(entry), Some(running)) if item.from == TOWER_NAME => {
            !running.contains(&entry.agent_id)
        }
        _ => false,
    };
    let note = match (undelivered, entry) {
        (true, Some(entry)) => format!(
            "\nnote: {} has no running task in this session — the message sits in its inbox until you deliver it with Agent(resume=\"{}\", run_in_background=true, prompt=\"...\")",
            item.to, entry.agent_id
        ),
        _ => String::new(),
    };
    format!("{}{note}", steer_into_running_agent(host, state, sent))
}

fn steer_into_running_agent(
    host: &dyn TowerDeliveryHost,
    state: &TowerState,
    sent: &TowerSendResult,
) -> String {
    let item = &sent.item;
    if item.to == TOWER_NAME || item.to == BROADCAST_NAME {
        return String::new();
    }
    let Some(entry) = state
        .roster
        .agents
        .iter()
        .find(|agent| agent.name == item.to)
    else {
        return String::new();
    };
    if host.agent_loop_running(&entry.agent_id) != Some(true) {
        return String::new();
    }
    let subject = if item.subject.chars().count() > STEER_SUBJECT_PREVIEW_MAX {
        format!(
            "{}…",
            item.subject
                .chars()
                .take(STEER_SUBJECT_PREVIEW_MAX)
                .collect::<String>()
        )
    } else {
        item.subject.clone()
    };
    let text = format!(
        "[tower inbox] new message from {}: \"{subject}\" — read it now with TowerInbox and act on it before continuing.",
        item.from
    );
    match host.steer_agent(&entry.agent_id, &text) {
        Ok(()) => format!(
            "\nnote: {} is mid-turn — the message was steered into its running turn; it sees the directive at its next step boundary.",
            item.to
        ),
        Err(_) => String::new(),
    }
}
