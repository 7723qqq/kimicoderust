use serde_json::{Map, Value};

use crate::agent::r#loop::config_section::LOOP_CONTROL_SECTION;
use crate::agent::task::config_section::{LEGACY_BACKGROUND_SECTION, TASK_SECTION};
use crate::app::config::config::{ConfigInspectValue, ConfigTarget, IConfigService};
use crate::features::swarm::configSection::SWARM_SECTION;
use crate::session::subagent::configSection::SUBAGENT_SECTION;
use crate::wire::record::WireAsyncError;

pub const MAX_TIMER_DELAY_MS: u64 = 0x7fffffff;
pub const PRINT_WAIT_CEILING_S_DEFAULT: u64 = MAX_TIMER_DELAY_MS / 1000;
pub const PRINT_MAX_TURNS_DEFAULT: u32 = 100_000;
pub const PRINT_BASH_TASK_TIMEOUT_S_DEFAULT: u64 = 0;
pub const PRINT_SUBAGENT_TIMEOUT_MS_DEFAULT: u64 = 0;
pub const PRINT_SWARM_TIMEOUT_MS_DEFAULT: u64 = 0;

fn is_unset(inspected: &ConfigInspectValue, key: &str) -> bool {
    if inspected
        .user_value
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|value| value.contains_key(key))
        || inspected
            .memory_value
            .as_ref()
            .and_then(Value::as_object)
            .is_some_and(|value| value.contains_key(key))
    {
        return false;
    }
    let Some(effective) = inspected
        .value
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|value| value.get(key))
    else {
        return true;
    };
    inspected
        .default_value
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|value| value.get(key))
        == Some(effective)
}

async fn fill_section_default(
    config: &dyn IConfigService,
    domain: &str,
    key: &str,
    value: u64,
    legacy_user_value: Option<&Value>,
) -> Result<(), WireAsyncError> {
    let inspected = config.inspect(domain);
    if !is_unset(&inspected, key) {
        return Ok(());
    }
    if legacy_user_value
        .and_then(Value::as_object)
        .is_some_and(|value| value.contains_key(key))
    {
        return Ok(());
    }
    let mut next = inspected
        .value
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(Map::new);
    next.insert(key.to_string(), Value::from(value));
    config
        .set(
            domain,
            Some(Value::Object(next)),
            Some(ConfigTarget::Memory),
        )
        .await
}

pub async fn apply_print_mode_config_defaults(
    config: &dyn IConfigService,
) -> Result<(), WireAsyncError> {
    let legacy = config.inspect(LEGACY_BACKGROUND_SECTION);
    fill_section_default(
        config,
        TASK_SECTION,
        "bashTaskTimeoutS",
        PRINT_BASH_TASK_TIMEOUT_S_DEFAULT,
        legacy.user_value.as_ref(),
    )
    .await?;
    fill_section_default(config, LOOP_CONTROL_SECTION, "maxStepsPerTurn", 0, None).await?;
    fill_section_default(
        config,
        SUBAGENT_SECTION,
        "timeoutMs",
        PRINT_SUBAGENT_TIMEOUT_MS_DEFAULT,
        None,
    )
    .await?;
    fill_section_default(
        config,
        SWARM_SECTION,
        "timeoutMs",
        PRINT_SWARM_TIMEOUT_MS_DEFAULT,
        None,
    )
    .await
}
