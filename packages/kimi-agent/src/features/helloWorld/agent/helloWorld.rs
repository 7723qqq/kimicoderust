use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

pub const HELLO_WORLD_STAGES: [&str; 7] = [
    "requirements",
    "design",
    "implement",
    "test",
    "review",
    "docs",
    "deliver",
];

pub type HelloWorldStage = String;

pub const HELLO_WORLD_RUN_ITEM_IDS: [&str; 7] = [
    "requirements-approved",
    "design-written",
    "tasks-complete",
    "tests-green",
    "review-done",
    "docs-updated",
    "changeset-ready",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelloWorldModeTrigger {
    Manual,
    Task,
    Tool,
}

impl HelloWorldModeTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            HelloWorldModeTrigger::Manual => "manual",
            HelloWorldModeTrigger::Task => "task",
            HelloWorldModeTrigger::Tool => "tool",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "manual" => Some(HelloWorldModeTrigger::Manual),
            "task" => Some(HelloWorldModeTrigger::Task),
            "tool" => Some(HelloWorldModeTrigger::Tool),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloWorldRunItem {
    pub id: String,
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloWorldRunState {
    pub stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub items: Vec<HelloWorldRunItem>,
    pub waivers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloWorldRunStateDelta {
    pub stage: String,
    pub items: Vec<HelloWorldRunItem>,
    pub waivers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSignal {
    pub kind: ToolSignalKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSignalKind {
    Write,
    Command,
}

pub struct EvidenceRule {
    pub matches: fn(&ToolSignal) -> bool,
    pub from: &'static [&'static str],
    pub to: &'static str,
    pub items: &'static [&'static str],
}

pub static SPECS_REQUIREMENTS_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|/)specs/[^/]+/requirements\.md$").expect("path pattern compiles")
});
pub static SPECS_PLAN_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|/)specs/[^/]+/(design|tasks)\.md$").expect("path pattern compiles")
});
pub static SPECS_TREE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|/)specs/").expect("path pattern compiles"));
pub static DOCS_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|/)docs/|(^|/)README[^/]*$").expect("path pattern compiles"));
pub static CHANGESET_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|/)\.changeset/[^/]+\.md$").expect("path pattern compiles"));
pub static CHECK_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bbun\b.*\b(?:test|lint|typecheck|check:)").expect("command pattern compiles")
});

fn is_code_write_path(path: &str) -> bool {
    !SPECS_TREE.is_match(path) && !DOCS_PATH.is_match(path) && !CHANGESET_PATH.is_match(path)
}

pub static EVIDENCE_RULES: [EvidenceRule; 7] = [
    EvidenceRule {
        matches: |signal| {
            signal.kind == ToolSignalKind::Write && SPECS_REQUIREMENTS_PATH.is_match(&signal.value)
        },
        from: &["requirements"],
        to: "design",
        items: &["requirements-approved"],
    },
    EvidenceRule {
        matches: |signal| {
            signal.kind == ToolSignalKind::Write && SPECS_PLAN_PATH.is_match(&signal.value)
        },
        from: &["design"],
        to: "implement",
        items: &["design-written", "tasks-complete"],
    },
    EvidenceRule {
        matches: |signal| signal.kind == ToolSignalKind::Write && is_code_write_path(&signal.value),
        from: &["requirements", "design"],
        to: "implement",
        items: &[],
    },
    EvidenceRule {
        matches: |signal| {
            signal.kind == ToolSignalKind::Command && CHECK_COMMAND.is_match(&signal.value)
        },
        from: &["implement"],
        to: "test",
        items: &[],
    },
    EvidenceRule {
        matches: |signal| {
            signal.kind == ToolSignalKind::Command && CHECK_COMMAND.is_match(&signal.value)
        },
        from: &["test"],
        to: "review",
        items: &["tests-green"],
    },
    EvidenceRule {
        matches: |signal| signal.kind == ToolSignalKind::Write && DOCS_PATH.is_match(&signal.value),
        from: &["review"],
        to: "docs",
        items: &["review-done"],
    },
    EvidenceRule {
        matches: |signal| {
            signal.kind == ToolSignalKind::Write && CHANGESET_PATH.is_match(&signal.value)
        },
        from: &["docs"],
        to: "deliver",
        items: &["docs-updated", "changeset-ready"],
    },
];

pub fn create_hello_world_run(task: Option<String>) -> HelloWorldRunState {
    HelloWorldRunState {
        stage: HELLO_WORLD_STAGES[0].to_string(),
        task,
        items: HELLO_WORLD_RUN_ITEM_IDS
            .iter()
            .map(|id| HelloWorldRunItem {
                id: (*id).to_string(),
                done: false,
                evidence: None,
            })
            .collect(),
        waivers: Vec::new(),
    }
}

pub fn next_hello_world_stage(stage: &str) -> Option<&'static str> {
    let index = HELLO_WORLD_STAGES
        .iter()
        .position(|candidate| *candidate == stage)?;
    HELLO_WORLD_STAGES.get(index + 1).copied()
}

pub fn mark_waived(entry: HelloWorldRunItem) -> HelloWorldRunItem {
    HelloWorldRunItem {
        id: entry.id,
        done: true,
        evidence: Some("waived".to_string()),
    }
}

pub fn apply_evidence(
    run: &HelloWorldRunState,
    rule: &EvidenceRule,
    evidence: &str,
) -> HelloWorldRunState {
    HelloWorldRunState {
        stage: rule.to.to_string(),
        task: run.task.clone(),
        items: run
            .items
            .iter()
            .map(|item| {
                if rule.items.contains(&item.id.as_str()) && !item.done {
                    HelloWorldRunItem {
                        id: item.id.clone(),
                        done: true,
                        evidence: Some(evidence.to_string()),
                    }
                } else {
                    item.clone()
                }
            })
            .collect(),
        waivers: run.waivers.clone(),
    }
}

pub fn advance(run: &HelloWorldRunState, stage: Option<&str>) -> Option<HelloWorldRunStateDelta> {
    let target = match stage {
        Some(stage) => stage,
        None => next_hello_world_stage(&run.stage)?,
    };
    let current_index = HELLO_WORLD_STAGES
        .iter()
        .position(|candidate| *candidate == run.stage)?;
    let target_index = HELLO_WORLD_STAGES
        .iter()
        .position(|candidate| *candidate == target)?;
    if target_index <= current_index {
        return None;
    }
    Some(HelloWorldRunStateDelta {
        stage: target.to_string(),
        items: run.items.clone(),
        waivers: run.waivers.clone(),
    })
}

pub fn waive(run: &HelloWorldRunState, item: &str) -> Option<HelloWorldRunStateDelta> {
    if run.waivers.iter().any(|existing| existing == item) {
        return None;
    }
    Some(HelloWorldRunStateDelta {
        stage: run.stage.clone(),
        items: run
            .items
            .iter()
            .map(|entry| {
                if entry.id == item {
                    mark_waived(entry.clone())
                } else {
                    entry.clone()
                }
            })
            .collect(),
        waivers: run
            .waivers
            .iter()
            .cloned()
            .chain([item.to_string()])
            .collect(),
    })
}

pub fn observe_tool_round(
    run: &HelloWorldRunState,
    signal: &ToolSignal,
) -> Option<HelloWorldRunStateDelta> {
    let rule = EVIDENCE_RULES
        .iter()
        .find(|rule| rule.from.contains(&run.stage.as_str()) && (rule.matches)(signal))?;
    let next = apply_evidence(run, rule, &signal.value);
    Some(HelloWorldRunStateDelta {
        stage: next.stage,
        items: next.items,
        waivers: next.waivers,
    })
}

pub fn tool_signal(
    tool_name: &str,
    args: Option<&serde_json::Value>,
    executed: bool,
    is_error: bool,
) -> Option<ToolSignal> {
    if !executed || is_error {
        return None;
    }
    let args = args?.as_object()?;
    match tool_name {
        "Write" | "Edit" => {
            let path = args.get("path")?.as_str()?;
            Some(ToolSignal {
                kind: ToolSignalKind::Write,
                value: path.replace('\\', "/"),
            })
        }
        "Bash" => {
            let command = args.get("command")?.as_str()?;
            Some(ToolSignal {
                kind: ToolSignalKind::Command,
                value: command.to_string(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> HelloWorldRunState {
        create_hello_world_run(Some("ship it".to_string()))
    }

    fn write(path: &str) -> ToolSignal {
        ToolSignal {
            kind: ToolSignalKind::Write,
            value: path.to_string(),
        }
    }

    fn command(value: &str) -> ToolSignal {
        ToolSignal {
            kind: ToolSignalKind::Command,
            value: value.to_string(),
        }
    }

    #[test]
    fn a_new_run_starts_at_requirements_with_every_item_undone() {
        let state = run();
        assert_eq!(state.stage, "requirements");
        assert_eq!(state.task.as_deref(), Some("ship it"));
        assert_eq!(state.items.len(), HELLO_WORLD_RUN_ITEM_IDS.len());
        assert!(state.items.iter().all(|item| !item.done));
        assert_eq!(state.items[0].id, "requirements-approved");
        assert!(state.waivers.is_empty());
    }

    #[test]
    fn stages_walk_forward_and_stop_at_deliver() {
        assert_eq!(next_hello_world_stage("requirements"), Some("design"));
        assert_eq!(next_hello_world_stage("deliver"), None);
        assert_eq!(next_hello_world_stage("nope"), None);
    }

    #[test]
    fn advance_refuses_to_stand_still_or_go_backwards() {
        let state = run();
        assert!(advance(&state, Some("requirements")).is_none());
        assert!(advance(&state, Some("design")).is_some());
        assert!(advance(&state, None).is_some());
        let later = HelloWorldRunState {
            stage: "review".to_string(),
            ..state
        };
        assert!(advance(&later, Some("design")).is_none());
        assert_eq!(advance(&later, None).unwrap().stage, "docs");
    }

    #[test]
    fn waiving_marks_the_item_done_with_the_waiver_evidence() {
        let state = run();
        let delta = waive(&state, "requirements-approved").expect("a fresh waiver applies");
        assert_eq!(delta.items[0].id, "requirements-approved");
        assert!(delta.items[0].done);
        assert_eq!(delta.items[0].evidence.as_deref(), Some("waived"));
        assert_eq!(delta.waivers, vec!["requirements-approved".to_string()]);
        assert_eq!(delta.stage, "requirements");

        let waived = HelloWorldRunState {
            items: delta.items,
            waivers: delta.waivers,
            ..state
        };
        assert!(waive(&waived, "requirements-approved").is_none());
    }

    #[test]
    fn a_requirements_spec_advances_to_design_and_records_the_item() {
        let state = run();
        let delta = observe_tool_round(&state, &write("specs/kyle-rayner/requirements.md"))
            .expect("the requirements write is evidence");
        assert_eq!(delta.stage, "design");
        assert!(delta.items[0].done);
        assert_eq!(
            delta.items[0].evidence.as_deref(),
            Some("specs/kyle-rayner/requirements.md")
        );
        assert!(!delta.items[1].done);
    }

    #[test]
    fn evidence_only_applies_from_the_current_stage() {
        let state = run();
        assert!(observe_tool_round(&state, &command("bun test")).is_none());
        assert!(observe_tool_round(&state, &write("docs/README.md")).is_none());
    }

    #[test]
    fn a_plan_spec_completes_design_and_tasks_at_once() {
        let state = HelloWorldRunState {
            stage: "design".to_string(),
            ..run()
        };
        let delta = observe_tool_round(&state, &write("specs/x/tasks.md")).unwrap();
        assert_eq!(delta.stage, "implement");
        assert!(delta.items[1].done && delta.items[2].done);
        assert_eq!(delta.items[1].evidence.as_deref(), Some("specs/x/tasks.md"));
    }

    #[test]
    fn a_code_write_moves_requirements_or_design_to_implement() {
        for stage in ["requirements", "design"] {
            let state = HelloWorldRunState {
                stage: stage.to_string(),
                ..run()
            };
            let delta = observe_tool_round(&state, &write("src/main.rs")).unwrap();
            assert_eq!(delta.stage, "implement", "{stage}");
            assert!(delta.items.iter().all(|item| !item.done));
        }
        let state = HelloWorldRunState {
            stage: "implement".to_string(),
            ..run()
        };
        assert!(observe_tool_round(&state, &write("src/main.rs")).is_none());
    }

    #[test]
    fn a_check_command_walks_implement_to_test_to_review() {
        let implement = HelloWorldRunState {
            stage: "implement".to_string(),
            ..run()
        };
        let delta = observe_tool_round(&implement, &command("bun test")).unwrap();
        assert_eq!(delta.stage, "test");
        assert!(delta.items.iter().all(|item| !item.done));

        let test = HelloWorldRunState {
            stage: delta.stage.clone(),
            items: delta.items,
            waivers: delta.waivers,
            ..run()
        };
        let delta = observe_tool_round(&test, &command("bun run lint")).unwrap();
        assert_eq!(delta.stage, "review");
        assert_eq!(delta.items[3].id, "tests-green");
        assert!(delta.items[3].done);
    }

    #[test]
    fn docs_and_changesets_close_the_last_two_stages() {
        let review = HelloWorldRunState {
            stage: "review".to_string(),
            ..run()
        };
        let delta = observe_tool_round(&review, &write("docs/usage.md")).unwrap();
        assert_eq!(delta.stage, "docs");
        assert_eq!(delta.items[4].id, "review-done");
        assert!(delta.items[4].done);

        let docs = HelloWorldRunState {
            stage: delta.stage.clone(),
            items: delta.items,
            waivers: delta.waivers,
            ..run()
        };
        let delta = observe_tool_round(&docs, &write(".changeset/abc.md")).unwrap();
        assert_eq!(delta.stage, "deliver");
        assert!(delta.items[5].done && delta.items[6].done);
        assert_eq!(delta.items[6].id, "changeset-ready");
    }

    #[test]
    fn windows_paths_are_normalized_before_matching() {
        let state = run();
        let args = serde_json::json!({ "path": "specs\\kyle-rayner\\requirements.md" });
        let signal = tool_signal("Write", Some(&args), true, false).expect("a write signal");
        assert_eq!(signal.value, "specs/kyle-rayner/requirements.md");
        let delta = observe_tool_round(&state, &signal).unwrap();
        assert_eq!(delta.stage, "design");
    }

    #[test]
    fn only_successful_write_and_bash_rounds_produce_a_signal() {
        let args = serde_json::json!({ "path": "specs/x/requirements.md" });
        assert!(tool_signal("Write", Some(&args), true, false).is_some());
        assert!(tool_signal("Write", Some(&args), false, false).is_none());
        assert!(tool_signal("Write", Some(&args), true, true).is_none());
        assert!(tool_signal("Write", None, true, false).is_none());
        assert!(tool_signal("Read", Some(&args), true, false).is_none());

        let bash = serde_json::json!({ "command": "bun test" });
        assert!(tool_signal("Bash", Some(&bash), true, false).is_some());
        assert!(tool_signal("Bash", None, true, false).is_none());
    }
}
