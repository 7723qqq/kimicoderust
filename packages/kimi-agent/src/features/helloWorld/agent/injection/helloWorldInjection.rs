use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::helloWorld::{
    HelloWorldModeTrigger, HelloWorldRunItem, HelloWorldRunState,
};
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::features::reminder::reminderService::ReminderService;
use crate::features::reminder::types::{
    ContextInjectionContent, ContextInjectionContext, ContextInjectionResult,
};
use crate::rpc::types::BoxFuture;

pub const HELLO_WORLD_MODE_INJECTION_VARIANT: &str = "hello_world_mode";
pub const HELLO_WORLD_MODE_ENTER_REMINDER: &str = include_str!("../enter-reminder.md");
pub const HELLO_WORLD_MODE_EXIT_REMINDER: &str = include_str!("../exit-reminder.md");
pub const REQUIREMENTS_STAGE_SCRIPT: &str = include_str!("../stages/requirements.md");
pub const DESIGN_STAGE_SCRIPT: &str = include_str!("../stages/design.md");
pub const IMPLEMENT_STAGE_SCRIPT: &str = include_str!("../stages/implement.md");
pub const TEST_STAGE_SCRIPT: &str = include_str!("../stages/test.md");
pub const REVIEW_STAGE_SCRIPT: &str = include_str!("../stages/review.md");
pub const DOCS_STAGE_SCRIPT: &str = include_str!("../stages/docs.md");
pub const DELIVER_STAGE_SCRIPT: &str = include_str!("../stages/deliver.md");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloWorldModeInjectionDisclosure {
    pub kind: String,
    pub state: String,
    pub stage: String,
}

#[derive(Clone)]
pub struct HelloWorldInjectionOptions {
    pub get_trigger: Arc<dyn Fn() -> Option<HelloWorldModeTrigger> + Send + Sync>,
    pub get_run: Arc<dyn Fn() -> Option<HelloWorldRunState> + Send + Sync>,
}

struct HelloWorldInjectionInner {
    options: HelloWorldInjectionOptions,
    context: Arc<dyn IAgentContextMemoryService>,
}

impl HelloWorldInjectionInner {
    fn reminder(
        &self,
        ctx: &ContextInjectionContext,
    ) -> Option<ContextInjectionResult> {
        let trigger = (self.options.get_trigger)();
        let run = (self.options.get_run)();
        let last = self.rendered_disclosure(ctx);
        if let Some(run) = run
            && let Some(trigger) = trigger
            && trigger != HelloWorldModeTrigger::Tool
        {
            let stage = run.stage.clone();
            if last.as_ref().is_some_and(|last| {
                last.state == "active" && last.stage == stage
            }) {
                return None;
            }
            let opening = if last.as_ref().is_some_and(|last| last.state == "active") {
                ""
            } else {
                HELLO_WORLD_MODE_ENTER_REMINDER
            };
            let script = hello_world_stage_script(&stage)?;
            let content = if opening.is_empty() {
                format!("{}\n\n{script}", run_status_block(&run))
            } else {
                format!("{}\n\n{opening}\n\n{script}", run_status_block(&run))
            };
            return Some(ContextInjectionResult {
                content: ContextInjectionContent::Text(content),
                disclosure: Some(json!({
                    "kind": HELLO_WORLD_MODE_INJECTION_VARIANT,
                    "state": "active",
                    "stage": stage,
                })),
            });
        }
        if trigger.is_some() {
            return None;
        }
        let last = last.filter(|last| last.state == "active")?;
        Some(ContextInjectionResult {
            content: ContextInjectionContent::Text(HELLO_WORLD_MODE_EXIT_REMINDER.to_string()),
            disclosure: Some(json!({
                "kind": HELLO_WORLD_MODE_INJECTION_VARIANT,
                "state": "inactive",
                "stage": last.stage,
            })),
        })
    }

    fn rendered_disclosure(
        &self,
        ctx: &ContextInjectionContext,
    ) -> Option<HelloWorldModeInjectionDisclosure> {
        if let Some(last) = ctx.last_disclosure.as_ref()
            && let Ok(parsed) = serde_json::from_value::<HelloWorldModeInjectionDisclosure>(last.clone())
        {
            return Some(parsed);
        }
        for message in self.context.get().iter().rev() {
            let Some(origin) = message.origin.as_ref() else {
                continue;
            };
            if origin.injection_variant() == Some(HELLO_WORLD_MODE_INJECTION_VARIANT) {
                return Some(HelloWorldModeInjectionDisclosure {
                    kind: HELLO_WORLD_MODE_INJECTION_VARIANT.to_string(),
                    state: "active".to_string(),
                    stage: "requirements".to_string(),
                });
            }
        }
        None
    }
}

pub struct HelloWorldInjection {
    disposables: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
}

impl HelloWorldInjection {
    pub fn new(
        options: HelloWorldInjectionOptions,
        injector: Arc<dyn ReminderService>,
        context: Arc<dyn IAgentContextMemoryService>,
    ) -> Self {
        let inner = Arc::new(HelloWorldInjectionInner { options, context });
        let disposable = injector.register(
            HELLO_WORLD_MODE_INJECTION_VARIANT,
            Arc::new(
                move |ctx| -> BoxFuture<'static, Result<Option<ContextInjectionResult>, String>> {
                    let inner = inner.clone();
                    Box::pin(async move { Ok(inner.reminder(&ctx)) })
                },
            ),
        );
        Self {
            disposables: Mutex::new(vec![disposable]),
        }
    }

    pub fn dispose(&self) {
        for disposable in std::mem::take(&mut *self.disposables.lock()) {
            disposable();
        }
    }
}

impl Drop for HelloWorldInjection {
    fn drop(&mut self) {
        self.dispose();
    }
}

pub fn hello_world_stage_script(stage: &str) -> Option<&'static str> {
    Some(match stage {
        "requirements" => REQUIREMENTS_STAGE_SCRIPT,
        "design" => DESIGN_STAGE_SCRIPT,
        "implement" => IMPLEMENT_STAGE_SCRIPT,
        "test" => TEST_STAGE_SCRIPT,
        "review" => REVIEW_STAGE_SCRIPT,
        "docs" => DOCS_STAGE_SCRIPT,
        "deliver" => DELIVER_STAGE_SCRIPT,
        _ => return None,
    })
}

fn run_status_block(run: &HelloWorldRunState) -> String {
    let mut lines = vec![
        "## Run State".to_string(),
        format!("Task: {}", run.task.as_deref().unwrap_or("(not recorded)")),
        format!("Stage: {}", run.stage),
        "Checklist:".to_string(),
    ];
    lines.extend(run.items.iter().map(checklist_line));
    if !run.waivers.is_empty() {
        lines.push(format!("Waived: {}", run.waivers.join(", ")));
    }
    lines.join("\n")
}

fn checklist_line(item: &HelloWorldRunItem) -> String {
    let mark = if item.done { 'x' } else { ' ' };
    let evidence = item
        .evidence
        .as_deref()
        .map(|evidence| format!(" — {evidence}"))
        .unwrap_or_default();
    format!("- [{mark}] {}{evidence}", item.id)
}
