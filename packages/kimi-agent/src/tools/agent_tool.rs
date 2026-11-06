//! Native `Agent` tool — the foreground core of v2's `SubagentTool` (P46).
//!
//! Scope (deliberately narrow): a foreground `Agent` call with a known
//! profile runs an inline subagent turn through the same native pipeline
//! (permission gate, stale/plan/dedup guards, truncation) and reports the
//! v2-shaped result. Everything the v2 tool supports beyond that stays
//! host-owned by falling back (returning `None`):
//! - profiles missing from the pushed snapshot (plugin sources, external
//!   backends like claude-code/codex)
//! - a turn without an injected subagent runtime
//!
//! `resume` (P55), `run_in_background` (P58), `fork` and the explicit
//! `model` override started out on that list and are native now.
//!
//! The host keeps its `Agent` tool registered either way, so a fallback is
//! a routing decision, never a capability loss.

mod defs;
mod events;
mod execute;
mod format;
mod resume;

pub use defs::{
    DEFAULT_PROFILE_NAME, DEFAULT_SUBAGENT_TIMEOUT_MS, agent_tool_def, agent_tool_def_filtered,
    format_timeout_description,
};
pub(crate) use defs::{
    agent_tool_def_filtered_with_background, background_agent_available_for_caller,
};
pub use events::emit_spawned_started;
pub use execute::execute_agent;
pub use format::agent_call_display;

#[allow(unused_imports)]
pub(crate) use events::{emit_cancelled, emit_completed, emit_failed, usage_json};

#[cfg(test)]
use crate::subagent::SubagentManager;
#[cfg(test)]
use crate::subagent::manager::ForegroundTurnOutcome;
#[cfg(test)]
use crate::subagent::types::ParentCancel;
#[cfg(test)]
use defs::{SUBAGENT_STOPPED_MESSAGE, USER_INTERRUPTED_SUBAGENT_MESSAGE, profile_tools_listing};
#[cfg(test)]
use resume::{execute_resume, tower_resume_denial};
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
mod tests;
