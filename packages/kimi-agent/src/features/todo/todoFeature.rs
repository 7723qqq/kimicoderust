pub const TODO_FEATURE_NAME: &str = "todo";
pub const TODO_SERVICE_SCOPE: &str = "agent";
pub const TODO_TOOL_NAME: &str = "TodoList";
pub const TODO_TOOL_DOMAIN: &str = "todo";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TodoFeature;

impl TodoFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        TODO_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        TODO_SERVICE_SCOPE
    }
}
