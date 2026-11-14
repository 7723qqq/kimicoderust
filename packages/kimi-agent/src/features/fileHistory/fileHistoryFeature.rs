pub const FILE_HISTORY_FEATURE_NAME: &str = "fileHistory";
pub const FILE_HISTORY_SERVICE_SCOPE: &str = "agent";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileHistoryFeature;

impl FileHistoryFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        FILE_HISTORY_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        FILE_HISTORY_SERVICE_SCOPE
    }
}
