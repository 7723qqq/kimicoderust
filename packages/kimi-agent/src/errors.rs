use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use crate::_base::errors::codes::*;
pub use crate::_base::errors::error_message::*;
pub use crate::_base::errors::errors::*;
pub use crate::_base::errors::serialize::*;
pub use crate::_base::errors::unexpected_error::*;
pub use kimi_core::primitives::errors::{KimiErrorPayload, to_kimi_error_payload};

pub use crate::agent::full_compaction::errors as FullCompactionErrors;
pub use crate::agent::r#loop::errors as LoopErrors;
pub use crate::agent::profile::errors as ProfileErrors;
pub use crate::agent::prompt::errors as PromptErrors;
pub use crate::agent::task::errors as TaskErrors;
pub use crate::agent::usage::errors as UsageErrors;
pub use crate::app::auth::errors as AuthErrors;
pub use crate::app::capability::errors as CapabilityErrors;
pub use crate::app::config::errors as ConfigErrors;
pub use crate::app::event::errors as EventErrors;
pub use crate::app::file::file_service as FileErrors;
pub use crate::app::kosong_config::errors as ModelsDevImportErrors;
pub use crate::app::plugin::errors as PluginErrors;
pub use crate::app::session_export::errors as SessionExportErrors;
pub use crate::app::web::errors as WebErrors;
pub use crate::app::workspace::errors as WorkspaceErrors;
pub use crate::debug::errors as DebugErrors;
pub use crate::features::attachment::errors as AttachmentErrors;
pub use crate::features::cron::errors as CronErrors;
pub use crate::features::goal::errors as GoalErrors;
pub use crate::features::lsp::errors as LspErrors;
pub use crate::features::promptOptimizer::errors as PromptOptimizerErrors;
pub use crate::features::sessionQuery::errors as SessionQueryErrors;
pub use crate::features::skill::catalog::errors as SkillErrors;
pub use crate::features::spec::errors as SpecErrors;
pub use crate::llm_adapter::model::errors as ModelCatalogErrors;
pub use crate::llm_adapter::protocol::errors as ProtocolErrors;
pub use crate::mcp_core::errors as McpErrors;
pub use crate::os::interface::host_fs_errors as OsFsErrors;
pub use crate::os::interface::host_process as OsProcessErrors;
pub use crate::persistence::interface::storage as StorageErrors;
pub use crate::session::agentLifecycle::errors as AgentLifecycleErrors;
pub use crate::session::errors as SessionErrors;
pub use crate::state::errors as StateErrors;
pub use crate::wire::errors as WireErrors;
pub use crate::workspace::workspace_fs::internal::errors as FsErrors;

pub type KimiErrorCode = String;

pub const ERROR_CODES: &[&str] = &[
    "agent.already_exists",
    "agent.already_running",
    "agent.max_tokens_exceeded",
    "agent.no_final_message",
    "agent.not_a_subagent",
    "agent.not_found",
    "agent.not_owned",
    "agent.subagent_depth_exceeded",
    "agent.type_not_allowed",
    "attachment.image_too_large",
    "attachment.image_too_many_pixels",
    "attachment.image_type_mismatch",
    "attachment.invalid_image",
    "attachment.invalid_ref",
    "auth.login_required",
    "auth.model_not_resolved",
    "auth.provisioning_required",
    "auth.token_missing",
    "auth.token_unauthorized",
    "capability.install_in_progress",
    "capability.not_found",
    "capability.unsupported",
    "compaction.failed",
    "compaction.unable",
    "config.invalid",
    "config.persist_blocked",
    "context.overflow",
    "cron.expression_invalid",
    "debug.scope_not_found",
    "debug.token_not_found",
    "event.duplicate_event",
    "event.schema_missing",
    "file.not_found",
    "file.too_large",
    "fs.already_exists",
    "fs.git_unavailable",
    "fs.grep_timeout",
    "fs.is_binary",
    "fs.is_directory",
    "fs.path_escapes",
    "fs.path_not_found",
    "fs.permission_denied",
    "fs.too_large",
    "fs.too_many_results",
    "goal.already_exists",
    "goal.metadata_reserved",
    "goal.not_found",
    "goal.not_resumable",
    "goal.objective_empty",
    "goal.objective_too_long",
    "goal.status_invalid",
    "goal.unsupported_agent",
    "internal",
    "loop.max_steps_exceeded",
    "lsp.conflict",
    "lsp.malformed_response",
    "lsp.server_failed",
    "lsp.unavailable",
    "lsp.unsupported_operation",
    "lsp.workspace_required",
    "mcp.oauth_failed",
    "mcp.server_disabled",
    "mcp.server_not_found",
    "mcp.startup_failed",
    "mcp.tool_name_collision",
    "message.not_found",
    "model.config_invalid",
    "model.not_configured",
    "model.not_found",
    "not_implemented",
    "os.fs.already_exists",
    "os.fs.is_directory",
    "os.fs.not_directory",
    "os.fs.not_empty",
    "os.fs.not_found",
    "os.fs.permission_denied",
    "os.fs.unavailable",
    "os.fs.unknown",
    "os.process.kill_failed",
    "os.process.spawn_failed",
    "plugin.load_failed",
    "plugin.not_found",
    "profile.already_bound",
    "profile.not_bound",
    "profile.thinking_alias_conflict",
    "profile.unknown",
    "prompt.id_conflict",
    "prompt.not_found",
    "prompt_optimizer.busy",
    "prompt_optimizer.disabled",
    "prompt_optimizer.empty_draft",
    "prompt_optimizer.input_too_long",
    "prompt_optimizer.no_output",
    "prompt_optimizer.output_too_long",
    "prompt_optimizer.timeout",
    "provider.api_error",
    "provider.auth_error",
    "provider.connection_error",
    "provider.filtered",
    "provider.not_found",
    "provider.oauth_managed",
    "provider.overloaded",
    "provider.rate_limit",
    "records.write_failed",
    "request.invalid",
    "request.prompt_input_empty",
    "request.work_dir_required",
    "session.already_exists",
    "session.approval_handler_error",
    "session.busy",
    "session.closed",
    "session.export_missing_version",
    "session.export_not_found",
    "session.export_output_conflict",
    "session.export_too_large",
    "session.fork_active_turn",
    "session.id_empty",
    "session.id_invalid",
    "session.id_required",
    "session.init_failed",
    "session.model_empty",
    "session.not_found",
    "session.occupied",
    "session.permission_mode_invalid",
    "session.plan_mode_invalid",
    "session.question_handler_error",
    "session.stale",
    "session.state_invalid",
    "session.state_not_found",
    "session.thinking_empty",
    "session.title_empty",
    "session.tower_mode_invalid",
    "session.undo_unavailable",
    "session_query.index_unavailable",
    "session_query.invalid_filter",
    "session_query.session_not_found",
    "shell.git_bash_not_found",
    "skill.name_empty",
    "skill.nested_too_deep",
    "skill.not_found",
    "skill.parse_failed",
    "skill.type_unsupported",
    "spec.incomplete",
    "spec.mode_invalid",
    "spec.write_denied",
    "state.cycle",
    "state.duplicate_fold",
    "state.durability_mismatch",
    "storage.corrupted",
    "storage.decode_failed",
    "storage.disk_full",
    "storage.io_failed",
    "storage.locked",
    "storage.not_found",
    "storage.permission_denied",
    "task.limit_exceeded",
    "task.task_id_empty",
    "turn.agent_busy",
    "usage.turn_id_conflict",
    "validation.failed",
    "web.fetch_failed",
    "web.invalid_url",
    "web.private_address",
    "wire.cycle",
    "wire.duplicate_op",
    "wire.migration_missing",
    "wire.unknown_record",
    "wire.version_too_low",
    "modelsDev.catalog_entry_not_found",
    "modelsDev.catalog_unavailable",
    "modelsDev.import_invalid",
    "modelsDev.registry_import_invalid",
    "workspace.not_found",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub code: KimiErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Map<String, Value>>,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<KimiErrorPayload>,
}

impl ErrorEvent {
    pub fn new(code: KimiErrorCode, message: String, retryable: bool) -> Self {
        Self {
            type_: "error".to_string(),
            code,
            message,
            name: None,
            details: None,
            retryable,
            cause: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarningEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl WarningEvent {
    pub fn new(message: String, code: Option<String>) -> Self {
        Self {
            type_: "warning".to_string(),
            message,
            code,
        }
    }
}
