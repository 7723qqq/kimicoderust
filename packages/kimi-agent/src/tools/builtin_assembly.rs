use std::sync::Arc;

use crate::agent::tool_activation::tool_activation::{
    ToolActivationProfileData, ToolActivationSource,
};
use crate::agent::tool_activation::tool_availability::AgentEnvironment;
use crate::agent::tool_registry::tool_contribution::{
    AgentToolContribution, AgentToolContributionOptions, ToolWhen,
};
use crate::subagent::secondary::SecondaryModelRuntime;
use crate::tools::registry::{ToolDefinition, ToolDisclosure, ToolSource};
use crate::tools::tool_policy::ToolsFilter;
use crate::turn_loop::types::ToolInfo;

pub struct BuiltinToolContext<'a> {
    pub github_available: bool,
    pub tower_enabled: bool,
    pub notify_user_enabled: bool,
    pub secondary_model: Option<&'a SecondaryModelRuntime>,
    pub filter: Option<&'a ToolsFilter>,
}

pub fn builtin_tool_contributions(context: &BuiltinToolContext<'_>) -> Vec<AgentToolContribution> {
    let mut records: Vec<(ToolInfo, Option<ToolWhen>)> = Vec::new();
    let operation_log_enabled =
        crate::features::operationLog::operationService::operation_log_enabled();
    let background_tasks_available = context
        .filter
        .is_none_or(ToolsFilter::allows_background_tasks);
    let background_shell_available = !operation_log_enabled && background_tasks_available;
    records.extend(
        crate::tools::core_tool_defs::core_tool_defs()
            .into_iter()
            .map(|mut info| {
                if !background_shell_available {
                    disable_shell_background_description(&mut info);
                }
                (info, None)
            }),
    );
    records.push((
        crate::tools::agent_tool::agent_tool_def(context.secondary_model),
        None,
    ));
    records.push((
        crate::tools::swarm_tool::agent_swarm_tool_def(context.secondary_model),
        None,
    ));
    records.push((crate::tools::todo_list::todo_list_tool_def(), None));
    let allow_background_questions = background_tasks_available;
    records.push((
        crate::agent::tools::ask_user_question::ask_user_question_tool::ask_user_question_tool_def(
            allow_background_questions,
        ),
        None,
    ));
    records.push((
        crate::tools::exit_plan_mode::exit_plan_mode_tool_def(),
        None,
    ));
    records.push((crate::tools::spec_mode::enter_spec_mode_tool_def(), None));
    records.push((crate::tools::spec_mode::exit_spec_mode_tool_def(), None));
    records.push((crate::tools::get_goal::get_goal_tool_def(), None));
    records.push((crate::tools::create_goal::create_goal_tool_def(), None));
    records.push((crate::tools::goal_tools::update_goal_tool_def(), None));
    records.push((crate::tools::goal_tools::set_goal_budget_tool_def(), None));
    records.push((crate::tools::cron_tools::cron_list_tool_def(), None));
    records.push((crate::tools::cron_tools::cron_create_tool_def(), None));
    records.push((crate::tools::cron_tools::cron_delete_tool_def(), None));
    records.push((crate::tools::task_tools::task_list_tool_def(), None));
    records.push((crate::tools::task_tools::task_output_tool_def(), None));
    records.push((crate::tools::task_tools::task_stop_tool_def(), None));
    records.push((
        crate::tools::task_tools::wait_for_tool_def(),
        Some(Arc::new(crate::tools::task_tools::wait_for_enabled) as ToolWhen),
    ));
    // v2 registers the wait_for flag at module load (task-wait/flag.ts:16);
    // force the LazyLock so `FlagService.explain("wait_for")` can resolve and
    // the subagent_background_task injection gate reads the same switch.
    let _ = crate::agent::tools::task::task_wait::flag::wait_for_flag();
    records.push((crate::tools::skill::skill_tool_def(), None));
    records.push((crate::tools::knowledge_tool::knowledge_tool_def(), None));
    records.push((
        crate::tools::core_tool_defs::memory_tool_def(),
        Some(Arc::new(crate::tools::memory_action::memory_tool_enabled) as ToolWhen),
    ));
    records.push((
        crate::tools::core_tool_defs::memory_graph_tool_def(),
        Some(Arc::new(
            crate::tools::memory_action::associative_memory_enabled,
        ) as ToolWhen),
    ));
    records.push((
        crate::tools::git_memory::repo_map_tool_def(),
        Some(Arc::new(crate::tools::memory_repo::git_memory_enabled) as ToolWhen),
    ));
    records.push((crate::tools::workflow::workflow_tool_def(), None));
    records.push((crate::tools::run_code::run_code_tool_def(), None));
    records.push((crate::tools::core_tool_defs::session_query_tool_def(), None));
    if context.github_available {
        records.extend(
            crate::agent::tools::github::github_legacy::github_tool_defs()
                .into_iter()
                .map(|info| (info, None)),
        );
    }
    if context.notify_user_enabled {
        records.push((crate::tools::core_tool_defs::notify_user_tool_def(), None));
    }
    if context.tower_enabled {
        records.extend(
            crate::tools::tower::tower_tool_defs_with_models(context.secondary_model)
                .into_iter()
                .map(|info| (info, None)),
        );
    }
    records
        .into_iter()
        .map(|(info, when)| contribution(info, when))
        .collect()
}

pub(crate) fn restrict_to_foreground(tool: &mut ToolInfo) {
    if tool.name == "AskUserQuestion" {
        *tool = crate::agent::tools::ask_user_question::ask_user_question_tool::ask_user_question_tool_def(false);
    } else {
        disable_shell_background_description(tool);
    }
}

fn disable_shell_background_description(tool: &mut ToolInfo) {
    match tool.name.as_str() {
        "Bash" => {
            tool.description =
                crate::tools::core_tool_defs::bash_description_without_background(&tool.description);
        }
        "PowerShell" => {
            tool.description =
                crate::tools::powershell::without_background_description(&tool.description);
        }
        _ => {}
    }
}

fn contribution(info: ToolInfo, when: Option<ToolWhen>) -> AgentToolContribution {
    let name = info.name.clone();
    let tool = ToolDefinition {
        name: info.name,
        description: info.description,
        input_schema: crate::tool::input_schema::to_input_json_schema(info.input_schema),
        source: ToolSource::Builtin,
        disclosure: ToolDisclosure::Inline,
    };
    let mut options = AgentToolContributionOptions::new(name.clone());
    options.source = Some(ToolSource::Builtin);
    options.when = when;
    AgentToolContribution::new(name, Arc::new(move || tool.clone()), options)
}

pub struct FilterActivationSource {
    filter: Option<ToolsFilter>,
}

impl FilterActivationSource {
    pub fn new(filter: Option<&ToolsFilter>) -> Self {
        Self {
            filter: filter.cloned(),
        }
    }
}

impl ToolActivationSource for FilterActivationSource {
    fn profile(&self) -> ToolActivationProfileData {
        let Some(filter) = &self.filter else {
            return ToolActivationProfileData::default();
        };
        ToolActivationProfileData {
            active_tool_names: (!filter.enabled.is_empty()).then(|| filter.enabled.clone()),
            disallowed_tools: (!filter.disabled.is_empty()).then(|| filter.disabled.clone()),
        }
    }

    fn workspace_disabled_tools(&self) -> Option<Vec<String>> {
        None
    }
}

pub struct EngineEnvironment;

impl AgentEnvironment for EngineEnvironment {
    fn is_available(&self, _capabilities: &[String]) -> bool {
        true
    }

    fn connect_inspectable(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(filter: Option<&ToolsFilter>) -> BuiltinToolContext<'_> {
        BuiltinToolContext {
            github_available: false,
            tower_enabled: false,
            notify_user_enabled: true,
            secondary_model: None,
            filter,
        }
    }

    #[test]
    fn contributions_follow_the_model_tool_order() {
        let records = builtin_tool_contributions(&context(None));
        let names: Vec<&str> = records
            .iter()
            .map(|record| record.options.name.as_str())
            .collect();
        assert_eq!(
            &names[..6],
            &["Read", "Grep", "Glob", "Write", "Edit", "Bash"]
        );
        assert!(names.contains(&"Memory"));
        assert!(names.contains(&"NotifyUser"));
        assert!(!names.contains(&"GitHubGetRepo"));
    }

    #[test]
    fn shell_descriptions_require_every_task_tool() {
        for disabled in ["TaskList", "TaskOutput", "TaskStop"] {
            let filter = ToolsFilter {
                enabled: Vec::new(),
                disabled: vec![disabled.to_string()],
            };
            let records = builtin_tool_contributions(&context(Some(&filter)));
            for record in records {
                if matches!(record.options.name.as_str(), "Bash" | "PowerShell") {
                    let tool = (record.ctor)();
                    assert!(
                        tool.description.contains("Background execution is disabled"),
                        "{disabled}: {}",
                        tool.description
                    );
                }
            }
        }
    }

    #[test]
    fn operation_log_only_disables_shell_background_execution() {
        let _env = crate::test_support::EnvScope::set(vec![(
            crate::features::operationLog::flag::OPERATION_LOG_FLAG_ENV,
            Some("1".into()),
        )]);
        let records = builtin_tool_contributions(&context(None));
        let mut saw_bash = false;
        let mut saw_question = false;
        for record in records {
            let tool = (record.ctor)();
            match tool.name.as_str() {
                "Bash" => {
                    saw_bash = true;
                    assert!(tool.description.contains("Background execution is disabled"));
                }
                "AskUserQuestion" => {
                    saw_question = true;
                    assert!(tool.input_schema["properties"].get("background").is_some());
                }
                _ => {}
            }
        }
        assert!(saw_bash && saw_question);
    }

    #[test]
    fn repo_map_is_advertised_only_with_the_git_memory_flag() {
        {
            let _scope =
                crate::test_support::EnvScope::unset(&["KIMI_CODE_EXPERIMENTAL_GIT_MEMORY"]);
            let names: Vec<String> =
                crate::tools::tool_policy::native_tool_defs(false, false, None, None)
                    .into_iter()
                    .map(|definition| definition.name)
                    .collect();
            assert!(!names.iter().any(|name| name == "RepoMap"), "{names:?}");
        }
        {
            let _scope = crate::test_support::EnvScope::set(vec![(
                "KIMI_CODE_EXPERIMENTAL_GIT_MEMORY",
                Some("1".into()),
            )]);
            let names: Vec<String> =
                crate::tools::tool_policy::native_tool_defs(false, false, None, None)
                    .into_iter()
                    .map(|definition| definition.name)
                    .collect();
            assert!(names.iter().any(|name| name == "RepoMap"), "{names:?}");
        }
    }

    #[test]
    fn every_object_schema_closes_additional_properties() {
        let records = builtin_tool_contributions(&BuiltinToolContext {
            github_available: true,
            tower_enabled: true,
            notify_user_enabled: true,
            secondary_model: None,
            filter: None,
        });
        for record in records {
            let tool = (record.ctor)();
            assert_closed(&tool.input_schema, &tool.name);
        }
    }

    fn assert_closed(value: &serde_json::Value, name: &str) {
        if let Some(object) = value.as_object() {
            if object.get("type").and_then(|kind| kind.as_str()) == Some("object") {
                assert!(
                    object.contains_key("additionalProperties"),
                    "{name} has an object schema without additionalProperties: {value}"
                );
            }
            for child in object.values() {
                assert_closed(child, name);
            }
        } else if let Some(items) = value.as_array() {
            for item in items {
                assert_closed(item, name);
            }
        }
    }

    #[test]
    fn the_filter_supplies_the_profile_layers() {
        let filter = ToolsFilter {
            enabled: vec!["Read".to_string()],
            disabled: vec!["Bash".to_string()],
        };
        let source = FilterActivationSource::new(Some(&filter));
        let profile = source.profile();
        assert_eq!(profile.active_tool_names, Some(vec!["Read".to_string()]));
        assert_eq!(profile.disallowed_tools, Some(vec!["Bash".to_string()]));
        assert_eq!(source.workspace_disabled_tools(), None);
    }
}
