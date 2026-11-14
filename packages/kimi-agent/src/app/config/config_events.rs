use std::sync::Arc;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::event::event2::{Event2, Event2Class};

pub const CONFIG_WARNING_TYPE: &str = "event.config.warning";
pub const CONFIG_CHANGED_TYPE: &str = "event.config.changed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigWarningItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigWarningPayload {
    pub warnings: Vec<ConfigWarningItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigWarning {
    pub payload: ConfigWarningPayload,
    pub time: i64,
}

impl ConfigWarning {
    pub const TYPE: &'static str = CONFIG_WARNING_TYPE;

    pub fn new(payload: ConfigWarningPayload) -> Self {
        Self::with_time(payload, None)
    }

    pub fn with_time(payload: ConfigWarningPayload, time: Option<i64>) -> Self {
        Self {
            payload,
            time: time.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        }
    }

    pub fn class() -> Arc<Event2Class> {
        Arc::new(Event2Class {
            type_: CONFIG_WARNING_TYPE.to_string(),
            durable: false,
            observable: false,
            agent_domain: false,
            schema: None,
        })
    }

    pub fn to_event2(&self) -> Event2 {
        Event2 {
            type_: CONFIG_WARNING_TYPE.to_string(),
            time: self.time,
            payload: json!({"payload": self.payload}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChangedPayload {
    pub changed_fields: Vec<String>,
    pub config: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigChanged {
    pub payload: ConfigChangedPayload,
    pub time: i64,
}

impl ConfigChanged {
    pub const TYPE: &'static str = CONFIG_CHANGED_TYPE;

    pub fn new(payload: ConfigChangedPayload) -> Self {
        Self::with_time(payload, None)
    }

    pub fn with_time(payload: ConfigChangedPayload, time: Option<i64>) -> Self {
        Self {
            payload,
            time: time.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        }
    }

    pub fn class() -> Arc<Event2Class> {
        Arc::new(Event2Class {
            type_: CONFIG_CHANGED_TYPE.to_string(),
            durable: false,
            observable: false,
            agent_domain: false,
            schema: None,
        })
    }

    pub fn to_event2(&self) -> Event2 {
        Event2 {
            type_: CONFIG_CHANGED_TYPE.to_string(),
            time: self.time,
            payload: json!({"payload": self.payload}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfigResponse {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    pub has_api_key: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigResponse {
    pub providers: IndexMap<String, ProviderConfigResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_plan_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_all_available_skills: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_skill_dirs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_control: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<IndexMap<String, bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for ConfigResponse {
    fn default() -> Self {
        Self {
            providers: IndexMap::new(),
            default_provider: None,
            default_model: None,
            models: None,
            thinking: None,
            plan_mode: None,
            yolo: None,
            default_permission_mode: None,
            default_plan_mode: None,
            permission: None,
            hooks: None,
            services: None,
            merge_all_available_skills: None,
            extra_skill_dirs: None,
            loop_control: None,
            background: None,
            experimental: None,
            telemetry: None,
            raw: None,
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChangedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub changed_fields: Vec<String>,
    pub config: ConfigResponse,
}

impl Default for ConfigChangedEvent {
    fn default() -> Self {
        Self {
            type_: CONFIG_CHANGED_TYPE.to_string(),
            changed_fields: Vec::new(),
            config: ConfigResponse::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigWarningEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub warnings: Vec<ConfigWarningItem>,
}

impl Default for ConfigWarningEvent {
    fn default() -> Self {
        Self {
            type_: CONFIG_WARNING_TYPE.to_string(),
            warnings: Vec::new(),
        }
    }
}
