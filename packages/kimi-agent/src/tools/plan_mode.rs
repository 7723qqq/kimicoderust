use serde_json::Value;

use crate::native::shell_path_bridge::ShellPathBridge;

pub fn plan_guarded_tool(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "write"
            | "edit"
            | "taskstop"
            | "task_stop"
            | "croncreate"
            | "cron_create"
            | "crondelete"
            | "cron_delete"
    )
}

pub fn plan_denial(
    plan: &Value,
    tool_name: &str,
    args: &Value,
    workspace_root: Option<&std::path::Path>,
    bridge: &ShellPathBridge,
) -> Option<String> {
    if plan.get("active").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let plan_path = plan.get("path").and_then(|p| p.as_str());
    match tool_name.to_ascii_lowercase().as_str() {
        "write" | "edit" => {





            let write_paths: Vec<String> =
                crate::turn_loop::tool_scheduler::infer_tool_accesses(tool_name, args)
                    .iter()
                    .filter_map(|access| match access {
                        crate::turn_loop::types::ToolResourceAccess::File(file)
                            if matches!(
                                file.operation,
                                crate::turn_loop::types::FileOperation::Write
                                    | crate::turn_loop::types::FileOperation::ReadWrite
                            ) =>
                        {
                            Some(file.path.clone())
                        }
                        _ => None,
                    })
                    .collect();
            let Some(plan_path) = plan_path else {
                return Some(format!(
                    "Plan mode is active. You may only write to the current plan file: {}. Call ExitPlanMode to exit plan mode before editing other files.",
                    "(no plan file selected yet)"
                ));
            };
            if write_paths.is_empty() {
                return Some(format!(
                    "Plan mode is active. You may only write to the current plan file: {plan_path}. Call ExitPlanMode to exit plan mode before editing other files."
                ));
            }
            for path in &write_paths {




                let resolved = match workspace_root {
                    Some(root) => super::NativeToolset::candidate_path(root, bridge, path)
                        .to_string_lossy()
                        .into_owned(),
                    None => path.clone(),
                };


                if std::path::Path::new(plan_path) != std::path::Path::new(&resolved) {
                    return Some(format!(
                        "Plan mode is active. You may only write to the current plan file: {plan_path}. Call ExitPlanMode to exit plan mode before editing other files."
                    ));
                }
            }
            None
        }
        "taskstop" | "task_stop" => Some(
            "TaskStop is not available in plan mode. Call ExitPlanMode to exit plan mode before stopping a background task."
                .into(),
        ),
        "croncreate" | "cron_create" | "crondelete" | "cron_delete" => Some(format!(
            "{} is not available in plan mode because it would mutate scheduled work that runs after plan exit. Call ExitPlanMode first.",
            tool_name
        )),
        _ => None,
    }
}

pub fn plan_state_unavailable_message(tool_name: &str) -> String {
    crate::i18n::LocalizedText::with_params(
        "engine.permission.planStateUnavailable",
        crate::i18n::i18n_params!["tool" => tool_name],
    )
    .render()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> ShellPathBridge {
        ShellPathBridge::new(&crate::native::shell::resolve_shell(None).program)
    }

    fn active_plan(path: &str) -> Value {
        serde_json::json!({ "active": true, "id": "plan-1", "path": path })
    }

    #[test]
    fn test_plan_denial_inactive_plan_lets_everything_through() {
        for plan in [
            serde_json::json!({ "active": false }),
            serde_json::json!({}),
            Value::Null,
        ] {
            assert!(
                plan_denial(
                    &plan,
                    "write",
                    &serde_json::json!({ "path": "a.txt" }),
                    None,
                    &bridge(),
                )
                .is_none()
            );
            assert!(
                plan_denial(&plan, "taskstop", &serde_json::json!({}), None, &bridge()).is_none()
            );
        }
    }

    #[test]
    fn test_plan_denial_unguarded_tools_pass() {
        let plan = active_plan("/w/plans/plan-1.md");
        for tool in ["read", "grep", "bash", "todolist", "exitplanmode"] {
            assert!(
                plan_denial(&plan, tool, &serde_json::json!({}), None, &bridge()).is_none(),
                "tool: {tool}"
            );
        }
    }

    #[test]
    fn test_plan_denial_allows_plan_file_writes() {
        let plan = active_plan("/w/plans/plan-1.md");
        assert!(
            plan_denial(
                &plan,
                "Write",
                &serde_json::json!({ "path": "/w/plans/plan-1.md" }),
                None,
                &bridge(),
            )
            .is_none()
        );
        assert!(
            plan_denial(
                &plan,
                "Edit",
                &serde_json::json!({ "path": "/w/plans/plan-1.md" }),
                None,
                &bridge(),
            )
            .is_none()
        );
    }

    #[test]
    fn test_plan_denial_denies_other_writes_with_v2_message() {
        let plan = active_plan("/w/plans/plan-1.md");
        let denial = plan_denial(
            &plan,
            "Write",
            &serde_json::json!({ "path": "/w/src/main.rs" }),
            None,
            &bridge(),
        );
        assert_eq!(
            denial.unwrap(),
            "Plan mode is active. You may only write to the current plan file: /w/plans/plan-1.md. Call ExitPlanMode to exit plan mode before editing other files."
        );
    }

    #[test]
    fn test_plan_denial_resolves_relative_paths_against_workspace() {
        let plan = active_plan("/w/plans/plan-1.md");
        let root = std::path::Path::new("/w");
        assert!(
            plan_denial(
                &plan,
                "write",
                &serde_json::json!({ "path": "plans/plan-1.md" }),
                Some(root),
                &bridge(),
            )
            .is_none()
        );
        assert!(
            plan_denial(
                &plan,
                "write",
                &serde_json::json!({ "path": "src/main.rs" }),
                Some(root),
                &bridge(),
            )
            .is_some()
        );
    }

    #[test]
    fn test_plan_denial_without_plan_path_denies_all_writes() {
        let plan = serde_json::json!({ "active": true });
        let denial = plan_denial(
            &plan,
            "write",
            &serde_json::json!({ "path": "a.txt" }),
            None,
            &bridge(),
        );
        assert_eq!(
            denial.unwrap(),
            "Plan mode is active. You may only write to the current plan file: (no plan file selected yet). Call ExitPlanMode to exit plan mode before editing other files."
        );
    }

    #[test]
    fn test_plan_denial_denies_writes_without_a_path_argument() {
        let plan = active_plan("/w/plans/plan-1.md");
        let denial = plan_denial(&plan, "write", &serde_json::json!({}), None, &bridge());
        assert_eq!(
            denial.unwrap(),
            "Plan mode is active. You may only write to the current plan file: /w/plans/plan-1.md. Call ExitPlanMode to exit plan mode before editing other files."
        );
        let denial = plan_denial(
            &plan,
            "edit",
            &serde_json::json!({ "edits": [] }),
            None,
            &bridge(),
        );
        assert!(denial.is_some());
    }

    #[test]
    fn test_plan_denial_denies_taskstop_and_cron_mutations() {
        let plan = active_plan("/w/plans/plan-1.md");
        for tool in ["TaskStop", "taskstop", "task_stop"] {
            let denial = plan_denial(&plan, tool, &serde_json::json!({}), None, &bridge());
            assert!(
                denial
                    .unwrap()
                    .contains("TaskStop is not available in plan mode")
            );
        }
        for tool in ["CronCreate", "cron_create", "CronDelete", "cron_delete"] {
            let denial = plan_denial(&plan, tool, &serde_json::json!({}), None, &bridge());
            assert!(
                denial.unwrap().contains("would mutate scheduled work"),
                "tool: {tool}"
            );
        }
    }

    #[test]
    fn test_plan_guarded_tool_covers_both_spellings() {
        for tool in [
            "Write",
            "write",
            "Edit",
            "TaskStop",
            "CronCreate",
            "cron_delete",
        ] {
            assert!(plan_guarded_tool(tool), "tool: {tool}");
        }
        for tool in ["Read", "Grep", "Bash", "ExitPlanMode"] {
            assert!(!plan_guarded_tool(tool), "tool: {tool}");
        }
    }
}
