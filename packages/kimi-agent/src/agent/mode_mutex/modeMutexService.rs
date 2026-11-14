use std::sync::Arc;

use serde_json::{Value, json};

use super::mode_mutex_service::{
    ExclusiveModeControl, ExclusiveReviewMode, MODE_MUTEX_EXIT_REASON, active_mode,
    evict_conflicts_of, leave, switch_to,
};
use crate::rpc::types::BoxFuture;

pub trait GoalControl: Send + Sync {
    fn is_active(&self) -> bool;

    fn pause_goal<'a>(
        &'a self,
        reason: &'a str,
        source: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;
}

pub struct AgentModeMutexServiceImpl {
    control: Arc<dyn ExclusiveModeControl>,
    goal: Option<Arc<dyn GoalControl>>,
    dispatch: Arc<dyn Fn(Value) + Send + Sync>,
}

impl AgentModeMutexServiceImpl {
    pub fn new(
        control: Arc<dyn ExclusiveModeControl>,
        goal: Option<Arc<dyn GoalControl>>,
        dispatch: Arc<dyn Fn(Value) + Send + Sync>,
    ) -> Self {
        Self {
            control,
            goal,
            dispatch,
        }
    }

    pub async fn on_plan_mode_enter(&self) {
        evict_conflicts_of(self.control.as_ref(), ExclusiveReviewMode::Plan).await;
    }

    pub async fn on_swarm_mode_enter(&self) {
        evict_conflicts_of(self.control.as_ref(), ExclusiveReviewMode::Swarm).await;
    }

    pub async fn on_hello_world_mode_enter(&self) {
        evict_conflicts_of(self.control.as_ref(), ExclusiveReviewMode::HelloWorld).await;
    }

    pub async fn on_spec_mode_enter(&self) {
        evict_conflicts_of(self.control.as_ref(), ExclusiveReviewMode::Spec).await;
    }

    pub async fn on_tower_mode_enter(&self) {
        evict_conflicts_of(self.control.as_ref(), ExclusiveReviewMode::Tower).await;
        if let Some(goal) = self.goal.as_ref()
            && goal.is_active()
        {
            let _ = goal
                .pause_goal("Paused after tower mode entered", "system")
                .await;
        }
    }

    pub async fn on_goal_updated(&self, agent_id: &str, status: Option<&str>) {
        if status != Some("active") {
            return;
        }
        if !self.control.is_active(ExclusiveReviewMode::Tower) {
            return;
        }
        (self.dispatch)(json!({
            "type": "agent.status.updated",
            "agentId": agent_id,
            "towerMode": false,
            "towerExitReason": MODE_MUTEX_EXIT_REASON,
        }));
        let _ = self
            .control
            .exit(ExclusiveReviewMode::Tower, Some(MODE_MUTEX_EXIT_REASON))
            .await;
    }
}

impl super::mode_mutex::AgentModeMutexService for AgentModeMutexServiceImpl {
    fn active_mode(&self) -> Option<ExclusiveReviewMode> {
        active_mode(self.control.as_ref())
    }

    fn switch_to(&self, target: ExclusiveReviewMode) -> BoxFuture<'static, Result<(), String>> {
        let control = self.control.clone();
        Box::pin(async move { switch_to(control.as_ref(), target).await })
    }

    fn leave(&self, target: ExclusiveReviewMode) -> BoxFuture<'static, Result<(), String>> {
        let control = self.control.clone();
        Box::pin(async move { leave(control.as_ref(), target).await })
    }
}
