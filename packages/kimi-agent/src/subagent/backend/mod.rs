//! External subagent backends (`claude-code`, `codex`, `acp`), ported from v2
//! `session/subagent/backend/`.

pub mod acp_backend;
pub mod backend_compat;
pub mod claude_code_backend;
pub mod codex_backend;
pub mod codex_wire;
pub mod config_section;
pub mod flag;
pub mod process;
pub mod subagent_backend;
pub mod subagent_backend_service;
