
use super::codes::ErrorDomain;

pub mod core {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const INTERNAL: &str = "internal";
    pub const NOT_IMPLEMENTED: &str = "not_implemented";
    pub const VALIDATION_FAILED: &str = "validation.failed";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "CoreErrors",
        codes: &[
            ("INTERNAL", INTERNAL),
            ("NOT_IMPLEMENTED", NOT_IMPLEMENTED),
            ("VALIDATION_FAILED", VALIDATION_FAILED),
        ],
        retryable: &[],
        info: &[
            ("internal", ErrorInfo::new(false, true)),
            ("not_implemented", ErrorInfo::new(false, true)),
        ],
    };
}

pub mod attachment {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const ATTACHMENT_INVALID_IMAGE: &str = "attachment.invalid_image";
    pub const ATTACHMENT_IMAGE_TYPE_MISMATCH: &str = "attachment.image_type_mismatch";
    pub const ATTACHMENT_IMAGE_TOO_LARGE: &str = "attachment.image_too_large";
    pub const ATTACHMENT_IMAGE_TOO_MANY_PIXELS: &str = "attachment.image_too_many_pixels";
    pub const ATTACHMENT_INVALID_REF: &str = "attachment.invalid_ref";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "AttachmentErrors",
        codes: &[
            ("ATTACHMENT_INVALID_IMAGE", ATTACHMENT_INVALID_IMAGE),
            (
                "ATTACHMENT_IMAGE_TYPE_MISMATCH",
                ATTACHMENT_IMAGE_TYPE_MISMATCH,
            ),
            ("ATTACHMENT_IMAGE_TOO_LARGE", ATTACHMENT_IMAGE_TOO_LARGE),
            (
                "ATTACHMENT_IMAGE_TOO_MANY_PIXELS",
                ATTACHMENT_IMAGE_TOO_MANY_PIXELS,
            ),
            ("ATTACHMENT_INVALID_REF", ATTACHMENT_INVALID_REF),
        ],
        retryable: &[],
        info: &[
            ("attachment.invalid_image", ErrorInfo::new(false, true)),
            (
                "attachment.image_type_mismatch",
                ErrorInfo::new(false, true),
            ),
            ("attachment.image_too_large", ErrorInfo::new(false, true)),
            (
                "attachment.image_too_many_pixels",
                ErrorInfo::new(false, true),
            ),
            ("attachment.invalid_ref", ErrorInfo::new(false, true)),
        ],
    };
}

pub mod lsp {
    use super::super::codes::ErrorDomain;

    pub const LSP_UNAVAILABLE: &str = "lsp.unavailable";
    pub const LSP_CONFLICT: &str = "lsp.conflict";
    pub const LSP_UNSUPPORTED_OPERATION: &str = "lsp.unsupported_operation";
    pub const LSP_MALFORMED_RESPONSE: &str = "lsp.malformed_response";
    pub const LSP_WORKSPACE_REQUIRED: &str = "lsp.workspace_required";
    pub const LSP_SERVER_FAILED: &str = "lsp.server_failed";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "LspErrors",
        codes: &[
            ("LSP_UNAVAILABLE", LSP_UNAVAILABLE),
            ("LSP_CONFLICT", LSP_CONFLICT),
            ("LSP_UNSUPPORTED_OPERATION", LSP_UNSUPPORTED_OPERATION),
            ("LSP_MALFORMED_RESPONSE", LSP_MALFORMED_RESPONSE),
            ("LSP_WORKSPACE_REQUIRED", LSP_WORKSPACE_REQUIRED),
            ("LSP_SERVER_FAILED", LSP_SERVER_FAILED),
        ],
        retryable: &["lsp.server_failed"],
        info: &[],
    };
}

pub mod session_query {
    use super::super::codes::ErrorDomain;

    pub const SESSION_QUERY_INVALID_FILTER: &str = "session_query.invalid_filter";
    pub const SESSION_QUERY_SESSION_NOT_FOUND: &str = "session_query.session_not_found";
    pub const SESSION_QUERY_INDEX_UNAVAILABLE: &str = "session_query.index_unavailable";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "SessionQueryErrors",
        codes: &[
            ("SESSION_QUERY_INVALID_FILTER", SESSION_QUERY_INVALID_FILTER),
            (
                "SESSION_QUERY_SESSION_NOT_FOUND",
                SESSION_QUERY_SESSION_NOT_FOUND,
            ),
            (
                "SESSION_QUERY_INDEX_UNAVAILABLE",
                SESSION_QUERY_INDEX_UNAVAILABLE,
            ),
        ],
        retryable: &["session_query.index_unavailable"],
        info: &[],
    };
}

pub mod prompt_optimizer {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const PROMPT_OPTIMIZER_DISABLED: &str = "prompt_optimizer.disabled";
    pub const PROMPT_OPTIMIZER_EMPTY_DRAFT: &str = "prompt_optimizer.empty_draft";
    pub const PROMPT_OPTIMIZER_INPUT_TOO_LONG: &str = "prompt_optimizer.input_too_long";
    pub const PROMPT_OPTIMIZER_NO_OUTPUT: &str = "prompt_optimizer.no_output";
    pub const PROMPT_OPTIMIZER_BUSY: &str = "prompt_optimizer.busy";
    pub const PROMPT_OPTIMIZER_OUTPUT_TOO_LONG: &str = "prompt_optimizer.output_too_long";
    pub const PROMPT_OPTIMIZER_TIMEOUT: &str = "prompt_optimizer.timeout";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "PromptOptimizerErrors",
        codes: &[
            ("PROMPT_OPTIMIZER_DISABLED", PROMPT_OPTIMIZER_DISABLED),
            ("PROMPT_OPTIMIZER_EMPTY_DRAFT", PROMPT_OPTIMIZER_EMPTY_DRAFT),
            (
                "PROMPT_OPTIMIZER_INPUT_TOO_LONG",
                PROMPT_OPTIMIZER_INPUT_TOO_LONG,
            ),
            ("PROMPT_OPTIMIZER_NO_OUTPUT", PROMPT_OPTIMIZER_NO_OUTPUT),
            ("PROMPT_OPTIMIZER_BUSY", PROMPT_OPTIMIZER_BUSY),
            (
                "PROMPT_OPTIMIZER_OUTPUT_TOO_LONG",
                PROMPT_OPTIMIZER_OUTPUT_TOO_LONG,
            ),
            ("PROMPT_OPTIMIZER_TIMEOUT", PROMPT_OPTIMIZER_TIMEOUT),
        ],
        retryable: &["prompt_optimizer.busy", "prompt_optimizer.timeout"],
        info: &[
            ("prompt_optimizer.disabled", ErrorInfo::new(false, true)),
            ("prompt_optimizer.empty_draft", ErrorInfo::new(false, true)),
            (
                "prompt_optimizer.input_too_long",
                ErrorInfo::new(false, true),
            ),
            ("prompt_optimizer.no_output", ErrorInfo::new(true, true)),
            ("prompt_optimizer.busy", ErrorInfo::new(true, true)),
            (
                "prompt_optimizer.output_too_long",
                ErrorInfo::new(false, true),
            ),
            ("prompt_optimizer.timeout", ErrorInfo::new(true, true)),
        ],
    };
}

pub mod spec {
    use super::super::codes::ErrorDomain;

    pub const SPEC_MODE_INVALID: &str = "spec.mode_invalid";
    pub const SPEC_INCOMPLETE: &str = "spec.incomplete";
    pub const SPEC_WRITE_DENIED: &str = "spec.write_denied";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "SpecErrors",
        codes: &[
            ("SPEC_MODE_INVALID", SPEC_MODE_INVALID),
            ("SPEC_INCOMPLETE", SPEC_INCOMPLETE),
            ("SPEC_WRITE_DENIED", SPEC_WRITE_DENIED),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod agent_lifecycle {
    use super::super::codes::ErrorDomain;

    pub const AGENT_NOT_FOUND: &str = "agent.not_found";
    pub const AGENT_ALREADY_EXISTS: &str = "agent.already_exists";
    pub const AGENT_ALREADY_RUNNING: &str = "agent.already_running";
    pub const AGENT_NOT_A_SUBAGENT: &str = "agent.not_a_subagent";
    pub const AGENT_NOT_OWNED: &str = "agent.not_owned";
    pub const AGENT_TYPE_NOT_ALLOWED: &str = "agent.type_not_allowed";
    pub const AGENT_MAX_TOKENS_EXCEEDED: &str = "agent.max_tokens_exceeded";
    pub const SUBAGENT_DEPTH_EXCEEDED: &str = "agent.subagent_depth_exceeded";
    pub const AGENT_NO_FINAL_MESSAGE: &str = "agent.no_final_message";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "AgentLifecycleErrors",
        codes: &[
            ("AGENT_NOT_FOUND", AGENT_NOT_FOUND),
            ("AGENT_ALREADY_EXISTS", AGENT_ALREADY_EXISTS),
            ("AGENT_ALREADY_RUNNING", AGENT_ALREADY_RUNNING),
            ("AGENT_NOT_A_SUBAGENT", AGENT_NOT_A_SUBAGENT),
            ("AGENT_NOT_OWNED", AGENT_NOT_OWNED),
            ("AGENT_TYPE_NOT_ALLOWED", AGENT_TYPE_NOT_ALLOWED),
            ("AGENT_MAX_TOKENS_EXCEEDED", AGENT_MAX_TOKENS_EXCEEDED),
            ("SUBAGENT_DEPTH_EXCEEDED", SUBAGENT_DEPTH_EXCEEDED),
            ("AGENT_NO_FINAL_MESSAGE", AGENT_NO_FINAL_MESSAGE),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod auth {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const AUTH_LOGIN_REQUIRED: &str = "auth.login_required";
    pub const AUTH_PROVISIONING_REQUIRED: &str = "auth.provisioning_required";
    pub const AUTH_TOKEN_MISSING: &str = "auth.token_missing";
    pub const AUTH_TOKEN_UNAUTHORIZED: &str = "auth.token_unauthorized";
    pub const AUTH_MODEL_NOT_RESOLVED: &str = "auth.model_not_resolved";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "AuthErrors",
        codes: &[
            ("AUTH_LOGIN_REQUIRED", AUTH_LOGIN_REQUIRED),
            ("AUTH_PROVISIONING_REQUIRED", AUTH_PROVISIONING_REQUIRED),
            ("AUTH_TOKEN_MISSING", AUTH_TOKEN_MISSING),
            ("AUTH_TOKEN_UNAUTHORIZED", AUTH_TOKEN_UNAUTHORIZED),
            ("AUTH_MODEL_NOT_RESOLVED", AUTH_MODEL_NOT_RESOLVED),
        ],
        retryable: &[],
        info: &[
            ("auth.login_required", ErrorInfo::new(false, true)),
            ("auth.provisioning_required", ErrorInfo::new(false, true)),
            ("auth.token_missing", ErrorInfo::new(false, true)),
            ("auth.token_unauthorized", ErrorInfo::new(false, true)),
            ("auth.model_not_resolved", ErrorInfo::new(false, true)),
        ],
    };
}

pub mod task {
    use super::super::codes::ErrorDomain;

    pub const TASK_ID_EMPTY: &str = "task.task_id_empty";
    pub const TASK_LIMIT_EXCEEDED: &str = "task.limit_exceeded";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "TaskErrors",
        codes: &[
            ("TASK_ID_EMPTY", TASK_ID_EMPTY),
            ("TASK_LIMIT_EXCEEDED", TASK_LIMIT_EXCEEDED),
        ],
        retryable: &["task.limit_exceeded"],
        info: &[],
    };
}

pub mod protocol {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const PROVIDER_API_ERROR: &str = "provider.api_error";
    pub const PROVIDER_FILTERED: &str = "provider.filtered";
    pub const PROVIDER_RATE_LIMIT: &str = "provider.rate_limit";
    pub const PROVIDER_AUTH_ERROR: &str = "provider.auth_error";
    pub const PROVIDER_CONNECTION_ERROR: &str = "provider.connection_error";
    pub const PROVIDER_OVERLOADED: &str = "provider.overloaded";
    pub const CONTEXT_OVERFLOW: &str = "context.overflow";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "ProtocolErrors",
        codes: &[
            ("PROVIDER_API_ERROR", PROVIDER_API_ERROR),
            ("PROVIDER_FILTERED", PROVIDER_FILTERED),
            ("PROVIDER_RATE_LIMIT", PROVIDER_RATE_LIMIT),
            ("PROVIDER_AUTH_ERROR", PROVIDER_AUTH_ERROR),
            ("PROVIDER_CONNECTION_ERROR", PROVIDER_CONNECTION_ERROR),
            ("PROVIDER_OVERLOADED", PROVIDER_OVERLOADED),
            ("CONTEXT_OVERFLOW", CONTEXT_OVERFLOW),
        ],
        retryable: &[
            "provider.rate_limit",
            "provider.connection_error",
            "provider.overloaded",
            "context.overflow",
        ],
        info: &[
            ("provider.rate_limit", ErrorInfo::new(true, true)),
            ("provider.filtered", ErrorInfo::new(false, true)),
            ("provider.auth_error", ErrorInfo::new(false, true)),
            ("provider.overloaded", ErrorInfo::new(true, true)),
            ("context.overflow", ErrorInfo::new(true, true)),
        ],
    };
}

pub mod config {
    use super::super::codes::ErrorDomain;

    pub const CONFIG_INVALID: &str = "config.invalid";
    pub const CONFIG_PERSIST_BLOCKED: &str = "config.persist_blocked";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "ConfigErrors",
        codes: &[
            ("CONFIG_INVALID", CONFIG_INVALID),
            ("CONFIG_PERSIST_BLOCKED", CONFIG_PERSIST_BLOCKED),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod capability {
    use super::super::codes::ErrorDomain;

    pub const CAPABILITY_NOT_FOUND: &str = "capability.not_found";
    pub const CAPABILITY_UNSUPPORTED: &str = "capability.unsupported";
    pub const CAPABILITY_INSTALL_IN_PROGRESS: &str = "capability.install_in_progress";
    pub const CAPABILITY_PROCESSES_RUNNING: &str = "capability.processes_running";
    pub const CAPABILITY_PROCESS_CONTROL_FAILED: &str = "capability.process_control_failed";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "CapabilityErrors",
        codes: &[
            ("CAPABILITY_NOT_FOUND", CAPABILITY_NOT_FOUND),
            ("CAPABILITY_UNSUPPORTED", CAPABILITY_UNSUPPORTED),
            (
                "CAPABILITY_INSTALL_IN_PROGRESS",
                CAPABILITY_INSTALL_IN_PROGRESS,
            ),
            ("CAPABILITY_PROCESSES_RUNNING", CAPABILITY_PROCESSES_RUNNING),
            (
                "CAPABILITY_PROCESS_CONTROL_FAILED",
                CAPABILITY_PROCESS_CONTROL_FAILED,
            ),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod cron {
    use super::super::codes::ErrorDomain;

    pub const CRON_EXPRESSION_INVALID: &str = "cron.expression_invalid";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "CronErrors",
        codes: &[("CRON_EXPRESSION_INVALID", CRON_EXPRESSION_INVALID)],
        retryable: &[],
        info: &[],
    };
}

pub mod debug {
    use super::super::codes::ErrorDomain;

    pub const DEBUG_SCOPE_NOT_FOUND: &str = "debug.scope_not_found";
    pub const DEBUG_TOKEN_NOT_FOUND: &str = "debug.token_not_found";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "DebugErrors",
        codes: &[
            ("DEBUG_SCOPE_NOT_FOUND", DEBUG_SCOPE_NOT_FOUND),
            ("DEBUG_TOKEN_NOT_FOUND", DEBUG_TOKEN_NOT_FOUND),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod file {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const FILE_NOT_FOUND: &str = "file.not_found";
    pub const FILE_TOO_LARGE: &str = "file.too_large";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "FileErrors",
        codes: &[
            ("FILE_NOT_FOUND", FILE_NOT_FOUND),
            ("FILE_TOO_LARGE", FILE_TOO_LARGE),
        ],
        retryable: &[],
        info: &[
            ("file.not_found", ErrorInfo::new(false, true)),
            ("file.too_large", ErrorInfo::new(false, true)),
        ],
    };
}

pub mod fs {
    use super::super::codes::ErrorDomain;

    pub const FS_PATH_NOT_FOUND: &str = "fs.path_not_found";
    pub const FS_PERMISSION_DENIED: &str = "fs.permission_denied";
    pub const FS_PATH_ESCAPES: &str = "fs.path_escapes";
    pub const FS_IS_DIRECTORY: &str = "fs.is_directory";
    pub const FS_IS_BINARY: &str = "fs.is_binary";
    pub const FS_TOO_LARGE: &str = "fs.too_large";
    pub const FS_ALREADY_EXISTS: &str = "fs.already_exists";
    pub const FS_TOO_MANY_RESULTS: &str = "fs.too_many_results";
    pub const FS_GREP_TIMEOUT: &str = "fs.grep_timeout";
    pub const FS_GIT_UNAVAILABLE: &str = "fs.git_unavailable";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "FsErrors",
        codes: &[
            ("FS_PATH_NOT_FOUND", FS_PATH_NOT_FOUND),
            ("FS_PERMISSION_DENIED", FS_PERMISSION_DENIED),
            ("FS_PATH_ESCAPES", FS_PATH_ESCAPES),
            ("FS_IS_DIRECTORY", FS_IS_DIRECTORY),
            ("FS_IS_BINARY", FS_IS_BINARY),
            ("FS_TOO_LARGE", FS_TOO_LARGE),
            ("FS_ALREADY_EXISTS", FS_ALREADY_EXISTS),
            ("FS_TOO_MANY_RESULTS", FS_TOO_MANY_RESULTS),
            ("FS_GREP_TIMEOUT", FS_GREP_TIMEOUT),
            ("FS_GIT_UNAVAILABLE", FS_GIT_UNAVAILABLE),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod full_compaction {
    use super::super::codes::ErrorDomain;

    pub const COMPACTION_FAILED: &str = "compaction.failed";
    pub const COMPACTION_UNABLE: &str = "compaction.unable";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "FullCompactionErrors",
        codes: &[
            ("COMPACTION_FAILED", COMPACTION_FAILED),
            ("COMPACTION_UNABLE", COMPACTION_UNABLE),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod goal {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const GOAL_ALREADY_EXISTS: &str = "goal.already_exists";
    pub const GOAL_NOT_FOUND: &str = "goal.not_found";
    pub const GOAL_OBJECTIVE_EMPTY: &str = "goal.objective_empty";
    pub const GOAL_OBJECTIVE_TOO_LONG: &str = "goal.objective_too_long";
    pub const GOAL_STATUS_INVALID: &str = "goal.status_invalid";
    pub const GOAL_METADATA_RESERVED: &str = "goal.metadata_reserved";
    pub const GOAL_NOT_RESUMABLE: &str = "goal.not_resumable";
    pub const GOAL_UNSUPPORTED_AGENT: &str = "goal.unsupported_agent";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "GoalErrors",
        codes: &[
            ("GOAL_ALREADY_EXISTS", GOAL_ALREADY_EXISTS),
            ("GOAL_NOT_FOUND", GOAL_NOT_FOUND),
            ("GOAL_OBJECTIVE_EMPTY", GOAL_OBJECTIVE_EMPTY),
            ("GOAL_OBJECTIVE_TOO_LONG", GOAL_OBJECTIVE_TOO_LONG),
            ("GOAL_STATUS_INVALID", GOAL_STATUS_INVALID),
            ("GOAL_METADATA_RESERVED", GOAL_METADATA_RESERVED),
            ("GOAL_NOT_RESUMABLE", GOAL_NOT_RESUMABLE),
            ("GOAL_UNSUPPORTED_AGENT", GOAL_UNSUPPORTED_AGENT),
        ],
        retryable: &[],
        info: &[
            ("goal.already_exists", ErrorInfo::new(false, true)),
            ("goal.not_found", ErrorInfo::new(false, true)),
            ("goal.objective_empty", ErrorInfo::new(false, true)),
            ("goal.objective_too_long", ErrorInfo::new(false, true)),
            ("goal.status_invalid", ErrorInfo::new(false, true)),
            ("goal.metadata_reserved", ErrorInfo::new(false, true)),
            ("goal.not_resumable", ErrorInfo::new(false, true)),
            ("goal.unsupported_agent", ErrorInfo::new(false, true)),
        ],
    };
}

pub mod agent_loop {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const LOOP_MAX_STEPS_EXCEEDED: &str = "loop.max_steps_exceeded";
    pub const TURN_AGENT_BUSY: &str = "turn.agent_busy";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "LoopErrors",
        codes: &[
            ("LOOP_MAX_STEPS_EXCEEDED", LOOP_MAX_STEPS_EXCEEDED),
            ("TURN_AGENT_BUSY", TURN_AGENT_BUSY),
        ],
        retryable: &["turn.agent_busy"],
        info: &[("loop.max_steps_exceeded", ErrorInfo::new(false, true))],
    };
}

pub mod mcp {
    use super::super::codes::ErrorDomain;

    pub const MCP_SERVER_NOT_FOUND: &str = "mcp.server_not_found";
    pub const MCP_SERVER_DISABLED: &str = "mcp.server_disabled";
    pub const MCP_STARTUP_FAILED: &str = "mcp.startup_failed";
    pub const MCP_TOOL_NAME_COLLISION: &str = "mcp.tool_name_collision";
    pub const MCP_OAUTH_FAILED: &str = "mcp.oauth_failed";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "McpErrors",
        codes: &[
            ("MCP_SERVER_NOT_FOUND", MCP_SERVER_NOT_FOUND),
            ("MCP_SERVER_DISABLED", MCP_SERVER_DISABLED),
            ("MCP_STARTUP_FAILED", MCP_STARTUP_FAILED),
            ("MCP_TOOL_NAME_COLLISION", MCP_TOOL_NAME_COLLISION),
            ("MCP_OAUTH_FAILED", MCP_OAUTH_FAILED),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod model_catalog {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const PROVIDER_NOT_FOUND: &str = "provider.not_found";
    pub const MODEL_NOT_FOUND: &str = "model.not_found";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "ModelCatalogErrors",
        codes: &[
            ("PROVIDER_NOT_FOUND", PROVIDER_NOT_FOUND),
            ("MODEL_NOT_FOUND", MODEL_NOT_FOUND),
        ],
        retryable: &[],
        info: &[
            ("provider.not_found", ErrorInfo::new(false, true)),
            ("model.not_found", ErrorInfo::new(false, true)),
        ],
    };
}

pub mod os_fs {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const OS_FS_NOT_FOUND: &str = "os.fs.not_found";
    pub const OS_FS_IS_DIRECTORY: &str = "os.fs.is_directory";
    pub const OS_FS_NOT_DIRECTORY: &str = "os.fs.not_directory";
    pub const OS_FS_ALREADY_EXISTS: &str = "os.fs.already_exists";
    pub const OS_FS_PERMISSION_DENIED: &str = "os.fs.permission_denied";
    pub const OS_FS_NOT_EMPTY: &str = "os.fs.not_empty";
    pub const OS_FS_DIRECTORY_TOO_LARGE: &str = "os.fs.directory_too_large";
    pub const OS_FS_UNAVAILABLE: &str = "os.fs.unavailable";
    pub const OS_FS_UNKNOWN: &str = "os.fs.unknown";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "OsFsErrors",
        codes: &[
            ("OS_FS_NOT_FOUND", OS_FS_NOT_FOUND),
            ("OS_FS_IS_DIRECTORY", OS_FS_IS_DIRECTORY),
            ("OS_FS_NOT_DIRECTORY", OS_FS_NOT_DIRECTORY),
            ("OS_FS_ALREADY_EXISTS", OS_FS_ALREADY_EXISTS),
            ("OS_FS_PERMISSION_DENIED", OS_FS_PERMISSION_DENIED),
            ("OS_FS_NOT_EMPTY", OS_FS_NOT_EMPTY),
            ("OS_FS_DIRECTORY_TOO_LARGE", OS_FS_DIRECTORY_TOO_LARGE),
            ("OS_FS_UNAVAILABLE", OS_FS_UNAVAILABLE),
            ("OS_FS_UNKNOWN", OS_FS_UNKNOWN),
        ],
        retryable: &["os.fs.unavailable", "os.fs.unknown"],
        info: &[
            ("os.fs.not_found", ErrorInfo::new(false, true)),
            ("os.fs.is_directory", ErrorInfo::new(false, true)),
            ("os.fs.not_directory", ErrorInfo::new(false, true)),
            ("os.fs.already_exists", ErrorInfo::new(false, true)),
            ("os.fs.permission_denied", ErrorInfo::new(false, true)),
            ("os.fs.not_empty", ErrorInfo::new(false, true)),
            (
                "os.fs.directory_too_large",
                ErrorInfo::new(false, true)
                    .with_title("Directory listing exceeds the supported limit"),
            ),
            ("os.fs.unavailable", ErrorInfo::new(true, true)),
            ("os.fs.unknown", ErrorInfo::new(true, true)),
        ],
    };
}

pub mod os_process {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const OS_PROCESS_SPAWN_FAILED: &str = "os.process.spawn_failed";
    pub const OS_PROCESS_KILL_FAILED: &str = "os.process.kill_failed";
    pub const SHELL_GIT_BASH_NOT_FOUND: &str = "shell.git_bash_not_found";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "OsProcessErrors",
        codes: &[
            ("OS_PROCESS_SPAWN_FAILED", OS_PROCESS_SPAWN_FAILED),
            ("OS_PROCESS_KILL_FAILED", OS_PROCESS_KILL_FAILED),
            ("SHELL_GIT_BASH_NOT_FOUND", SHELL_GIT_BASH_NOT_FOUND),
        ],
        retryable: &[],
        info: &[
            ("os.process.spawn_failed", ErrorInfo::new(false, true)),
            ("os.process.kill_failed", ErrorInfo::new(false, true)),
            ("shell.git_bash_not_found", ErrorInfo::new(false, true)),
        ],
    };
}

pub mod plugin {
    use super::super::codes::ErrorDomain;

    pub const PLUGIN_NOT_FOUND: &str = "plugin.not_found";
    pub const PLUGIN_LOAD_FAILED: &str = "plugin.load_failed";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "PluginErrors",
        codes: &[
            ("PLUGIN_NOT_FOUND", PLUGIN_NOT_FOUND),
            ("PLUGIN_LOAD_FAILED", PLUGIN_LOAD_FAILED),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod profile {
    use super::super::codes::ErrorDomain;

    pub const MODEL_NOT_CONFIGURED: &str = "model.not_configured";
    pub const MODEL_CONFIG_INVALID: &str = "model.config_invalid";
    pub const THINKING_ALIAS_CONFLICT: &str = "profile.thinking_alias_conflict";
    pub const PROFILE_UNKNOWN: &str = "profile.unknown";
    pub const PROFILE_ALREADY_BOUND: &str = "profile.already_bound";
    pub const PROFILE_NOT_BOUND: &str = "profile.not_bound";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "ProfileErrors",
        codes: &[
            ("MODEL_NOT_CONFIGURED", MODEL_NOT_CONFIGURED),
            ("MODEL_CONFIG_INVALID", MODEL_CONFIG_INVALID),
            ("THINKING_ALIAS_CONFLICT", THINKING_ALIAS_CONFLICT),
            ("PROFILE_UNKNOWN", PROFILE_UNKNOWN),
            ("PROFILE_ALREADY_BOUND", PROFILE_ALREADY_BOUND),
            ("PROFILE_NOT_BOUND", PROFILE_NOT_BOUND),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod prompt {
    use super::super::codes::ErrorDomain;

    pub const REQUEST_INVALID: &str = "request.invalid";
    pub const REQUEST_WORK_DIR_REQUIRED: &str = "request.work_dir_required";
    pub const REQUEST_PROMPT_INPUT_EMPTY: &str = "request.prompt_input_empty";
    pub const PROMPT_ID_CONFLICT: &str = "prompt.id_conflict";
    pub const PROMPT_NOT_FOUND: &str = "prompt.not_found";
    pub const SESSION_BUSY: &str = "session.busy";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "PromptErrors",
        codes: &[
            ("REQUEST_INVALID", REQUEST_INVALID),
            ("REQUEST_WORK_DIR_REQUIRED", REQUEST_WORK_DIR_REQUIRED),
            ("REQUEST_PROMPT_INPUT_EMPTY", REQUEST_PROMPT_INPUT_EMPTY),
            ("PROMPT_ID_CONFLICT", PROMPT_ID_CONFLICT),
            ("PROMPT_NOT_FOUND", PROMPT_NOT_FOUND),
            ("SESSION_BUSY", SESSION_BUSY),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod models_dev_import {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const CATALOG_UNAVAILABLE: &str = "modelsDev.catalog_unavailable";
    pub const CATALOG_ENTRY_NOT_FOUND: &str = "modelsDev.catalog_entry_not_found";
    pub const CATALOG_IMPORT_INVALID: &str = "modelsDev.import_invalid";
    pub const REGISTRY_IMPORT_INVALID: &str = "modelsDev.registry_import_invalid";
    pub const PROVIDER_OAUTH_MANAGED: &str = "provider.oauth_managed";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "ModelsDevImportErrors",
        codes: &[
            ("CATALOG_UNAVAILABLE", CATALOG_UNAVAILABLE),
            ("CATALOG_ENTRY_NOT_FOUND", CATALOG_ENTRY_NOT_FOUND),
            ("CATALOG_IMPORT_INVALID", CATALOG_IMPORT_INVALID),
            ("REGISTRY_IMPORT_INVALID", REGISTRY_IMPORT_INVALID),
            ("PROVIDER_OAUTH_MANAGED", PROVIDER_OAUTH_MANAGED),
        ],
        retryable: &[],
        info: &[
            ("modelsDev.catalog_unavailable", ErrorInfo::new(true, true)),
            (
                "modelsDev.catalog_entry_not_found",
                ErrorInfo::new(false, true),
            ),
            ("modelsDev.import_invalid", ErrorInfo::new(false, true)),
            (
                "modelsDev.registry_import_invalid",
                ErrorInfo::new(false, true),
            ),
            ("provider.oauth_managed", ErrorInfo::new(false, true)),
        ],
    };
}

pub mod session_export {
    use super::super::codes::ErrorDomain;

    pub const SESSION_EXPORT_NOT_FOUND: &str = "session.export_not_found";
    pub const SESSION_EXPORT_MISSING_VERSION: &str = "session.export_missing_version";
    pub const SESSION_EXPORT_OUTPUT_CONFLICT: &str = "session.export_output_conflict";
    pub const SESSION_EXPORT_TOO_LARGE: &str = "session.export_too_large";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "SessionExportErrors",
        codes: &[
            ("SESSION_EXPORT_NOT_FOUND", SESSION_EXPORT_NOT_FOUND),
            (
                "SESSION_EXPORT_MISSING_VERSION",
                SESSION_EXPORT_MISSING_VERSION,
            ),
            (
                "SESSION_EXPORT_OUTPUT_CONFLICT",
                SESSION_EXPORT_OUTPUT_CONFLICT,
            ),
            ("SESSION_EXPORT_TOO_LARGE", SESSION_EXPORT_TOO_LARGE),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod session {
    use super::super::codes::ErrorDomain;

    pub const SESSION_NOT_FOUND: &str = "session.not_found";
    pub const SESSION_ALREADY_EXISTS: &str = "session.already_exists";
    pub const SESSION_ID_INVALID: &str = "session.id_invalid";
    pub const SESSION_ID_REQUIRED: &str = "session.id_required";
    pub const SESSION_ID_EMPTY: &str = "session.id_empty";
    pub const SESSION_TITLE_EMPTY: &str = "session.title_empty";
    pub const SESSION_MODEL_EMPTY: &str = "session.model_empty";
    pub const SESSION_THINKING_EMPTY: &str = "session.thinking_empty";
    pub const SESSION_PERMISSION_MODE_INVALID: &str = "session.permission_mode_invalid";
    pub const SESSION_STATE_NOT_FOUND: &str = "session.state_not_found";
    pub const SESSION_STATE_INVALID: &str = "session.state_invalid";
    pub const SESSION_APPROVAL_HANDLER_ERROR: &str = "session.approval_handler_error";
    pub const SESSION_QUESTION_HANDLER_ERROR: &str = "session.question_handler_error";
    pub const MESSAGE_NOT_FOUND: &str = "message.not_found";
    pub const SESSION_CLOSED: &str = "session.closed";
    pub const SESSION_FORK_ACTIVE_TURN: &str = "session.fork_active_turn";
    pub const SESSION_UNDO_UNAVAILABLE: &str = "session.undo_unavailable";
    pub const SESSION_INIT_FAILED: &str = "session.init_failed";
    pub const SESSION_PLAN_MODE_INVALID: &str = "session.plan_mode_invalid";
    pub const SESSION_TOWER_MODE_INVALID: &str = "session.tower_mode_invalid";
    pub const SESSION_OCCUPIED: &str = "session.occupied";
    pub const SESSION_STALE: &str = "session.stale";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "SessionErrors",
        codes: &[
            ("SESSION_NOT_FOUND", SESSION_NOT_FOUND),
            ("SESSION_ALREADY_EXISTS", SESSION_ALREADY_EXISTS),
            ("SESSION_ID_INVALID", SESSION_ID_INVALID),
            ("SESSION_ID_REQUIRED", SESSION_ID_REQUIRED),
            ("SESSION_ID_EMPTY", SESSION_ID_EMPTY),
            ("SESSION_TITLE_EMPTY", SESSION_TITLE_EMPTY),
            ("SESSION_MODEL_EMPTY", SESSION_MODEL_EMPTY),
            ("SESSION_THINKING_EMPTY", SESSION_THINKING_EMPTY),
            (
                "SESSION_PERMISSION_MODE_INVALID",
                SESSION_PERMISSION_MODE_INVALID,
            ),
            ("SESSION_STATE_NOT_FOUND", SESSION_STATE_NOT_FOUND),
            ("SESSION_STATE_INVALID", SESSION_STATE_INVALID),
            (
                "SESSION_APPROVAL_HANDLER_ERROR",
                SESSION_APPROVAL_HANDLER_ERROR,
            ),
            (
                "SESSION_QUESTION_HANDLER_ERROR",
                SESSION_QUESTION_HANDLER_ERROR,
            ),
            ("MESSAGE_NOT_FOUND", MESSAGE_NOT_FOUND),
            ("SESSION_CLOSED", SESSION_CLOSED),
            ("SESSION_FORK_ACTIVE_TURN", SESSION_FORK_ACTIVE_TURN),
            ("SESSION_UNDO_UNAVAILABLE", SESSION_UNDO_UNAVAILABLE),
            ("SESSION_INIT_FAILED", SESSION_INIT_FAILED),
            ("SESSION_PLAN_MODE_INVALID", SESSION_PLAN_MODE_INVALID),
            ("SESSION_TOWER_MODE_INVALID", SESSION_TOWER_MODE_INVALID),
            ("SESSION_OCCUPIED", SESSION_OCCUPIED),
            ("SESSION_STALE", SESSION_STALE),
        ],
        retryable: &["session.fork_active_turn", "session.occupied"],
        info: &[],
    };
}

pub mod skill {
    use super::super::codes::ErrorDomain;

    pub const SKILL_NOT_FOUND: &str = "skill.not_found";
    pub const SKILL_TYPE_UNSUPPORTED: &str = "skill.type_unsupported";
    pub const SKILL_NAME_EMPTY: &str = "skill.name_empty";
    pub const SKILL_PARSE_FAILED: &str = "skill.parse_failed";
    pub const SKILL_NESTED_TOO_DEEP: &str = "skill.nested_too_deep";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "SkillErrors",
        codes: &[
            ("SKILL_NOT_FOUND", SKILL_NOT_FOUND),
            ("SKILL_TYPE_UNSUPPORTED", SKILL_TYPE_UNSUPPORTED),
            ("SKILL_NAME_EMPTY", SKILL_NAME_EMPTY),
            ("SKILL_PARSE_FAILED", SKILL_PARSE_FAILED),
            ("SKILL_NESTED_TOO_DEEP", SKILL_NESTED_TOO_DEEP),
        ],
        retryable: &[],
        info: &[],
    };
}

pub mod storage {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const STORAGE_NOT_FOUND: &str = "storage.not_found";
    pub const STORAGE_DECODE_FAILED: &str = "storage.decode_failed";
    pub const STORAGE_CORRUPTED: &str = "storage.corrupted";
    pub const STORAGE_IO_FAILED: &str = "storage.io_failed";
    pub const STORAGE_LOCKED: &str = "storage.locked";
    pub const STORAGE_PERMISSION_DENIED: &str = "storage.permission_denied";
    pub const STORAGE_DISK_FULL: &str = "storage.disk_full";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "StorageErrors",
        codes: &[
            ("STORAGE_NOT_FOUND", STORAGE_NOT_FOUND),
            ("STORAGE_DECODE_FAILED", STORAGE_DECODE_FAILED),
            ("STORAGE_CORRUPTED", STORAGE_CORRUPTED),
            ("STORAGE_IO_FAILED", STORAGE_IO_FAILED),
            ("STORAGE_LOCKED", STORAGE_LOCKED),
            ("STORAGE_PERMISSION_DENIED", STORAGE_PERMISSION_DENIED),
            ("STORAGE_DISK_FULL", STORAGE_DISK_FULL),
        ],
        retryable: &["storage.io_failed", "storage.locked"],
        info: &[
            ("storage.not_found", ErrorInfo::new(false, true)),
            ("storage.decode_failed", ErrorInfo::new(false, true)),
            ("storage.corrupted", ErrorInfo::new(false, true)),
            ("storage.io_failed", ErrorInfo::new(true, true)),
            ("storage.locked", ErrorInfo::new(true, true)),
            ("storage.permission_denied", ErrorInfo::new(false, true)),
            ("storage.disk_full", ErrorInfo::new(false, true)),
        ],
    };
}

pub mod usage {
    use super::super::codes::ErrorDomain;

    pub const TURN_ID_CONFLICT: &str = "usage.turn_id_conflict";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "UsageErrors",
        codes: &[("TURN_ID_CONFLICT", TURN_ID_CONFLICT)],
        retryable: &[],
        info: &[],
    };
}

pub mod web {
    use super::super::codes::ErrorDomain;

    pub const WEB_INVALID_URL: &str = "web.invalid_url";
    pub const WEB_PRIVATE_ADDRESS: &str = "web.private_address";
    pub const WEB_FETCH_FAILED: &str = "web.fetch_failed";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "WebErrors",
        codes: &[
            ("WEB_INVALID_URL", WEB_INVALID_URL),
            ("WEB_PRIVATE_ADDRESS", WEB_PRIVATE_ADDRESS),
            ("WEB_FETCH_FAILED", WEB_FETCH_FAILED),
        ],
        retryable: &["web.fetch_failed"],
        info: &[],
    };
}

pub mod wire {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const WIRE_DUPLICATE_OP: &str = "wire.duplicate_op";
    pub const WIRE_UNKNOWN_RECORD: &str = "wire.unknown_record";
    pub const WIRE_MIGRATION_MISSING: &str = "wire.migration_missing";
    pub const WIRE_CYCLE: &str = "wire.cycle";
    pub const WIRE_VERSION_TOO_LOW: &str = "wire.version_too_low";
    pub const RECORDS_WRITE_FAILED: &str = "records.write_failed";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "WireErrors",
        codes: &[
            ("WIRE_DUPLICATE_OP", WIRE_DUPLICATE_OP),
            ("WIRE_UNKNOWN_RECORD", WIRE_UNKNOWN_RECORD),
            ("WIRE_MIGRATION_MISSING", WIRE_MIGRATION_MISSING),
            ("WIRE_CYCLE", WIRE_CYCLE),
            ("WIRE_VERSION_TOO_LOW", WIRE_VERSION_TOO_LOW),
            ("RECORDS_WRITE_FAILED", RECORDS_WRITE_FAILED),
        ],
        retryable: &[],
        info: &[
            ("wire.duplicate_op", ErrorInfo::new(false, true)),
            ("wire.unknown_record", ErrorInfo::new(false, true)),
            ("wire.cycle", ErrorInfo::new(false, true)),
            ("wire.migration_missing", ErrorInfo::new(false, true)),
            (
                "wire.version_too_low",
                ErrorInfo::new(false, true)
                    .with_title("Kimi Code upgrade required")
                    .with_action("Upgrade Kimi Code to the latest version to read this session."),
            ),
            ("records.write_failed", ErrorInfo::new(false, true)),
        ],
    };
}

pub mod workspace {
    use super::super::codes::ErrorDomain;

    pub const WORKSPACE_NOT_FOUND: &str = "workspace.not_found";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "WorkspaceErrors",
        codes: &[("WORKSPACE_NOT_FOUND", WORKSPACE_NOT_FOUND)],
        retryable: &[],
        info: &[],
    };
}

pub mod event {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const EVENT_DUPLICATE_EVENT: &str = "event.duplicate_event";
    pub const EVENT_SCHEMA_MISSING: &str = "event.schema_missing";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "EventErrors",
        codes: &[
            ("EVENT_DUPLICATE_EVENT", EVENT_DUPLICATE_EVENT),
            ("EVENT_SCHEMA_MISSING", EVENT_SCHEMA_MISSING),
        ],
        retryable: &[],
        info: &[
            ("event.duplicate_event", ErrorInfo::new(false, true)),
            ("event.schema_missing", ErrorInfo::new(false, true)),
        ],
    };
}

pub mod state {
    use super::super::codes::{ErrorDomain, ErrorInfo};

    pub const STATE_DUPLICATE_FOLD: &str = "state.duplicate_fold";
    pub const STATE_DURABILITY_MISMATCH: &str = "state.durability_mismatch";
    pub const STATE_CYCLE: &str = "state.cycle";

    pub static DOMAIN: ErrorDomain = ErrorDomain {
        id: "StateErrors",
        codes: &[
            ("STATE_DUPLICATE_FOLD", STATE_DUPLICATE_FOLD),
            ("STATE_DURABILITY_MISMATCH", STATE_DURABILITY_MISMATCH),
            ("STATE_CYCLE", STATE_CYCLE),
        ],
        retryable: &[],
        info: &[
            ("state.duplicate_fold", ErrorInfo::new(false, true)),
            ("state.durability_mismatch", ErrorInfo::new(false, true)),
            ("state.cycle", ErrorInfo::new(false, true)),
        ],
    };
}

pub static BUILTIN_DOMAINS: &[&ErrorDomain] = &[
    &core::DOMAIN,
    &attachment::DOMAIN,
    &lsp::DOMAIN,
    &session_query::DOMAIN,
    &prompt_optimizer::DOMAIN,
    &spec::DOMAIN,
    &agent_lifecycle::DOMAIN,
    &auth::DOMAIN,
    &task::DOMAIN,
    &protocol::DOMAIN,
    &config::DOMAIN,
    &capability::DOMAIN,
    &cron::DOMAIN,
    &debug::DOMAIN,
    &file::DOMAIN,
    &fs::DOMAIN,
    &full_compaction::DOMAIN,
    &goal::DOMAIN,
    &agent_loop::DOMAIN,
    &mcp::DOMAIN,
    &model_catalog::DOMAIN,
    &os_fs::DOMAIN,
    &os_process::DOMAIN,
    &plugin::DOMAIN,
    &profile::DOMAIN,
    &prompt::DOMAIN,
    &models_dev_import::DOMAIN,
    &session_export::DOMAIN,
    &session::DOMAIN,
    &skill::DOMAIN,
    &storage::DOMAIN,
    &usage::DOMAIN,
    &web::DOMAIN,
    &wire::DOMAIN,
    &workspace::DOMAIN,
    &event::DOMAIN,
    &state::DOMAIN,
];
