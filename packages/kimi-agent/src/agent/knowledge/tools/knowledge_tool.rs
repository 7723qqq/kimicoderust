use std::sync::{Arc, LazyLock};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::agent::knowledge::knowledge::{
    IAgentKnowledgeService, KnowledgeAddInput, KnowledgeAddSource, KnowledgeCategory,
};
use crate::agent::tool_executor::tool_executor::{
    ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::tool_contract::ToolAccesses;

const DESCRIPTION: &str = r#"Use this tool to interact with the local knowledge base — a structured store of coding standards, pitfalls, architecture decisions, and workflow rules for the current project.

The knowledge base is automatically queried before each turn to inject relevant standards. You can also actively search, add, confirm, or reject entries.

Sub-commands (pass as the `action` parameter):
- `search`: Find entries relevant to a query, file path, or tags
- `add`: Record a new standard or pitfall discovered during work
- `confirm`: Upgrade an AI-learned entry to confirmed (confidence → 1.0)
- `reject`: Remove an incorrect or outdated entry

When to use:
- After discovering a non-obvious constraint or convention: use `add`
- When the user corrects you and the correction is a reusable rule: use `add`
- When reviewing existing entries: use `search`
- To validate or dismiss auto-learned entries: use `confirm` or `reject`

Do NOT use this tool for:
- General knowledge or facts (only project-specific standards)
- Temporary per-session preferences
- Information already in AGENTS.md (avoid duplication)"#;

static PARAMETERS: LazyLock<Value> = LazyLock::new(|| {
    to_input_json_schema(json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["search", "add", "confirm", "reject"],
                "description": "The knowledge sub-command to run."
            },
            "query": {
                "type": "string",
                "description": "Search query (FTS5 full-text)."
            },
            "scope": {
                "type": "string",
                "description": "File path scope for search."
            },
            "tags": {
                "type": "string",
                "description": "Comma-separated tags for search or add."
            },
            "id": {
                "type": "string",
                "description": "Entry id for confirm/reject."
            },
            "title": {
                "type": "string",
                "description": "Entry title for add."
            },
            "category": {
                "type": "string",
                "enum": ["coding-style", "pitfall", "architecture", "workflow"],
                "description": "Entry category for add."
            },
            "content": {
                "type": "string",
                "description": "Entry content for add."
            }
        },
        "required": ["action"],
        "additionalProperties": false
    }))
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KnowledgeAction {
    Search,
    Add,
    Confirm,
    Reject,
}

impl KnowledgeAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Add => "add",
            Self::Confirm => "confirm",
            Self::Reject => "reject",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct KnowledgeInput {
    pub action: KnowledgeAction,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub category: Option<KnowledgeCategory>,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Clone)]
pub struct KnowledgeTool {
    knowledge: Arc<dyn IAgentKnowledgeService>,
}

impl KnowledgeTool {
    pub fn new(knowledge: Arc<dyn IAgentKnowledgeService>) -> Self {
        Self { knowledge }
    }

    pub fn name(&self) -> &'static str {
        "Knowledge"
    }

    pub fn side_effect(&self) -> &'static str {
        "write"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        PARAMETERS
            .as_object()
            .cloned()
            .expect("Knowledge input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<KnowledgeInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for Knowledge: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: KnowledgeInput) -> ToolExecution {
        let description = format!("Knowledge: {}", args.action.as_str());
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: Some(ToolAccesses::none()),
            display: None,
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: self.name().to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execution(args)) })
            }),
        })
    }

    fn execution(&self, input: KnowledgeInput) -> ExecutableToolResult {
        match input.action {
            KnowledgeAction::Search => self.execute_search(input),
            KnowledgeAction::Add => self.execute_add(input),
            KnowledgeAction::Confirm => self.execute_confirm(input),
            KnowledgeAction::Reject => self.execute_reject(input),
        }
    }

    fn execute_search(&self, input: KnowledgeInput) -> ExecutableToolResult {
        let query = input.query.unwrap_or_default();
        let tags = input
            .tags
            .map(|tags| {
                tags.split(',')
                    .filter(|tag| !tag.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            });
        let results = match self.knowledge.search(
            &query,
            input.scope.as_deref(),
            tags.as_deref(),
            None,
        ) {
            Ok(results) => results,
            Err(error) => return ExecutableToolResult::error(error.to_string()),
        };
        if results.is_empty() {
            return ExecutableToolResult {
                output: "No matching knowledge entries found.".into(),
                ..Default::default()
            };
        }
        let output = results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                let first_line = result.entry.content.split('\n').next().unwrap_or_default();
                format!(
                    "{}. [{}] {} (confidence: {})\n   {}",
                    index + 1,
                    result.entry.category.as_str(),
                    result.entry.title,
                    result.entry.confidence,
                    first_line
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        ExecutableToolResult {
            output: output.into(),
            ..Default::default()
        }
    }

    fn execute_add(&self, input: KnowledgeInput) -> ExecutableToolResult {
        let title = input.title.filter(|value| !value.is_empty());
        let content = input.content.filter(|value| !value.is_empty());
        let (Some(title), Some(content), Some(category)) =
            (title, content, input.category)
        else {
            return ExecutableToolResult::error(
                "Error: title, content, and category are required for add action.",
            );
        };
        let tags = input.tags.map(|tags| {
            tags.split(',')
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
        let entry = self.knowledge.add(KnowledgeAddInput {
            title: title.clone(),
            category,
            content,
            tags,
            scope: input.scope,
            source: Some(KnowledgeAddSource::AiLearned),
            confidence: Some(0.7),
        });
        match entry {
            Some(entry) => ExecutableToolResult {
                output: format!(
                    "Learned: [{}] {} (id: {}, confidence: 0.7)",
                    entry.category.as_str(),
                    entry.title,
                    entry.id
                )
                .into(),
                ..Default::default()
            },
            None => ExecutableToolResult::error("Failed to add knowledge entry."),
        }
    }

    fn execute_confirm(&self, input: KnowledgeInput) -> ExecutableToolResult {
        let Some(id) = input.id.filter(|value| !value.is_empty()) else {
            return ExecutableToolResult::error("Error: id is required for confirm action.");
        };
        if self.knowledge.confirm(&id) {
            ExecutableToolResult {
                output: format!("Confirmed entry {id} (confidence → 1.0)").into(),
                ..Default::default()
            }
        } else {
            ExecutableToolResult::error(format!("Entry {id} not found."))
        }
    }

    fn execute_reject(&self, input: KnowledgeInput) -> ExecutableToolResult {
        let Some(id) = input.id.filter(|value| !value.is_empty()) else {
            return ExecutableToolResult::error("Error: id is required for reject action.");
        };
        if self.knowledge.remove(&id) {
            ExecutableToolResult {
                output: format!("Rejected and removed entry {id}").into(),
                ..Default::default()
            }
        } else {
            ExecutableToolResult::error(format!("Entry {id} not found."))
        }
    }
}
