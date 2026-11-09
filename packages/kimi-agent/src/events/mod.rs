//! Event bus and strongly-typed engine events for `kimi-agent` (P26 批 5).
//!
//! Replaces direct host-only `emit_event` calls with an in-process pub-sub
//! architecture. Subsystems (telemetry, local transcript, metrics, debug tap)
//! subscribe in-process; an optional UI sink forwards events to the host.

pub mod bus;
pub mod project;
pub mod types;

pub use bus::{EventBus, Subscription};
pub use project::Projector;
pub use types::EngineEvent;

/// v2 keeps a `hidden` thinking part in the accumulated assistant message but
/// never dispatches it (`agent/loop/loopService.ts:1564-1581`: the delta is
/// consumed, then `if (part.hidden === true) return`). The engine parks the
/// provider's `reasoning_details` summary in `detailsSummary` instead of
/// `think`, so this guards the wire: a part that still declares itself hidden
/// must not reach a host projection.
pub(crate) fn is_hidden_delta_part(part: &serde_json::Value) -> bool {
    part.get("hidden").and_then(|value| value.as_bool()) == Some(true)
}
