pub const SNAPSHOT_FEATURE_NAME: &str = "snapshot";
pub const SNAPSHOT_SERVICE_SCOPE: &str = "agent";
pub const SNAPSHOT_TOOL_NAME: &str = "SnapshotList";
pub const SNAPSHOT_TOOL_DOMAIN: &str = "snapshot";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotFeature;

impl SnapshotFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        SNAPSHOT_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        SNAPSHOT_SERVICE_SCOPE
    }
}
