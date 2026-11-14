use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::agent::helloWorld::{
    HelloWorldModeTrigger, HelloWorldRunState, HelloWorldRunStateDelta, ToolSignal, advance,
    create_hello_world_run, observe_tool_round, waive,
};

#[derive(Clone, Default)]
pub struct HelloWorldModeRegistry {
    active: Arc<Mutex<HashMap<String, HelloWorldModeEntry>>>,
}

#[derive(Clone, Debug)]
pub struct HelloWorldModeEntry {
    pub trigger: HelloWorldModeTrigger,
    pub run: HelloWorldRunState,
}

impl HelloWorldModeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self, agent_id: &str) -> bool {
        let guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        guard.contains_key(agent_id)
    }

    pub fn trigger(&self, agent_id: &str) -> Option<HelloWorldModeTrigger> {
        let guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(agent_id).map(|entry| entry.trigger)
    }

    pub fn run(&self, agent_id: &str) -> Option<HelloWorldRunState> {
        let guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(agent_id).map(|entry| entry.run.clone())
    }

    pub fn enter(
        &self,
        agent_id: &str,
        trigger: HelloWorldModeTrigger,
        task: Option<String>,
    ) -> Option<HelloWorldRunState> {
        let mut guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if guard.contains_key(agent_id) {
            return None;
        }
        let run = create_hello_world_run(task);
        guard.insert(
            agent_id.to_string(),
            HelloWorldModeEntry {
                trigger,
                run: run.clone(),
            },
        );
        Some(run)
    }

    pub fn exit(&self, agent_id: &str) -> Option<HelloWorldModeTrigger> {
        let mut guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(agent_id).map(|entry| entry.trigger)
    }

    pub fn advance(&self, agent_id: &str, stage: Option<&str>) -> Option<HelloWorldRunState> {
        let mut guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard.get_mut(agent_id)?;
        let delta = advance(&entry.run, stage)?;
        let HelloWorldRunStateDelta {
            stage,
            items,
            waivers,
        } = delta;
        entry.run = HelloWorldRunState {
            stage,
            task: entry.run.task.clone(),
            items,
            waivers,
        };
        Some(entry.run.clone())
    }

    pub fn waive(&self, agent_id: &str, item: &str) -> Option<HelloWorldRunState> {
        let mut guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard.get_mut(agent_id)?;
        let delta = waive(&entry.run, item)?;
        let HelloWorldRunStateDelta {
            stage,
            items,
            waivers,
        } = delta;
        entry.run = HelloWorldRunState {
            stage,
            task: entry.run.task.clone(),
            items,
            waivers,
        };
        Some(entry.run.clone())
    }

    pub fn observe(&self, agent_id: &str, signal: &ToolSignal) -> Option<HelloWorldRunState> {
        let mut guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard.get_mut(agent_id)?;
        let delta = observe_tool_round(&entry.run, signal)?;
        let HelloWorldRunStateDelta {
            stage,
            items,
            waivers,
        } = delta;
        entry.run = HelloWorldRunState {
            stage,
            task: entry.run.task.clone(),
            items,
            waivers,
        };
        Some(entry.run.clone())
    }
}

pub fn hello_world_mode_registry() -> &'static HelloWorldModeRegistry {
    static REGISTRY: std::sync::OnceLock<HelloWorldModeRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(HelloWorldModeRegistry::new)
}

pub fn hello_world_mode_enter_event(
    agent_id: &str,
    trigger: HelloWorldModeTrigger,
    task: Option<&str>,
) -> serde_json::Value {
    let mut event = serde_json::json!({
        "type": "hello_world_mode.enter",
        "agentId": agent_id,
        "trigger": trigger.as_str(),
    });
    if let Some(task) = task {
        event["task"] = serde_json::Value::String(task.to_string());
    }
    event
}

pub fn hello_world_mode_exit_event(agent_id: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "hello_world_mode.exit",
        "agentId": agent_id,
    })
}

pub fn hello_world_run_updated_event(
    agent_id: &str,
    run: &HelloWorldRunState,
) -> serde_json::Value {
    let mut event = serde_json::json!({
        "type": "hello_world_run.updated",
        "agentId": agent_id,
        "stage": run.stage,
        "items": run.items,
        "waivers": run.waivers,
    });
    if let Some(task) = &run.task {
        event["task"] = serde_json::Value::String(task.clone());
    }
    event
}

pub fn hello_world_status_event(
    agent_id: &str,
    mode: bool,
    stage: Option<&str>,
) -> serde_json::Value {
    let mut event = serde_json::json!({
        "type": "agent.status.updated",
        "agentId": agent_id,
        "helloWorldMode": mode,
    });
    if let Some(stage) = stage {
        event["helloWorldStage"] = serde_json::Value::String(stage.to_string());
    }
    event
}
