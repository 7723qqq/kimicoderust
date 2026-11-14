use crate::agent::actor_service::services::{BootstrapService, FlagService};
use crate::features::notify::flag::NOTIFY_USER_FLAG_ID;

pub const NOTIFY_USER_UI_CAPABILITY: &str = "update_panel";

pub fn notify_user_available(flags: &dyn FlagService, bootstrap: &dyn BootstrapService) -> bool {
    flags.enabled(NOTIFY_USER_FLAG_ID)
        && bootstrap
            .ui_capabilities()
            .iter()
            .any(|capability| capability == NOTIFY_USER_UI_CAPABILITY)
}
