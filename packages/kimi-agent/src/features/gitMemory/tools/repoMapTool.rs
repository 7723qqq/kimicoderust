use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::tool_registry::tool_contribution::{
    AgentToolContribution, AgentToolContributionOptions, register_agent_tool_contribution,
};
use crate::features::gitMemory::gitMemoryService::IGitMemoryService;
use crate::features::gitMemory::types::GIT_MEMORY_TREE_DEPTH;
use crate::tool::input_schema::to_input_json_schema;
use crate::tools::registry::{ToolDefinition, ToolDisclosure, ToolSource};
use crate::turn_loop::types::ToolResourceAccess;
use crate::wire::record::WireAsyncError;

use super::repo_map::{IRepoMapTool, REPO_MAP_TOOL_NAME, RepoMapInput, repo_map_input_schema};

pub const REPO_MAP_EMPTY_OUTPUT: &str =
    "No tracked paths matched. Check the path prefix against the repository layout, or raise the depth.";

pub const DESCRIPTION: &str = "List the paths tracked by git in this repository, as recorded at HEAD.\n\nUse it to learn the shape of a repository or a subtree in one call: which packages, directories and files exist, and where they live. The output is the raw git path list, so it reflects the committed tree rather than untracked or ignored files in the working directory.\n\n- Omit `path` for the whole repository, or pass a directory or file prefix to narrow the listing. Give `path` relative to the repository root (for example `packages/agent-core-v2`); when nothing matches there, it is also tried relative to the working directory.\n- `depth` limits how deep the listing goes below `path`: depth 1 returns the direct children of `path`, depth 2 their children, and so on. The default is 3.\n- The output is capped. When it is, the last line says how many paths were left out — narrow `path` or raise `depth` to see them.\n";

pub struct RepoMapTool {
    git_memory: Arc<dyn IGitMemoryService>,
}

impl RepoMapTool {
    pub fn new(git_memory: Arc<dyn IGitMemoryService>) -> Self {
        Self { git_memory }
    }

    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: REPO_MAP_TOOL_NAME.to_string(),
            description: DESCRIPTION.to_string(),
            input_schema: self.parameters(),
            source: ToolSource::Builtin,
            disclosure: ToolDisclosure::Inline,
        }
    }
}

impl IRepoMapTool for RepoMapTool {
    fn name(&self) -> &'static str {
        REPO_MAP_TOOL_NAME
    }

    fn repo_mutation(&self) -> &'static str {
        "none"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        to_input_json_schema(repo_map_input_schema())
    }

    fn resolve_execution(&self, args: RepoMapInput) -> RepoMapToolExecution {
        RepoMapToolExecution {
            accesses: Vec::new(),
            description: "Reading the repository map".to_string(),
            approval_rule: REPO_MAP_TOOL_NAME.to_string(),
            git_memory: self.git_memory.clone(),
            path: args.path,
            depth: args.depth.unwrap_or(GIT_MEMORY_TREE_DEPTH),
        }
    }
}

pub struct RepoMapToolExecution {
    pub accesses: Vec<ToolResourceAccess>,
    pub description: String,
    pub approval_rule: String,
    git_memory: Arc<dyn IGitMemoryService>,
    path: Option<String>,
    depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMapToolOutput {
    pub output: String,
    #[serde(default, rename = "isError", skip_serializing_if = "is_false")]
    pub is_error: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl RepoMapToolExecution {
    pub async fn execute(&self) -> Result<RepoMapToolOutput, WireAsyncError> {
        let output = self
            .git_memory
            .repo_map(self.path.clone(), self.depth)
            .await;
        if output.is_empty() {
            return Ok(RepoMapToolOutput {
                output: REPO_MAP_EMPTY_OUTPUT.to_string(),
                is_error: true,
            });
        }
        Ok(RepoMapToolOutput {
            output,
            is_error: false,
        })
    }
}

static REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    register_agent_tool_contribution(AgentToolContribution::new(
        REPO_MAP_TOOL_NAME,
        Arc::new(|| ToolDefinition {
            name: REPO_MAP_TOOL_NAME.to_string(),
            description: DESCRIPTION.to_string(),
            input_schema: to_input_json_schema(repo_map_input_schema()),
            source: ToolSource::Builtin,
            disclosure: ToolDisclosure::Inline,
        }),
        AgentToolContributionOptions::new(REPO_MAP_TOOL_NAME)
            .domain("gitMemory")
            .when(Arc::new(crate::tools::memory_repo::git_memory_enabled)),
    ));
});

pub fn register_repo_map_tool() {
    LazyLock::force(&REGISTRATION);
}
