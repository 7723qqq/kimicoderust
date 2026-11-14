use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileIoOperation {
    Read,
    Write,
    Edit,
    Glob,
    Grep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandLanguage {
    Bash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TodoItemKind {
    Milestone,
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalStartMode {
    Manual,
    Yolo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoListItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TodoItemKind>,
    pub title: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReviewOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecReviewDocument {
    pub name: String,
    pub content: String,
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
        language: Option<CommandLanguage>,
    },
    FileIo {
        operation: FileIoOperation,
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
    TodoList {
        items: Vec<TodoListItem>,
    },
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
        #[serde(rename = "completionCriterion", default, skip_serializing_if = "Option::is_none")]
        completion_criterion: Option<String>,
        mode: GoalStartMode,
    },
    Generic {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Value>,
    },
}
