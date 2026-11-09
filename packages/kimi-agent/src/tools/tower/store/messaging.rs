use std::collections::BTreeSet;

use super::*;
use crate::tools::tower::types::{
    TowerInboxDelivery, TowerInboxPage, TowerInboxSent, TowerSendOutcome,
};

fn new_message_id() -> String {
    let mut bytes = [0u8; 16];
    for chunk in bytes.as_chunks_mut::<8>().0 {
        chunk.copy_from_slice(&fastrand::u64(..).to_be_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn sent_from_item(
    item: TowerInboxItem,
    mission_id: Option<String>,
    created: bool,
) -> TowerInboxSent {
    let delivery = TowerInboxDelivery {
        from: item.from.clone(),
        to: item.to.clone(),
        subject: item.subject.clone(),
        message_id: item.message_id.clone(),
        file: item.file.clone(),
        sent_at: item.sent_at.clone(),
        mission_id: mission_id.clone(),
    };
    TowerInboxSent {
        item,
        delivery,
        mission_id,
        created,
        activity_log_error: None,
    }
}

fn ack_context(raw: &serde_json::Value, caller_name: &str) -> (BTreeSet<String>, Option<String>) {
    let agent = (caller_name != TOWER_NAME)
        .then(|| {
            raw.get("roster")?
                .get("agents")?
                .as_array()?
                .iter()
                .find(|agent| {
                    agent.get("name").and_then(serde_json::Value::as_str) == Some(caller_name)
                })
                .cloned()
        })
        .flatten();
    let owner = if caller_name == TOWER_NAME {
        Some(raw)
    } else {
        agent.as_ref()
    };
    let acked = owner
        .and_then(|value| value.get("inboxAckIds"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string)
        .collect();
    let cutoff = agent.and_then(|agent| {
        let last_read = agent
            .get("lastInboxReadAt")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        last_read.or_else(|| {
            agent
                .get("spawnedAt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
    });
    (acked, cutoff)
}

fn is_unread(
    item: &TowerInboxItem,
    caller_name: &str,
    acked: &BTreeSet<String>,
    cutoff: Option<&str>,
) -> bool {
    !acked.contains(&item.message_id)
        && (caller_name == TOWER_NAME || cutoff.is_none_or(|cutoff| item.sent_at.as_str() > cutoff))
}

pub(super) fn unread_inbox_count_from_items(
    raw: &serde_json::Value,
    caller_name: &str,
    items: &[TowerInboxItem],
) -> usize {
    let (acked, cutoff) = ack_context(raw, caller_name);
    items
        .iter()
        .filter(|item| {
            (caller_name == TOWER_NAME || item.to == caller_name || item.to == BROADCAST_NAME)
                && is_unread(item, caller_name, &acked, cutoff.as_deref())
        })
        .count()
}

impl TowerStore {
    pub async fn send_agent(
        &self,
        caller_agent_id: &str,
        input: TowerSendInput,
    ) -> Result<TowerSendOutcome, String> {
        let (state, caller, recovered_at, placeholder) =
            self.resolve_agent_operation(caller_agent_id).await?;
        let session_id = state.session_id.clone();

        let target_agent_id = self
            .find_agent(&state, input.to.trim())
            .map(|agent| agent.agent_id);
        self.with_inbox_publication_lock(move || async move {
            let message = self.send_detailed_locked(&caller, input, false).await?;
            Ok(TowerSendOutcome {
                caller,
                session_id,
                recovered_at,
                target_agent_id,
                placeholder,
                message,
            })
        })
        .await
    }

    pub(super) async fn find_detailed_message(
        &self,
        caller_name: &str,
        input: &TowerSendInput,
    ) -> Result<Option<TowerInboxItem>, String> {
        let body = input.body.trim();
        Ok(self
            .read_inbox(TOWER_NAME, usize::MAX)
            .await?
            .into_iter()
            .find(|item| {
                item.from == caller_name
                    && item.to == input.to.trim()
                    && item.subject == input.subject
                    && item.scope == input.scope
                    && item.action == input.action
                    && item.body == body
            }))
    }

    pub(super) async fn send_detailed_locked(
        &self,
        caller_name: &str,
        mut input: TowerSendInput,
        deduplicate: bool,
    ) -> Result<TowerInboxSent, String> {
        let state = self.load().await?;
        input.to = input.to.trim().to_string();
        input.body = input.body.trim().to_string();
        let to = input.to.as_str();
        if to != TOWER_NAME && to != BROADCAST_NAME && self.find_agent(&state, to).is_none() {
            let mut known = vec![TOWER_NAME.to_string(), BROADCAST_NAME.to_string()];
            known.extend(state.roster.agents.iter().map(|agent| agent.name.clone()));
            return Err(format!(
                "unknown recipient \"{to}\" — address a roster agent, {TOWER_NAME}, or {BROADCAST_NAME} (known: {})",
                known.join(", ")
            ));
        }
        if to == caller_name {
            return Err("cannot send an inbox message to yourself".into());
        }

        let mission_id = if caller_name == TOWER_NAME {
            self.find_agent(&state, to)
        } else {
            self.find_agent(&state, caller_name)
        }
        .and_then(|agent| agent.mission_id.or(agent.review_mission_id));
        if deduplicate
            && let Some(existing) = self.find_detailed_message(caller_name, &input).await?
        {
            return Ok(sent_from_item(existing, mission_id, false));
        }

        let sent_at = chrono::Utc::now().to_rfc3339();
        let message_id = new_message_id();
        let mut fields = vec![
            ("type", "inbox"),
            ("message_id", message_id.as_str()),
            ("from", caller_name),
            ("to", to),
            ("subject", input.subject.as_str()),
            ("sent_at", sent_at.as_str()),
        ];
        if let Some(ref scope) = input.scope {
            fields.push(("scope", scope.as_str()));
        }
        if let Some(ref action) = input.action {
            fields.push(("action", action.as_str()));
        }
        if let Some(ref consent_ref) = input.consent_ref {
            fields.push(("consent_ref", consent_ref.as_str()));
        }

        let frontmatter = render_frontmatter(&fields)?;
        let content = format!("{frontmatter}\n\n{}\n", input.body);
        let file_name = inbox_file_name(caller_name, to, &input.subject);
        let rel = format!("{INBOX_DIR}/{file_name}");
        let written = self.write_unique(&rel, &content).await?;
        let activity_log_error = self
            .append_log(
                caller_name,
                "inbox.send",
                &[("to", to), ("subject", &slugify(&input.subject, 60))],
                Some(&written),
            )
            .await
            .err()
            .map(|error| error.to_string());

        let item = TowerInboxItem {
            message_id,
            file: written.clone(),
            from: caller_name.to_string(),
            to: input.to.clone(),
            subject: input.subject.clone(),
            sent_at,
            scope: input.scope.clone(),
            action: input.action.clone(),
            consent_ref: input.consent_ref.clone(),
            body: input.body.clone(),
        };
        let mut sent = sent_from_item(item, mission_id, true);
        sent.activity_log_error = activity_log_error;
        Ok(sent)
    }

    pub(super) fn review_mission_for<'a>(
        &self,
        state: &'a TowerState,
        review: &TowerReviewInfo,
    ) -> Option<&'a crate::tools::tower::types::TowerMission> {
        review
            .mission
            .as_deref()
            .and_then(|id| state.missions.iter().find(|mission| mission.id == id))
            .or_else(|| {
                state
                    .missions
                    .iter()
                    .rev()
                    .find(|mission| mission.branch == review.target && mission.status.is_open())
            })
    }

    pub(super) fn review_recipients(
        &self,
        state: &TowerState,
        caller_name: &str,
        review: &TowerReviewInfo,
    ) -> Vec<String> {
        let mission = self.review_mission_for(state, review);
        let mut recipients = Vec::new();
        if review.status != "clean"
            && let Some(mission) = mission
            && mission.status.is_open()
            && let Some(owner) = mission.owner.as_deref()
            && owner != caller_name
            && self.find_agent(state, owner).is_some_and(|agent| {
                agent.died_at.is_none() && agent.status.as_deref() != Some("dead")
            })
        {
            recipients.push(owner.to_string());
        }
        if caller_name != TOWER_NAME {
            recipients.push(TOWER_NAME.to_string());
        }
        recipients.sort();
        recipients.dedup();
        recipients
    }

    pub(super) fn review_notification(
        &self,
        state: &TowerState,
        review: &TowerReviewInfo,
    ) -> (String, Option<String>) {
        let mission = self.review_mission_for(state, review);
        let owner = mission
            .and_then(|mission| mission.owner.as_deref())
            .unwrap_or("(none)");
        let body = [
            format!("file: {}", review.file),
            format!("target: {}", review.target),
            format!(
                "mission: {}",
                mission
                    .map(|mission| mission.id.as_str())
                    .or(review.mission.as_deref())
                    .unwrap_or("(none)")
            ),
            format!("round: {}", review.round),
            format!("verdict: {}", review.status),
            format!("merge recommendation: {}", review.merge),
            format!("reviewedCommit: {}", review.reviewed_commit),
            format!("owner: {owner}"),
        ]
        .join("\n");
        let scope = mission
            .map(|mission| mission.id.clone())
            .or_else(|| review.mission.clone());
        (body, scope)
    }

    pub(super) async fn notify_review_result_locked(
        &self,
        caller_name: &str,
        review: &TowerReviewInfo,
    ) -> (Vec<TowerInboxSent>, Vec<String>) {
        let state = match self.load().await {
            Ok(state) => state,
            Err(error) => return (Vec::new(), vec![error]),
        };
        let (body, scope) = self.review_notification(&state, review);
        let recipients = self.review_recipients(&state, caller_name, review);
        let mut notifications = Vec::new();
        let mut errors = Vec::new();
        for recipient in recipients {
            let input = TowerSendInput {
                to: recipient,
                subject: "review-result".into(),
                body: body.clone(),
                scope: scope.clone(),
                action: Some("review-result".into()),
                consent_ref: None,
            };
            match self.send_detailed_locked(caller_name, input, true).await {
                Ok(sent) => notifications.push(sent),
                Err(error) => errors.push(error),
            }
        }
        (notifications, errors)
    }

    pub async fn read_inbox_page(
        &self,
        caller_name: &str,
        limit: usize,
        include_read: bool,
    ) -> Result<TowerInboxPage, String> {
        let caller_name = caller_name.to_string();
        self.with_state_lock(move || async move {
            self.read_inbox_page_locked(&caller_name, limit, include_read)
                .await
        })
        .await
    }

    async fn read_inbox_page_locked(
        &self,
        caller_name: &str,
        limit: usize,
        include_read: bool,
    ) -> Result<TowerInboxPage, String> {
        let mut raw = self.load_state_value().await?;
        let items = self.read_inbox(caller_name, usize::MAX).await?;
        let (mut acked, cutoff) = ack_context(&raw, caller_name);
        let mut selected: Vec<TowerInboxItem> = items
            .iter()
            .filter(|item| include_read || is_unread(item, caller_name, &acked, cutoff.as_deref()))
            .take(limit.max(1))
            .cloned()
            .collect();

        let previous_count = acked.len();
        acked.extend(selected.iter().map(|item| item.message_id.clone()));
        if acked.len() != previous_count {
            let values = acked
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect::<Vec<_>>();
            if caller_name == TOWER_NAME {
                raw["inboxAckIds"] = serde_json::Value::Array(values);
            } else if let Some(agent) = raw
                .get_mut("roster")
                .and_then(|roster| roster.get_mut("agents"))
                .and_then(serde_json::Value::as_array_mut)
                .and_then(|agents| {
                    agents.iter_mut().find(|agent| {
                        agent.get("name").and_then(serde_json::Value::as_str) == Some(caller_name)
                    })
                })
            {
                agent["inboxAckIds"] = serde_json::Value::Array(values);
            }
            self.save_state_value(&raw).await?;
        }

        let unread = items
            .iter()
            .filter(|item| is_unread(item, caller_name, &acked, cutoff.as_deref()))
            .count();
        Ok(TowerInboxPage {
            items: std::mem::take(&mut selected),
            unread,
        })
    }

    pub(crate) async fn unread_inbox_count_locked(
        &self,
        caller_name: &str,
    ) -> Result<usize, String> {
        let raw = self.load_state_value().await?;
        let items = self.read_inbox(caller_name, usize::MAX).await?;
        Ok(unread_inbox_count_from_items(&raw, caller_name, &items))
    }
}
