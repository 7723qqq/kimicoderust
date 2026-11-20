pub use super::progress_tracker::{
    EBM_BLIND_MUTATION_THRESHOLD, EBM_REMINDER, ProgressTracker, ToolReceipt, ToolReceiptPaths,
    is_verification_command,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::agent::actor_service::services::{Dispose, EventBusService};
use crate::agent::tool_executor::tool_executor::IAgentToolExecutorService;
use crate::agent::tool_executor::tool_hooks::{ToolDidExecuteContext, ToolExecutionOutcome};
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::ReminderNotification;
use crate::hooks::HookRegistration;
use crate::turn_loop::types::{FileOperation, ToolResourceAccess};

pub const EBM_REMINDER_VARIANT: &str = "progress-track-ebm";

pub trait IProgressTrackerService: Send + Sync {}

pub struct ProgressTrackerService {
    tracker: Arc<parking_lot::Mutex<ProgressTracker>>,
    ebm_injected_this_turn: Arc<AtomicBool>,
    hook: parking_lot::Mutex<Option<HookRegistration<ToolDidExecuteContext>>>,
    turn_subscription: parking_lot::Mutex<Option<Dispose>>,
}

impl ProgressTrackerService {
    pub fn new(
        tool_executor: Arc<dyn IAgentToolExecutorService>,
        reminders: Arc<dyn ReminderService>,
        event_bus: Arc<dyn EventBusService>,
    ) -> Self {
        let tracker = Arc::new(parking_lot::Mutex::new(ProgressTracker::default()));
        let ebm_injected_this_turn = Arc::new(AtomicBool::new(false));
        let hook = tool_executor.hooks().on_did_execute_tool.register(
            "progress-track",
            Arc::new({
                let tracker = tracker.clone();
                let reminders = reminders.clone();
                let ebm_injected_this_turn = ebm_injected_this_turn.clone();
                move |context, next| {
                    observe_tool_round(
                        &context,
                        &tracker,
                        reminders.as_ref(),
                        &ebm_injected_this_turn,
                    );
                    Box::pin(async move { next.proceed(Some(context)).await })
                }
            }),
            Default::default(),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let turn_subscription = event_bus.subscribe(
            "turn.started",
            Arc::new({
                let ebm_injected_this_turn = ebm_injected_this_turn.clone();
                move |_| {
                    ebm_injected_this_turn.store(false, Ordering::Relaxed);
                }
            }),
        );
        Self {
            tracker,
            ebm_injected_this_turn,
            hook: parking_lot::Mutex::new(Some(hook)),
            turn_subscription: parking_lot::Mutex::new(Some(turn_subscription)),
        }
    }
}

impl IProgressTrackerService for ProgressTrackerService {}

impl Drop for ProgressTrackerService {
    fn drop(&mut self) {
        if let Some(hook) = self.hook.get_mut().take() {
            hook.dispose();
        }
        if let Some(subscription) = self.turn_subscription.get_mut().take() {
            subscription();
        }
    }
}

fn observe_tool_round(
    context: &ToolDidExecuteContext,
    tracker: &parking_lot::Mutex<ProgressTracker>,
    reminders: &dyn ReminderService,
    ebm_injected_this_turn: &AtomicBool,
) {
    let receipt = receipt_from_context(context);
    let blind = tracker.lock().score_round(&[receipt]).blind_mutations;
    if blind < EBM_BLIND_MUTATION_THRESHOLD || ebm_injected_this_turn.swap(true, Ordering::Relaxed) {
        return;
    }
    reminders.notify(
        EBM_REMINDER,
        &ReminderNotification {
            variant: EBM_REMINDER_VARIANT.to_string(),
            owner_prompt_id: None,
        },
    );
}

fn receipt_from_context(context: &ToolDidExecuteContext) -> ToolReceipt {
    let mut read = Vec::new();
    let mut write = Vec::new();
    for access in context.accesses.as_deref().unwrap_or_default() {
        let ToolResourceAccess::File(access) = access else {
            continue;
        };
        match access.operation {
            FileOperation::Write => write.push(access.path.clone()),
            FileOperation::ReadWrite => {
                read.push(access.path.clone());
                write.push(access.path.clone());
            }
            FileOperation::Read | FileOperation::Search => read.push(access.path.clone()),
        }
    }
    let paths = (!read.is_empty() || !write.is_empty()).then(|| ToolReceiptPaths {
        read,
        write: write.clone(),
    });
    ToolReceipt {
        tool_name: context.base.tool_call.name.clone(),
        command: context
            .base
            .args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        paths,
        success: context.outcome == ToolExecutionOutcome::Executed
            && context.result.is_error != Some(true),
        is_error: context.result.is_error,
        write_paths: write,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &str) -> ToolReceipt {
        ToolReceipt {
            write_paths: vec![path.to_string()],
            success: true,
            ..Default::default()
        }
    }

    fn command(value: &str) -> ToolReceipt {
        ToolReceipt {
            command: Some(value.to_string()),
            success: true,
            ..Default::default()
        }
    }

    #[test]
    fn test_verification_commands_are_recognized() {
        for value in [
            "bun run test",
            "cargo test --lib",
            "npm test",
            "pnpm vitest run",
            "make test",
            "bun run typecheck",
        ] {
            assert!(is_verification_command(value), "{value}");
        }
        for value in ["bun run build", "cargo build", "git status"] {
            assert!(!is_verification_command(value), "{value}");
        }
    }

    #[test]
    fn test_a_failing_command_does_not_clear_debt() {
        let mut tracker = ProgressTracker::default();
        tracker.observe(&write("src/main.rs"));
        let failed = ToolReceipt {
            command: Some("cargo test --lib".into()),
            success: false,
            ..Default::default()
        };

        assert_eq!(tracker.observe(&failed), 0);
    }
}
