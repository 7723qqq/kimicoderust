pub const PROMPT_OPTIMIZER_FEATURE_NAME: &str = "promptOptimizer";
pub const PROMPT_OPTIMIZER_SERVICE_SCOPE: &str = "session";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptOptimizerFeature {
    service_enabled: bool,
}

impl PromptOptimizerFeature {
    pub fn new(flag_enabled: bool) -> Self {
        Self {
            service_enabled: flag_enabled,
        }
    }

    pub fn name(&self) -> &'static str {
        PROMPT_OPTIMIZER_FEATURE_NAME
    }

    pub fn contributes_session_service(&self) -> bool {
        self.service_enabled
    }
}
