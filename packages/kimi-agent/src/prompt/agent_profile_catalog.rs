use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProfileContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_listing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_dirs_info: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub js_runtimes: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_sections: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_adaptation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_style_guide: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_user_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_list_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode_available: Option<bool>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentDisclosureSnapshot {
    pub cwd: String,
}
