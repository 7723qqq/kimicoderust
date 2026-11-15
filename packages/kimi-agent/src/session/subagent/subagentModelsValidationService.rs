use std::sync::Arc;

use crate::app::config::config::IConfigService;
use crate::app::scopes::LifecycleScope;
use crate::llm_adapter::model::catalog::IModelCatalog;
use crate::wire::record::WireAsyncError;

use super::config_section::assert_valid_subagent_model_config;
use super::subagent_models_validation::ISessionSubagentModelsValidationService;

pub struct SessionSubagentModelsValidationService;

impl SessionSubagentModelsValidationService {
    pub fn new(
        config: Arc<dyn IConfigService>,
        model_catalog: Arc<dyn IModelCatalog>,
    ) -> Result<Self, WireAsyncError> {
        assert_valid_subagent_model_config(config.as_ref(), model_catalog.as_ref())?;
        Ok(Self)
    }
}

impl ISessionSubagentModelsValidationService for SessionSubagentModelsValidationService {}
