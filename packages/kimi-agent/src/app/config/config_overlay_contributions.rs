use std::sync::LazyLock;

use parking_lot::Mutex;

use super::config::{ApplyConfigOverlay, ConfigEffectiveOverlay, GetEnv, ValidateConfig};

static OVERLAYS: LazyLock<Mutex<Vec<ConfigEffectiveOverlay>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub fn register_config_overlay<F>(overlay: F)
where
    F: Fn(&mut serde_json::Value, &GetEnv, &ValidateConfig) -> Vec<&'static str>
        + Send
        + Sync
        + 'static,
{
    let overlay: ApplyConfigOverlay = std::sync::Arc::new(overlay);
    OVERLAYS.lock().push(ConfigEffectiveOverlay::new(overlay));
}

pub fn register_config_overlay_with_strip(overlay: ConfigEffectiveOverlay) {
    OVERLAYS.lock().push(overlay);
}

pub fn get_config_overlay_contributions() -> Vec<ConfigEffectiveOverlay> {
    OVERLAYS.lock().clone()
}

pub fn _clear_config_overlay_contributions_for_tests() {
    OVERLAYS.lock().clear();
}
