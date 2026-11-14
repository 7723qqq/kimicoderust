use super::tower::TOWER_FLAG_ID;
use super::towerFeature::TowerFeature;

pub const TOWER_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_TOWER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TowerFlag {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub env: &'static str,
    pub default: bool,
    pub surface: &'static str,
}

impl TowerFlag {
    pub fn is_exposed(&self, feature: &TowerFeature) -> bool {
        feature.is_tower_feature_assembled()
    }
}

pub const TOWER_FLAG: TowerFlag = TowerFlag {
    id: TOWER_FLAG_ID,
    title: "Tower mode",
    description: "Enable tower mode: coordinate multiple agents on a shared objective, toggled with the /tower command.",
    env: TOWER_FLAG_ENV,
    default: false,
    surface: "both",
};

pub use super::protocol::paths::tower_enabled;
