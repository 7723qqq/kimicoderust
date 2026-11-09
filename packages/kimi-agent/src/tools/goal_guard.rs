use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::callbacks::HostCallbacks;
use crate::permission::PermissionMode;

const STALE_GOAL_TOOL_RESULT: &str =
    "Goal changed since this turn started; ignored stale goal tool call.";

fn is_goal_mutation_tool(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "creategoal"
            | "create_goal"
            | "updategoal"
            | "update_goal"
            | "setgoalbudget"
            | "set_goal_budget"
    )
}

fn is_create_goal(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "creategoal" | "create_goal"
    )
}

pub struct GoalGuard {
    pub bindings: Arc<Mutex<HashMap<String, Option<String>>>>,

    mode: RwLock<Option<PermissionMode>>,

    route_to_host: bool,
}

impl GoalGuard {
    pub fn new(mode: Option<PermissionMode>, route_to_host: bool) -> Self {
        Self {
            bindings: Arc::new(Mutex::new(HashMap::new())),
            mode: RwLock::new(mode),
            route_to_host,
        }
    }

    pub fn set_mode(&self, mode: Option<PermissionMode>) {
        *self.mode.write().unwrap_or_else(|e| e.into_inner()) = mode;
    }

    pub fn bind_turn(&self, turn_id: &str, goal_id: Option<&str>) {
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(turn_id.to_string(), goal_id.map(str::to_string));
    }

    pub fn requires_host(&self, tool_name: &str) -> bool {
        let mode = self.mode.read().unwrap_or_else(|e| e.into_inner());
        self.route_to_host && is_create_goal(tool_name) && *mode != Some(PermissionMode::Auto)
    }

    pub async fn stale_denial(
        &self,
        inner: &dyn HostCallbacks,
        turn_id: &str,
        tool_name: &str,
    ) -> Option<String> {
        if !is_goal_mutation_tool(tool_name) {
            return None;
        }
        let bound = self
            .bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(turn_id)
            .cloned()?;

        let current = inner.goal().await.ok()?;
        if current.map(|g| g.goal_id) != bound {
            return Some(STALE_GOAL_TOOL_RESULT.into());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::types::{
        BoxFuture, LlmChatRequest, LlmChatResponse, PermissionCheckRequest, PermissionDecision,
        ToolExecuteRequest, ToolExecuteResponse,
    };
    use crate::turn_loop::types::GoalContext;

    struct GoalScriptCallbacks {
        current: std::sync::Mutex<Option<GoalContext>>,
        broken: std::sync::atomic::AtomicBool,
        reads: std::sync::atomic::AtomicU32,
    }

    impl HostCallbacks for GoalScriptCallbacks {
        fn llm_chat(
            &self,
            _: LlmChatRequest,
        ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn execute_tool(
            &self,
            _: ToolExecuteRequest,
        ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn check_permission(
            &self,
            _: PermissionCheckRequest,
        ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
            Box::pin(async { Ok(PermissionDecision::allow()) })
        }

        fn goal(&self) -> BoxFuture<'static, Result<Option<GoalContext>, String>> {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.broken.load(std::sync::atomic::Ordering::Relaxed) {
                return Box::pin(async { Err("goal read failed".into()) });
            }
            let current = self.current.lock().unwrap().clone();
            Box::pin(async move { Ok(current) })
        }
    }

    fn active_goal(id: &str) -> GoalContext {
        GoalContext {
            goal_id: id.into(),
            objective: String::new(),
            status: crate::turn_loop::types::GoalStatus::Active,
            token_budget: None,
            turn_budget: None,
            wall_clock_budget_ms: None,
            tokens_used: 0,
            turns_used: 0,
            wall_clock_ms: 0,
            updated_at: 0,
            terminal_reason: None,
        }
    }

    #[tokio::test]
    async fn stale_denial_vetoes_when_current_goal_differs() {
        let guard = GoalGuard::new(Some(PermissionMode::Auto), true);
        let host = GoalScriptCallbacks {
            current: std::sync::Mutex::new(Some(active_goal("g2"))),
            broken: std::sync::atomic::AtomicBool::new(false),
            reads: std::sync::atomic::AtomicU32::new(0),
        };
        guard.bind_turn("t1", Some("g1"));

        let denial = guard.stale_denial(&host, "t1", "UpdateGoal").await;
        assert_eq!(denial.unwrap(), STALE_GOAL_TOOL_RESULT);
    }

    #[tokio::test]
    async fn stale_denial_vetoes_when_goal_cleared() {
        let guard = GoalGuard::new(Some(PermissionMode::Auto), true);
        let host = GoalScriptCallbacks {
            current: std::sync::Mutex::new(None),
            broken: std::sync::atomic::AtomicBool::new(false),
            reads: std::sync::atomic::AtomicU32::new(0),
        };
        guard.bind_turn("t1", Some("g1"));

        let denial = guard.stale_denial(&host, "t1", "SetGoalBudget").await;
        assert_eq!(denial.unwrap(), STALE_GOAL_TOOL_RESULT);
    }

    #[tokio::test]
    async fn stale_denial_allows_when_goal_unchanged() {
        let guard = GoalGuard::new(Some(PermissionMode::Auto), true);
        let host = GoalScriptCallbacks {
            current: std::sync::Mutex::new(Some(active_goal("g1"))),
            broken: std::sync::atomic::AtomicBool::new(false),
            reads: std::sync::atomic::AtomicU32::new(0),
        };
        guard.bind_turn("t1", Some("g1"));

        assert_eq!(guard.stale_denial(&host, "t1", "UpdateGoal").await, None);

        assert_eq!(guard.stale_denial(&host, "t2", "UpdateGoal").await, None);

        assert_eq!(guard.stale_denial(&host, "t1", "GetGoal").await, None);
        assert_eq!(host.reads.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn stale_denial_fails_open_on_broken_goal_read() {
        let guard = GoalGuard::new(Some(PermissionMode::Auto), true);
        let host = GoalScriptCallbacks {
            current: std::sync::Mutex::new(None),
            broken: std::sync::atomic::AtomicBool::new(true),
            reads: std::sync::atomic::AtomicU32::new(0),
        };
        guard.bind_turn("t1", Some("g1"));

        assert_eq!(guard.stale_denial(&host, "t1", "CreateGoal").await, None);
    }

    #[test]
    fn requires_host_routes_non_auto_and_unknown_modes_only() {
        let auto = GoalGuard::new(Some(PermissionMode::Auto), true);
        assert!(!auto.requires_host("CreateGoal"), "auto stays native");

        for mode in [
            Some(PermissionMode::Manual),
            Some(PermissionMode::Yolo),
            None,
        ] {
            let guard = GoalGuard::new(mode, true);
            assert!(guard.requires_host("CreateGoal"), "mode {mode:?} routes");
            assert!(guard.requires_host("create_goal"), "snake spelling routes");
        }

        let auto = GoalGuard::new(Some(PermissionMode::Auto), true);
        for tool in ["UpdateGoal", "SetGoalBudget", "GetGoal", "Read"] {
            assert!(!auto.requires_host(tool), "only CreateGoal routes: {tool}");
        }
        let no_route = GoalGuard::new(Some(PermissionMode::Manual), false);
        assert!(
            !no_route.requires_host("CreateGoal"),
            "routing off stays native"
        );
    }

    #[test]
    fn bind_turn_records_and_overwrites() {
        let guard = GoalGuard::new(Some(PermissionMode::Auto), true);
        guard.bind_turn("t1", Some("g1"));
        guard.bind_turn("t2", None);
        let map = guard.bindings.lock().unwrap();
        assert_eq!(map.get("t1").unwrap().as_deref(), Some("g1"));
        assert_eq!(map.get("t2").unwrap(), &None);
    }
}
