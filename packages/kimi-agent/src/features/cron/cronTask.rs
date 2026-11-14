use std::sync::LazyLock;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub static CRON_ID_REGEX: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)^(?:[0-9a-f]{8}|[0-9A-HJKMNP-TV-Z]{26})$").expect("valid cron id regex")
});

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronTask {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurring: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<IndexMap<String, String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronTaskInit {
    pub cron: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurring: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<IndexMap<String, String>>,
}

impl CronTaskInit {
    pub fn into_task(self, id: String, created_at: i64) -> CronTask {
        CronTask {
            id,
            cron: self.cron,
            prompt: self.prompt,
            created_at,
            recurring: self.recurring,
            last_fired_at: self.last_fired_at,
            tags: self.tags,
        }
    }
}
