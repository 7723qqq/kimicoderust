
use serde::{Deserialize, Serialize};

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoListItem {
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecReviewDocument {
    pub name: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReviewOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolInputDisplay {
    Command {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    FileIo {
        operation: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
    },
    Diff {
        path: String,
        before: String,
        after: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hunks: Option<u32>,
    },
    Search {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    UrlFetch {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
    },
    AgentCall {
        agent_name: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background: Option<bool>,
    },

    SkillCall {
        skill_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    TodoList { items: Vec<TodoListItem> },
    Task {
        task_id: String,
        status: String,
        description: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_kind: Option<String>,
    },
    TaskStop {
        task_id: String,
        task_description: String,
    },
    PlanReview {
        plan: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<Vec<PlanReviewOption>>,
    },
    SpecReview {
        dir: String,
        documents: Vec<SpecReviewDocument>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<Vec<PlanReviewOption>>,
    },
    GoalStart {
        objective: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        completion_criterion: Option<String>,
        mode: String,
    },
    Generic {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Value>,
    },
}

impl ToolInputDisplay {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Command { .. } => "command",
            Self::FileIo { .. } => "file_io",
            Self::Diff { .. } => "diff",
            Self::Search { .. } => "search",
            Self::UrlFetch { .. } => "url_fetch",
            Self::AgentCall { .. } => "agent_call",
            Self::SkillCall { .. } => "skill_call",
            Self::TodoList { .. } => "todo_list",
            Self::Task { .. } => "task",
            Self::TaskStop { .. } => "task_stop",
            Self::PlanReview { .. } => "plan_review",
            Self::SpecReview { .. } => "spec_review",
            Self::GoalStart { .. } => "goal_start",
            Self::Generic { .. } => "generic",
        }
    }

    pub fn todo_items(&self) -> Option<&[TodoListItem]> {
        match self {
            Self::TodoList { items } => Some(items),
            _ => None,
        }
    }
}
