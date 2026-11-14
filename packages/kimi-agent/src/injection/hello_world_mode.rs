use crate::features::helloWorld::agent::helloWorld::{
    HELLO_WORLD_STAGES, HelloWorldModeTrigger, HelloWorldRunItem, HelloWorldRunState,
};
use crate::features::helloWorld::helloWorldOps::hello_world_mode_registry;
use crate::injection::InjectionContext;
use crate::turn_loop::types::LLMMessage;

pub const HELLO_WORLD_MODE_ENTER_REMINDER: &str =
    include_str!("../features/helloWorld/agent/enter-reminder.md");
pub const HELLO_WORLD_MODE_EXIT_REMINDER: &str =
    include_str!("../features/helloWorld/agent/exit-reminder.md");

pub const HELLO_WORLD_STAGE_SCRIPTS: [&str; 7] = [
    include_str!("../features/helloWorld/agent/stages/requirements.md"),
    include_str!("../features/helloWorld/agent/stages/design.md"),
    include_str!("../features/helloWorld/agent/stages/implement.md"),
    include_str!("../features/helloWorld/agent/stages/test.md"),
    include_str!("../features/helloWorld/agent/stages/review.md"),
    include_str!("../features/helloWorld/agent/stages/docs.md"),
    include_str!("../features/helloWorld/agent/stages/deliver.md"),
];

pub const HELLO_WORLD_MODE_INJECTION_VARIANT: &str = "hello_world_mode";

pub type HelloWorldDisclosure = (bool, String);

pub fn register_hello_world_mode_injection(
    registry: &mut crate::injection::InjectionRegistry,
    agent_id: String,
    baseline: Option<HelloWorldDisclosure>,
) {
    let mut disclosed = baseline;
    registry.register(
        HELLO_WORLD_MODE_INJECTION_VARIANT,
        Box::new(move |ctx: &InjectionContext| {
            if ctx.rearmed {
                disclosed = scan_hello_world_baseline(ctx.history);
            }
            let trigger = hello_world_mode_registry().trigger(&agent_id);
            let run = hello_world_mode_registry().run(&agent_id);
            if let (Some(trigger), Some(run)) = (trigger, run)
                && trigger != HelloWorldModeTrigger::Tool
            {
                let stage = run.stage.clone();
                if disclosed
                    .as_ref()
                    .is_some_and(|(active, last)| *active && *last == stage)
                {
                    return None;
                }
                let opening = if disclosed.as_ref().is_some_and(|(active, _)| *active) {
                    String::new()
                } else {
                    format!("{HELLO_WORLD_MODE_ENTER_REMINDER}\n\n")
                };
                let script = stage_script(&stage);
                disclosed = Some((true, stage));
                return Some(format!("{}\n\n{opening}{script}", run_status_block(&run)));
            }
            if trigger.is_some() {
                return None;
            }
            if !disclosed.as_ref().is_some_and(|(active, _)| *active) {
                return None;
            }
            let stage = disclosed
                .as_ref()
                .map(|(_, stage)| stage.clone())
                .unwrap_or_else(|| HELLO_WORLD_STAGES[0].to_string());
            disclosed = Some((false, stage));
            Some(HELLO_WORLD_MODE_EXIT_REMINDER.to_string())
        }),
    );
}

fn stage_script(stage: &str) -> &'static str {
    let index = HELLO_WORLD_STAGES
        .iter()
        .position(|candidate| *candidate == stage)
        .unwrap_or(0);
    HELLO_WORLD_STAGE_SCRIPTS[index]
}

fn run_status_block(run: &HelloWorldRunState) -> String {
    let mut lines = vec![
        "## Run State".to_string(),
        format!("Task: {}", run.task.as_deref().unwrap_or("(not recorded)")),
        format!("Stage: {}", run.stage),
        "Checklist:".to_string(),
    ];
    for item in &run.items {
        lines.push(checklist_line(item));
    }
    if !run.waivers.is_empty() {
        lines.push(format!("Waived: {}", run.waivers.join(", ")));
    }
    lines.join("\n")
}

fn checklist_line(item: &HelloWorldRunItem) -> String {
    let mark = if item.done { "x" } else { " " };
    let evidence = item
        .evidence
        .as_ref()
        .map(|evidence| format!(" — {evidence}"))
        .unwrap_or_default();
    format!("- [{mark}] {}{evidence}", item.id)
}

pub fn scan_hello_world_baseline(messages: &[LLMMessage]) -> Option<HelloWorldDisclosure> {
    for message in messages.iter().rev() {
        let Some(origin) = message.origin.as_ref() else {
            continue;
        };
        if origin.get("kind").and_then(serde_json::Value::as_str) != Some("injection") {
            continue;
        }
        if origin.get("variant").and_then(serde_json::Value::as_str)
            == Some(HELLO_WORLD_MODE_INJECTION_VARIANT)
        {
            return Some((true, HELLO_WORLD_STAGES[0].to_string()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::helloWorld::agent::helloWorld::create_hello_world_run;
    use crate::injection::{InjectionRegistry, wrap_system_reminder};

    fn injected(content: &str) -> LLMMessage {
        LLMMessage {
            role: "user".into(),
            content: wrap_system_reminder(content),
            origin: Some(serde_json::json!({
                "kind": "injection",
                "variant": "hello_world_mode",
            })),
            ..Default::default()
        }
    }

    fn agent(label: &str) -> String {
        format!("hello-world-injection-{label}-{}", ulid::Ulid::new())
    }

    #[test]
    fn entering_injects_the_status_block_and_the_stage_script_once() {
        let agent = agent("enter");
        hello_world_mode_registry().exit(&agent);
        hello_world_mode_registry().enter(
            &agent,
            HelloWorldModeTrigger::Manual,
            Some("ship".into()),
        );

        let mut registry = InjectionRegistry::new();
        register_hello_world_mode_injection(&mut registry, agent.clone(), None);

        let first = registry.build_injections(true);
        assert_eq!(first.len(), 1, "{first:?}");
        let text = &first[0];
        assert!(text.contains("## Hello-World Dev Run"), "{text}");
        assert!(text.contains("## Run State"), "{text}");
        assert!(text.contains("Task: ship"), "{text}");
        assert!(text.contains("Stage: requirements"), "{text}");
        assert!(text.contains("- [ ] requirements-approved"), "{text}");
        assert!(text.contains(&HELLO_WORLD_STAGE_SCRIPTS[0]), "{text}");

        assert!(
            registry.build_injections(false).is_empty(),
            "an unchanged stage must not repeat"
        );
        hello_world_mode_registry().exit(&agent);
    }

    #[test]
    fn a_stage_change_reinjects_without_the_enter_reminder() {
        let agent = agent("stage");
        hello_world_mode_registry().exit(&agent);
        hello_world_mode_registry().enter(&agent, HelloWorldModeTrigger::Manual, None);

        let mut registry = InjectionRegistry::new();
        register_hello_world_mode_injection(&mut registry, agent.clone(), None);
        assert_eq!(registry.build_injections(true).len(), 1);

        hello_world_mode_registry().advance(&agent, None);
        let second = registry.build_injections(false);
        assert_eq!(second.len(), 1, "{second:?}");
        assert!(
            !second[0].contains("## Hello-World Dev Run\n"),
            "{second:?}"
        );
        assert!(second[0].contains("Stage: design"), "{second:?}");
        assert!(
            second[0].contains(&HELLO_WORLD_STAGE_SCRIPTS[1]),
            "{second:?}"
        );
        assert!(registry.build_injections(false).is_empty());
        hello_world_mode_registry().exit(&agent);
    }

    #[test]
    fn leaving_injects_the_exit_reminder_once() {
        let agent = agent("exit");
        hello_world_mode_registry().exit(&agent);
        hello_world_mode_registry().enter(&agent, HelloWorldModeTrigger::Manual, None);

        let mut registry = InjectionRegistry::new();
        register_hello_world_mode_injection(&mut registry, agent.clone(), None);
        assert_eq!(registry.build_injections(true).len(), 1);

        hello_world_mode_registry().exit(&agent);
        assert_eq!(
            registry.build_injections(false),
            vec![wrap_system_reminder(HELLO_WORLD_MODE_EXIT_REMINDER)]
        );
        assert!(registry.build_injections(false).is_empty());
    }

    #[test]
    fn a_tool_triggered_run_is_never_announced() {
        let agent = agent("tool");
        hello_world_mode_registry().exit(&agent);
        hello_world_mode_registry().enter(&agent, HelloWorldModeTrigger::Tool, None);

        let mut registry = InjectionRegistry::new();
        register_hello_world_mode_injection(&mut registry, agent.clone(), None);
        assert!(registry.build_injections(true).is_empty());
        hello_world_mode_registry().exit(&agent);
    }

    #[test]
    fn the_waived_items_and_task_are_rendered_in_the_status_block() {
        let agent = agent("waive");
        hello_world_mode_registry().exit(&agent);
        hello_world_mode_registry().enter(
            &agent,
            HelloWorldModeTrigger::Manual,
            Some("ship".into()),
        );
        hello_world_mode_registry().waive(&agent, "requirements-approved");

        let mut registry = InjectionRegistry::new();
        register_hello_world_mode_injection(&mut registry, agent.clone(), None);
        let text = registry.build_injections(true).remove(0);
        assert!(
            text.contains("- [x] requirements-approved — waived"),
            "{text}"
        );
        assert!(text.contains("Waived: requirements-approved"), "{text}");
        hello_world_mode_registry().exit(&agent);
    }

    #[test]
    fn the_baseline_reads_the_last_disclosure_and_its_stage() {
        assert_eq!(scan_hello_world_baseline(&[]), None);
        assert_eq!(
            scan_hello_world_baseline(&[injected(HELLO_WORLD_MODE_ENTER_REMINDER)]),
            Some((true, "requirements".to_string()))
        );
        let active = format!(
            "{}\n\n{}",
            run_status_block(&create_hello_world_run(None)),
            HELLO_WORLD_STAGE_SCRIPTS[0]
        );
        assert_eq!(
            scan_hello_world_baseline(&[injected(&active)]),
            Some((true, "requirements".to_string()))
        );
        assert_eq!(
            scan_hello_world_baseline(&[injected(HELLO_WORLD_MODE_EXIT_REMINDER)]),
            Some((true, "requirements".to_string()))
        );
    }

    #[test]
    fn content_outside_an_injection_is_not_a_disclosure() {
        let quoted = LLMMessage {
            role: "user".into(),
            content: "why does the prompt say \"## Hello-World Dev Run\"?".to_string(),
            ..Default::default()
        };
        assert_eq!(scan_hello_world_baseline(&[quoted]), None);
    }
}
