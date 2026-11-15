use std::collections::HashMap;
use std::sync::Arc;

use crate::rpc::types::{SecondaryModelEntry, SecondaryModelPool};
use crate::turn_loop::types::LLM;

pub const PRIMARY_MODEL_CHOICE: &str = "primary";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentModelSource {
    PrimaryOverride,

    SecondaryPool,
}

#[derive(Clone)]
pub struct SubagentBinding {
    pub llm: Arc<dyn LLM>,
    pub source: SubagentModelSource,
}

#[derive(Clone)]
pub struct TowerModelBinding {
    pub llm: Arc<dyn LLM>,
    pub model: String,
}

pub struct SecondaryModelRuntime {
    config: SecondaryModelPool,
    llms: HashMap<String, Arc<dyn LLM>>,
}

impl SecondaryModelRuntime {
    pub fn new(config: SecondaryModelPool, llms: HashMap<String, Arc<dyn LLM>>) -> Self {
        Self { config, llms }
    }

    pub fn config(&self) -> &SecondaryModelPool {
        &self.config
    }

    pub fn exposes_choice(&self) -> bool {
        self.config.models.len() > 1
    }

    pub fn description(&self) -> String {
        let mut lines = vec!["Available models (pass via model):".to_string()];
        let default_model = self.config.default_model.as_str();
        let format_entry = |entry: &SecondaryModelEntry| {
            let marker = if entry.alias == default_model {
                " [default]"
            } else {
                ""
            };
            if entry.hint.is_empty() {
                format!("- {}{marker}", entry.alias)
            } else {
                format!("- {}{marker}: {}", entry.alias, entry.hint)
            }
        };

        if let Some(entry) = self
            .config
            .models
            .iter()
            .find(|entry| entry.alias == default_model)
        {
            lines.push(format_entry(entry));
        }
        for entry in self
            .config
            .models
            .iter()
            .filter(|entry| entry.alias != default_model)
        {
            lines.push(format_entry(entry));
        }
        lines.join("\n")
    }

    pub fn tower_description(&self) -> String {
        let primary_label = self
            .config
            .caller_model_alias
            .as_deref()
            .map(|alias| format!("{PRIMARY_MODEL_CHOICE} (= {alias})"))
            .unwrap_or_else(|| PRIMARY_MODEL_CHOICE.to_string());
        format!(
            "{}\n- {}: your current model and thinking level\nPool entries don't inherit your thinking level.",
            self.description(),
            primary_label
        )
    }

    pub fn resolve_tower(
        &self,
        primary: Arc<dyn LLM>,
        reviewer: bool,
        requested: Option<&str>,
    ) -> Result<TowerModelBinding, String> {
        if self.llms.contains_key(PRIMARY_MODEL_CHOICE) {
            return Err(format!(
                "[secondary_model.models] key \"{PRIMARY_MODEL_CHOICE}\" is reserved. Rename the pool entry."
            ));
        }
        let requested = requested.or(reviewer.then_some(PRIMARY_MODEL_CHOICE));
        if requested == Some(PRIMARY_MODEL_CHOICE) {
            return Ok(TowerModelBinding {
                model: self
                    .config
                    .caller_model_alias
                    .clone()
                    .unwrap_or_else(|| primary.model_name().to_string()),
                llm: primary,
            });
        }
        let alias = requested.unwrap_or(self.config.default_model.as_str());
        let llm = self.llms.get(alias).cloned().ok_or_else(|| {
            let mut available: Vec<&str> = self
                .config
                .models
                .iter()
                .map(|entry| entry.alias.as_str())
                .collect();
            available.push(PRIMARY_MODEL_CHOICE);
            format!(
                "Invalid model \"{alias}\". Available models: {}.",
                available.join(", ")
            )
        })?;
        Ok(TowerModelBinding {
            llm,
            model: alias.to_string(),
        })
    }

    pub fn resolve_tower_without_pool(
        primary: Arc<dyn LLM>,
        reviewer: bool,
        requested: Option<&str>,
    ) -> Result<TowerModelBinding, String> {
        let requested = requested.or(reviewer.then_some(PRIMARY_MODEL_CHOICE));
        if let Some(requested) = requested
            && requested != PRIMARY_MODEL_CHOICE
        {
            return Err(format!(
                "Invalid model \"{requested}\": no [secondary_model] pool is configured, so subagents inherit the caller's model (omit the model parameter)."
            ));
        }
        let model = primary.model_name().to_string();
        Ok(TowerModelBinding {
            llm: primary,
            model,
        })
    }

    pub fn resolve(&self, requested: Option<&str>) -> Result<SubagentBinding, String> {
        if self.llms.contains_key(PRIMARY_MODEL_CHOICE) {
            return Err(format!(
                "[secondary_model.models] key \"{PRIMARY_MODEL_CHOICE}\" is reserved. Rename the pool entry."
            ));
        }
        let choice = requested.unwrap_or(self.config.default_model.as_str());
        self.bind(choice, SubagentModelSource::SecondaryPool)
    }

    fn bind(&self, alias: &str, source: SubagentModelSource) -> Result<SubagentBinding, String> {
        let llm = self.llms.get(alias).ok_or_else(|| {
            let available: Vec<&str> = self
                .config
                .models
                .iter()
                .map(|entry| entry.alias.as_str())
                .collect();
            format!(
                "Invalid model \"{alias}\". Available models: {}.",
                available.join(", ")
            )
        })?;
        Ok(SubagentBinding {
            llm: llm.clone(),
            source,
        })
    }

    pub fn default_llm(&self) -> Option<Arc<dyn LLM>> {
        self.llms.get(self.config.default_model.as_str()).cloned()
    }

    pub fn resolve_without_pool(requested: Option<&str>) -> Result<(), String> {
        match requested {
            None => Ok(()),
            Some(requested) => Err(format!(
                "Invalid model \"{requested}\": no [secondary_model] pool is configured, so subagents inherit the caller's model (omit the model parameter)."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::types::{BoxFuture, SecondaryModelEntry};
    use crate::turn_loop::types::{LLMChatParams, LLMChatResponse};

    struct NamedLlm(String);

    impl LLM for NamedLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            &self.0
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            Box::pin(async {
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: kimi_core::types::TokenUsage::default(),
                    timing: None,
                })
            })
        }
    }

    fn llm_llm(name: &str) -> Arc<dyn LLM> {
        Arc::new(NamedLlm(name.into()))
    }

    fn entry(alias: &str, hint: &str) -> SecondaryModelEntry {
        SecondaryModelEntry {
            alias: alias.into(),
            hint: hint.into(),
            llm: Default::default(),
        }
    }

    fn runtime(models: Vec<SecondaryModelEntry>) -> SecondaryModelRuntime {
        let llms = models
            .iter()
            .map(|entry| (entry.alias.clone(), llm_llm(&entry.alias)))
            .collect();
        let config = SecondaryModelPool {
            default_model: models[0].alias.clone(),
            caller_model_alias: Some("main-model".into()),
            models,
        };
        SecondaryModelRuntime::new(config, llms)
    }

    fn pool_runtime() -> SecondaryModelRuntime {
        runtime(vec![
            entry("fast", "Cheap and quick."),
            entry("strong", "Pick this for hard problems."),
        ])
    }

    #[test]
    fn default_binding_uses_the_default_alias() {
        let pool = pool_runtime();
        let binding = pool.resolve(None).unwrap();
        assert_eq!(binding.source, SubagentModelSource::SecondaryPool);
        assert_eq!(binding.llm.model_name(), "fast");
    }

    #[test]
    fn explicit_alias_binds_that_alias() {
        let pool = pool_runtime();
        let binding = pool.resolve(Some("strong")).unwrap();
        assert_eq!(binding.llm.model_name(), "strong");
    }

    #[test]
    fn tower_resolver_binds_pool_default_alias_and_primary() {
        let pool = pool_runtime();
        let primary = llm_llm("main-wire");

        let worker = pool.resolve_tower(primary.clone(), false, None).unwrap();
        assert_eq!(worker.model, "fast");
        assert_eq!(worker.llm.model_name(), "fast");

        let explicit = pool
            .resolve_tower(primary.clone(), false, Some("strong"))
            .unwrap();
        assert_eq!(explicit.model, "strong");
        assert_eq!(explicit.llm.model_name(), "strong");

        let inherited = pool
            .resolve_tower(primary.clone(), false, Some("primary"))
            .unwrap();
        assert_eq!(inherited.model, "main-model");
        assert_eq!(inherited.llm.model_name(), "main-wire");

        let reviewer = pool.resolve_tower(primary, true, None).unwrap();
        assert_eq!(reviewer.model, "main-model");
        assert_eq!(reviewer.llm.model_name(), "main-wire");
    }

    #[test]
    fn tower_invalid_alias_lists_pool_entries_and_primary() {
        let pool = pool_runtime();
        let Err(error) = pool.resolve_tower(llm_llm("main-wire"), false, Some("other")) else {
            panic!("an unknown Tower alias must fail");
        };
        assert!(error.contains("fast, strong, primary"), "{error}");
    }

    #[test]
    fn tower_without_pool_accepts_primary_but_rejects_pool_aliases() {
        for (reviewer, requested) in [(false, None), (true, None), (false, Some("primary"))] {
            let binding = SecondaryModelRuntime::resolve_tower_without_pool(
                llm_llm("main-wire"),
                reviewer,
                requested,
            )
            .unwrap();
            assert_eq!(binding.llm.model_name(), "main-wire");
        }
        let Err(error) = SecondaryModelRuntime::resolve_tower_without_pool(
            llm_llm("main-wire"),
            false,
            Some("fast"),
        ) else {
            panic!("a pool alias without a pool must fail");
        };
        assert!(
            error.contains("no [secondary_model] pool is configured"),
            "{error}"
        );
        assert!(error.contains("omit the model parameter"), "{error}");
    }

    #[test]
    fn primary_is_an_invalid_alias_now() {
        let pool = pool_runtime();
        let Err(error) = pool.resolve(Some(PRIMARY_MODEL_CHOICE)) else {
            panic!("primary must not bind on the Agent path");
        };
        assert!(error.contains("Invalid model \"primary\""), "{error}");
        assert!(error.contains("fast, strong"), "{error}");
        assert!(!error.contains(", primary"), "{error}");
    }

    #[test]
    fn invalid_alias_lists_the_available_choices() {
        let pool = pool_runtime();
        let Err(error) = pool.resolve(Some("nope")) else {
            panic!("an unknown alias must fail");
        };
        assert!(error.contains("Invalid model \"nope\""), "{error}");
        assert!(error.contains("fast, strong"), "{error}");
    }

    #[test]
    fn a_single_entry_pool_hides_the_model_parameter() {
        assert!(!runtime(vec![entry("only", "")]).exposes_choice());
        assert!(pool_runtime().exposes_choice());
    }

    #[test]
    fn description_marks_default_and_lists_pool_entries_only() {
        let mut pool = pool_runtime();

        pool.config.models = vec![
            entry("strong", "Pick this for hard problems."),
            entry("fast", "Cheap and quick."),
        ];
        let description = pool.description();
        assert!(description.contains("- fast [default]"), "{description}");
        assert!(
            description.contains("- strong: Pick this for hard problems."),
            "{description}"
        );
        assert!(!description.contains("primary"), "{description}");
        let default_line = description
            .find("- fast [default]")
            .expect("default line listed");
        let strong_line = description.find("- strong:").expect("strong line listed");
        assert!(default_line < strong_line, "{description}");
    }

    #[test]
    fn without_a_pool_every_explicit_model_is_rejected() {
        assert!(SecondaryModelRuntime::resolve_without_pool(None).is_ok());
        for requested in ["fast", PRIMARY_MODEL_CHOICE] {
            let error = SecondaryModelRuntime::resolve_without_pool(Some(requested)).unwrap_err();
            assert!(
                error.contains("no [secondary_model] pool is configured"),
                "{requested}: {error}"
            );
            assert!(error.contains("omit the model parameter"), "{error}");
        }
    }

    #[test]
    fn tower_spawn_model_schema_tracks_pool_choices() {
        let pool = pool_runtime();
        let defs = crate::tools::tower::tower_tool_defs_with_models(Some(&pool));
        let spawn = defs
            .iter()
            .find(|definition| definition.name == "TowerSpawn")
            .unwrap();
        assert!(spawn.input_schema["properties"]["model"].is_object());
        assert!(spawn.description.contains("fast"), "{}", spawn.description);
        assert!(
            spawn.description.contains("strong"),
            "{}",
            spawn.description
        );
        assert!(
            spawn.description.contains("primary"),
            "{}",
            spawn.description
        );

        assert!(spawn.description.contains("primary (= main-model)"));
        assert!(
            spawn
                .description
                .contains("your current model and thinking level")
        );
        assert!(
            spawn
                .description
                .contains("Pool entries don't inherit your thinking level.")
        );

        let single = runtime(vec![entry("only", "")]);
        let defs = crate::tools::tower::tower_tool_defs_with_models(Some(&single));
        let spawn = defs
            .iter()
            .find(|definition| definition.name == "TowerSpawn")
            .unwrap();
        assert!(spawn.input_schema["properties"]["model"].is_object());
        assert!(spawn.description.contains("- only [default]"));
        assert!(spawn.description.contains("primary (= main-model)"));
        let defs = crate::tools::tower::tower_tool_defs_with_models(None);
        let spawn = defs
            .iter()
            .find(|definition| definition.name == "TowerSpawn")
            .unwrap();
        assert!(!spawn.input_schema["properties"]["model"].is_object());
    }
}
