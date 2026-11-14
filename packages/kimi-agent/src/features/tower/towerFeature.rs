use super::super::feature::Feature;
use super::tower::TOWER_WORKER_PROFILE;
use super::towerRateLimit::TOWER_RATE_LIMIT_SERVICE_ID;

pub const TOWER_FEATURE_NAME: &str = "tower";
pub const TOWER_TOOL_DOMAIN: &str = "tower";
pub const TOWER_RATE_LIMIT_SERVICE_SCOPE: &str = "app";
pub const TOWER_RATE_LIMIT_SERVICE_ACTIVATION: &str = "onDemand";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TowerToolContribution {
    pub id: &'static str,
    pub name: &'static str,
}

pub const TOWER_TOOL_CONTRIBUTIONS: &[TowerToolContribution] = &[
    TowerToolContribution { id: "towerInitTool", name: "TowerInit" },
    TowerToolContribution { id: "towerPlanTool", name: "TowerPlan" },
    TowerToolContribution { id: "towerSpawnTool", name: "TowerSpawn" },
    TowerToolContribution { id: "towerMergeTool", name: "TowerMerge" },
    TowerToolContribution { id: "towerRebaseTool", name: "TowerRebase" },
    TowerToolContribution { id: "towerTeardownTool", name: "TowerTeardown" },
    TowerToolContribution { id: "towerCompleteTool", name: "TowerComplete" },
    TowerToolContribution { id: "towerSendTool", name: "TowerSend" },
    TowerToolContribution { id: "towerInboxTool", name: "TowerInbox" },
    TowerToolContribution { id: "towerFindingTool", name: "TowerFinding" },
    TowerToolContribution { id: "towerReviewTool", name: "TowerReview" },
    TowerToolContribution { id: "towerMissionTool", name: "TowerMission" },
    TowerToolContribution { id: "towerStatusTool", name: "TowerStatus" },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TowerFeature {
    enabled: bool,
}

impl TowerFeature {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub fn name(&self) -> &'static str {
        TOWER_FEATURE_NAME
    }

    pub fn is_tower_feature_assembled(&self) -> bool {
        self.enabled
    }

    pub fn assemble(&self) -> Feature {
        let mut feature = Feature::new(TOWER_FEATURE_NAME);
        if !self.enabled {
            return feature;
        }
        feature.contribute_service(
            TOWER_RATE_LIMIT_SERVICE_SCOPE,
            TOWER_RATE_LIMIT_SERVICE_ID,
            Some(TOWER_RATE_LIMIT_SERVICE_ACTIVATION.to_string()),
        );
        for tool in TOWER_TOOL_CONTRIBUTIONS {
            feature.contribute_tool(tool.id, tool.name, TOWER_TOOL_DOMAIN);
        }
        feature.contribute_profiles(vec![TOWER_WORKER_PROFILE.to_string()], None, None);
        feature
    }
}
