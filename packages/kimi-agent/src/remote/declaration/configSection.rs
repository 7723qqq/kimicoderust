use crate::app::config::config::{IConfigService, RegisterSectionOptions};
use crate::app::config::config_section_contributions::register_config_section;
use crate::wire::record::WireAsyncError;

use super::remote_environment_declaration::{
    ENVIRONMENTS_SECTION_SCHEMA, EnvironmentDeclarationSet, EnvironmentsSection, section_entries,
};

pub const ENVIRONMENTS_SECTION: &str = "environments";

pub fn register_environments_config() {
    register_config_section(
        ENVIRONMENTS_SECTION,
        ENVIRONMENTS_SECTION_SCHEMA.clone(),
        RegisterSectionOptions::default(),
    );
}

pub async fn resolve_workspace_environment_declarations(
    config: &dyn IConfigService,
) -> Result<EnvironmentDeclarationSet, WireAsyncError> {
    config.ready().await?;
    config.assert_valid(ENVIRONMENTS_SECTION).await?;
    let user = config
        .get(ENVIRONMENTS_SECTION)
        .map(serde_json::from_value::<EnvironmentsSection>)
        .transpose()?;
    Ok(EnvironmentDeclarationSet {
        entries: section_entries(user.as_ref()),
        default_id: user.and_then(|user| user.default),
    })
}
