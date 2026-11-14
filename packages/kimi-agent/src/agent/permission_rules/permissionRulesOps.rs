use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::_base::state::state_registry::StateKey;
use crate::agent::interaction::approval::ApprovalResponse;
use crate::app::event::event2::{Event2, Event2Class};
use crate::state::state::{ReplayableOptions, ReplayableStateKey};

use super::permission_rules::PermissionRule;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRulesModelState {
    pub rules: Vec<PermissionRule>,
    pub session_approval_rule_patterns: Vec<String>,
}

pub const PERMISSION_RULES_ADD_EVENT: &str = "permission.rules.add";

pub static PERMISSION_RULES_ADD_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: PERMISSION_RULES_ADD_EVENT.to_string(),
        durable: false,
        observable: false,
        agent_domain: true,
        schema: None,
    })
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRulesAdd {
    pub agent_id: String,
    pub rules: Vec<PermissionRule>,
}

impl PermissionRulesAdd {
    pub const TYPE: &'static str = PERMISSION_RULES_ADD_EVENT;

    pub fn class() -> Arc<Event2Class> {
        PERMISSION_RULES_ADD_CLASS.clone()
    }

    pub fn to_event2(&self) -> Event2 {
        Event2::new(
            &PERMISSION_RULES_ADD_CLASS,
            serde_json::to_value(self).expect("permission rules event is serializable"),
            None,
        )
    }
}

impl From<PermissionRulesAdd> for Event2 {
    fn from(event: PermissionRulesAdd) -> Self {
        event.to_event2()
    }
}

pub const PERMISSION_RECORD_APPROVAL_RESULT_EVENT: &str = "permission.record_approval_result";

pub static PERMISSION_RECORD_APPROVAL_RESULT_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: PERMISSION_RECORD_APPROVAL_RESULT_EVENT.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "turnId", "toolCallId", "toolName", "action", "result"],
            "properties": {
                "agentId": {"type": "string"},
                "turnId": {"type": "number"},
                "toolCallId": {"type": "string"},
                "toolName": {"type": "string"},
                "action": {"type": "string"},
                "sessionApprovalRule": {"type": "string"},
                "result": {},
            },
        })),
    })
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRecordApprovalResult {
    pub agent_id: String,
    pub turn_id: u64,
    pub tool_call_id: String,
    pub tool_name: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_approval_rule: Option<String>,
    pub result: ApprovalResponse,
}

impl PermissionRecordApprovalResult {
    pub const TYPE: &'static str = PERMISSION_RECORD_APPROVAL_RESULT_EVENT;

    pub fn class() -> Arc<Event2Class> {
        PERMISSION_RECORD_APPROVAL_RESULT_CLASS.clone()
    }

    pub fn to_event2(&self) -> Event2 {
        Event2::new(
            &PERMISSION_RECORD_APPROVAL_RESULT_CLASS,
            serde_json::to_value(self).expect("permission approval event is serializable"),
            None,
        )
    }
}

impl From<PermissionRecordApprovalResult> for Event2 {
    fn from(event: PermissionRecordApprovalResult) -> Self {
        event.to_event2()
    }
}

pub fn permission_rules_key() -> StateKey<PermissionRulesModelState> {
    StateKey::new("permissionRules", PermissionRulesModelState::default)
}

pub static PERMISSION_RULES_REPLAYABLE_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        let mut key = ReplayableStateKey::new(
            "permissionRules".to_string(),
            Arc::new(|| {
                serde_json::to_value(PermissionRulesModelState::default())
                    .expect("permission rules state is serializable")
            }),
            ReplayableOptions {
                schema: json!({}),
                durable: None,
                blobs: None,
                mutable_replay: None,
            },
        );
        key.on(&PERMISSION_RULES_ADD_CLASS, Arc::new(|state, event, _ctx| {
            let mut model = serde_json::from_value::<PermissionRulesModelState>(state).ok()?;
            let rules = serde_json::from_value::<Vec<PermissionRule>>(
                event.payload.get("rules")?.clone(),
            )
            .ok()?;
            if rules.is_empty() {
                return None;
            }
            model.rules.extend(rules);
            serde_json::to_value(model).ok()
        }))
        .expect("permission rules add fold can be registered");
        key.on(&PERMISSION_RECORD_APPROVAL_RESULT_CLASS, Arc::new(|state, event, _ctx| {
            let result = event.payload.get("result")?;
            if result.get("decision")?.as_str()? != "approved"
                || result.get("scope").and_then(Value::as_str) != Some("session")
            {
                return None;
            }
            let Some(pattern) = event
                .payload
                .get("sessionApprovalRule")
                .and_then(Value::as_str)
            else {
                return None;
            };
            let mut model = serde_json::from_value::<PermissionRulesModelState>(state).ok()?;
            if model.session_approval_rule_patterns.iter().any(|item| item == pattern) {
                return None;
            }
            model.session_approval_rule_patterns.push(pattern.to_string());
            serde_json::to_value(model).ok()
        }))
        .expect("permission approval fold can be registered");
        Arc::new(key)
    });

pub fn permission_rules_replayable_key() -> Arc<ReplayableStateKey> {
    PERMISSION_RULES_REPLAYABLE_KEY.clone()
}
