pub mod bus;
pub mod legacy;
pub mod project;
pub mod types;

pub use bus::{EventBus, Subscription};
pub use legacy::{AgentEvent, Event, ToolResultEvent};
pub use project::Projector;
pub use types::EngineEvent;

pub(crate) fn is_hidden_delta_part(part: &serde_json::Value) -> bool {
    part.get("hidden").and_then(|value| value.as_bool()) == Some(true)
}
