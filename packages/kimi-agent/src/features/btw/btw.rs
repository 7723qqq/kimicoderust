use async_trait::async_trait;

pub use crate::subagent::btw::{BTW_READONLY_TOOLS, TOOL_CALL_DISABLED_MESSAGE};

pub const SIDE_QUESTION_SYSTEM_REMINDER: &str = "\
This is a side-channel conversation with the user. You should answer user questions directly.

IMPORTANT:
- You are a separate, lightweight instance.
- The main agent continues independently; do not reference being interrupted.
- You may use the read-only tools Read, Grep, and Glob to inspect files when
  the answer depends on current file contents. All other tools are disabled
  and will be rejected, even though their definitions are visible in this
  request (they exist only for technical reasons — prompt cache).
- Prefer answering from what you already know from the conversation and this
  side-channel conversation; reach for the read-only tools only when needed.
- Follow-up turns may happen in this side-channel conversation.
- If you do not know the answer, say so directly.";

pub const BTW_AGENT_LABEL: &str = "btw";
pub const SESSION_BTW_SERVICE_ID: &str = "sessionBtwService";

#[async_trait]
pub trait ISessionBtwService: Send + Sync {
    async fn start(&self) -> Result<String, crate::wire::record::WireAsyncError>;
}
