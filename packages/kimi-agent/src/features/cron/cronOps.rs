use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::features::cron::cron_task::CronTask;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronForkNotice {
    pub reminder_pending: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronModelState {
    pub tasks: IndexMap<String, CronTask>,
    pub fork_notice: CronForkNotice,
}

pub struct CronAdd;
pub struct CronDelete;
pub struct CronCursor;
pub struct CronFired;

impl CronAdd {
    pub const EVENT_TYPE: &'static str = "cron.add";
}

impl CronDelete {
    pub const EVENT_TYPE: &'static str = "cron.delete";
}

impl CronCursor {
    pub const EVENT_TYPE: &'static str = "cron.cursor";
}

impl CronFired {
    pub const EVENT_TYPE: &'static str = "cron.fired";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronFiredEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub origin: crate::agent::context_memory::types::CronJobOrigin,
    pub prompt: String,
}

pub fn cron_add_event(task: &CronTask) -> Value {
    json!({"type": CronAdd::EVENT_TYPE, "task": task})
}

pub fn cron_delete_event(ids: &[String]) -> Value {
    json!({"type": CronDelete::EVENT_TYPE, "ids": ids})
}

pub fn cron_cursor_event(id: &str, last_fired_at: i64) -> Value {
    json!({"type": CronCursor::EVENT_TYPE, "id": id, "lastFiredAt": last_fired_at})
}

pub fn cron_fired_event(
    origin: &crate::agent::context_memory::types::CronJobOrigin,
    prompt: &str,
) -> Value {
    json!({"type": CronFired::EVENT_TYPE, "origin": origin, "prompt": prompt})
}
