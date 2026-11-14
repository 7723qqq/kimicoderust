//! System prompt builder and template variable interpolator.
//!
//! Orchestrates environment discovery, AGENTS.md cascading, skills catalog
//! rendering, and role profiles into the complete system prompt for any turn.

use std::path::{Path, PathBuf};

use super::agents_md::load_agents_md;
use super::environment::{collect_environment, generate_cwd_listing};
use super::profiles::ProfileCatalog;
use super::skills_renderer::generate_skills_section_with_options;
use crate::tools::{git_memory, memory_action, memory_repo, memory_store};

pub const DEFAULT_PRODUCT_NAME: &str = "Kimi Code CLI";
pub const DEFAULT_REPLY_STYLE_GUIDE: &str = "Your text replies render as Markdown in the user's terminal. Keep structure light and shallow — deep nesting, large tables, and heavy headings read poorly there. Cite code locations as `path/to/file.ts:42` so the user can navigate to them. Do not use emoji unless the user does first or asks for it.";

pub const NOTIFY_USER_GUIDANCE: &str = "When `NotifyUser` is available, use it proactively to keep the end user informed while you work. For a multi-step task, send an early update describing your approach, then report meaningful findings, phase conclusions, long waits, and blockers. Keep each update to one or two sentences in the end user's language; avoid repeating unchanged status. The UI adds the source label automatically. If you are working as a subagent, report only your own subtask's progress, do not present its completion as completion of the whole task, and do not ask the end user questions or request decisions. Updates do not automatically reach your parent agent: include every important finding in your final handoff. Updates remain visible until the main agent starts its next turn, so your final reply must still stand on its own.";

/// v2 `TODO_LIST_GUIDANCE` (`profile-shared.ts`): injected before the
/// final-reply checklist while the `TodoList` tool is active.
pub const TODO_LIST_GUIDANCE: &str = "When you track work in `TodoList`, keep it in step with what you have actually done: mark an item done as soon as it is finished, before you start the next one, and make sure the list matches what you did before you write your final message.";

pub const SYSTEM_PROMPT_TEMPLATE: &str = include_str!("./system.md");

/// Memory section, injected only when the memory filesystem is available.
pub const MEMORY_SECTION: &str = include_str!("./memory.md");

/// A line of [`MEMORY_SECTION`] that survives interpolation, so a caller
/// holding only the assembled prompt can tell whether the section is in it.
pub const MEMORY_SECTION_MARKER: &str = "You have a persistent memory filesystem";

/// Builder for constructing comprehensive agent system prompts.
#[derive(Debug, Clone)]
pub struct SystemPromptBuilder {
    workspace_root: PathBuf,
    product_name: String,
    reply_style_guide: String,
    override_shell: Option<String>,
    profile_name: String,
    model_adaptation: Option<String>,
    custom_brand_home: Option<PathBuf>,
    custom_agents_md: Option<String>,
    custom_skills_section: Option<String>,
    plugin_sections: Option<String>,
    additional_dirs: Vec<PathBuf>,
    /// Extra skill scan roots (`extra_skill_dirs`): the listed skills must
    /// match what the `Skill` tool can load.
    skill_dirs: Vec<PathBuf>,
    /// Plugin-contributed skill roots with their identity: scanned under
    /// qualified `<plugin_id>:<skill>` names (v2 `registerPluginSkill`), so
    /// the section advertises exactly what the `Skill` tool resolves.
    plugin_skill_dirs: Vec<crate::skills::PluginSkillDir>,
    /// `merge_all_available_skills` (schema): whether a scope group scans
    /// every directory it declares or only its first existing one. Defaults
    /// to the documented `true`.
    merge_all_available_skills: bool,
    notify_user_active: bool,
    todo_list_active: bool,
    memory_active: bool,
    associative_memory_override: Option<bool>,
    git_memory_override: Option<bool>,
}

impl SystemPromptBuilder {
    /// Create a new builder anchored at the given workspace root.
    pub fn new(workspace_root: impl AsRef<Path>) -> Self {
        Self {
            workspace_root: workspace_root.as_ref().to_path_buf(),
            product_name: DEFAULT_PRODUCT_NAME.to_string(),
            reply_style_guide: DEFAULT_REPLY_STYLE_GUIDE.to_string(),
            override_shell: None,
            profile_name: "agent".to_string(),
            model_adaptation: None,
            custom_brand_home: None,
            custom_agents_md: None,
            custom_skills_section: None,
            plugin_sections: None,
            additional_dirs: Vec::new(),
            skill_dirs: Vec::new(),
            plugin_skill_dirs: Vec::new(),
            merge_all_available_skills: true,
            notify_user_active: false,
            todo_list_active: false,
            memory_active: false,
            associative_memory_override: None,
            git_memory_override: None,
        }
    }

    fn with_memory_feature_flags(mut self, associative: bool, git: bool) -> Self {
        self.associative_memory_override = Some(associative);
        self.git_memory_override = Some(git);
        self
    }

    fn associative_memory_enabled(&self) -> bool {
        self.associative_memory_override
            .unwrap_or_else(memory_action::associative_memory_enabled)
    }

    fn git_memory_enabled(&self) -> bool {
        self.git_memory_override
            .unwrap_or_else(memory_repo::git_memory_enabled)
    }

    /// Set whether `NotifyUser` guidance is injected into the system prompt.
    #[must_use]
    pub fn with_notify_user(mut self, active: bool) -> Self {
        self.notify_user_active = active;
        self
    }

    /// Set whether the `TodoList` guidance line is injected into the system
    /// prompt (v2 gates it on the tool being active for the profile).
    #[must_use]
    pub fn with_todo_list(mut self, active: bool) -> Self {
        self.todo_list_active = active;
        self
    }

    /// Set whether the memory section is injected into the system prompt.
    /// [`Self::new`] leaves it off; the production entry points
    /// ([`Self::build_default`] and [`Self::build_default_with_skill_config`])
    /// turn it on, because the memory tools are in the session tool table and
    /// the section is what tells the model how to use them.
    #[must_use]
    pub fn with_memory(mut self, active: bool) -> Self {
        self.memory_active = active;
        self
    }

    /// Extra skill scan roots (schema `extra_skill_dirs`).
    #[must_use]
    pub fn with_skill_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.skill_dirs = dirs;
        self
    }

    /// Plugin-contributed skill roots (v2 `manager.pluginSkillRoots`): scanned
    /// under qualified `<plugin_id>:<skill>` names, matching the `Skill`
    /// tool's scan (`NativeToolset::with_skill_scan`).
    #[must_use]
    pub fn with_plugin_skill_dirs(mut self, dirs: Vec<crate::skills::PluginSkillDir>) -> Self {
        self.plugin_skill_dirs = dirs;
        self
    }

    /// `merge_all_available_skills` (schema): off restricts each scope group
    /// to its first existing directory. The same value has to reach the `Skill`
    /// tool (`NativeToolset::with_skill_scan`), or the tool resolves skills the
    /// prompt never listed.
    #[must_use]
    pub fn with_merge_all_available_skills(mut self, merge: bool) -> Self {
        self.merge_all_available_skills = merge;
        self
    }

    pub fn with_product_name(mut self, name: impl Into<String>) -> Self {
        self.product_name = name.into();
        self
    }

    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile_name = profile.into();
        self
    }

    pub fn with_model_adaptation(mut self, adaptation: Option<String>) -> Self {
        self.model_adaptation = adaptation;
        self
    }

    pub fn with_shell(mut self, shell_path: impl Into<String>) -> Self {
        self.override_shell = Some(shell_path.into());
        self
    }

    pub fn with_brand_home(mut self, path: impl AsRef<Path>) -> Self {
        self.custom_brand_home = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn with_custom_agents_md(mut self, md: impl Into<String>) -> Self {
        self.custom_agents_md = Some(md.into());
        self
    }

    pub fn with_custom_skills(mut self, skills: impl Into<String>) -> Self {
        self.custom_skills_section = Some(skills.into());
        self
    }

    pub fn with_plugin_sections(mut self, plugins: impl Into<String>) -> Self {
        self.plugin_sections = Some(plugins.into());
        self
    }

    pub fn add_additional_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.additional_dirs.push(dir.as_ref().to_path_buf());
        self
    }

    /// Every extra root the sandbox authorizes, in one call. The
    /// `${additional_dirs_section}` block is what tells the model these
    /// directories are in scope (v2 `loadAdditionalDirsInfo`), so a host that
    /// authorizes roots must pass them here or the model never learns they exist.
    #[must_use]
    pub fn with_additional_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.additional_dirs = dirs;
        self
    }

    /// Build the default system prompt for the given workspace path.
    ///
    /// The default prompt carries the memory section: the memory tools are in
    /// the session tool table, so a prompt without the section would advertise
    /// a capability the model was never told how to use.
    pub fn build_default(
        workspace_root: impl AsRef<Path>,
    ) -> Result<String, crate::native::environment_probe::ProbeShellNotFound> {
        Self::new(workspace_root).with_memory(true).build()
    }

    /// [`Self::build_default`] with the skill scan (schema `extra_skill_dirs`
    /// plus `merge_all_available_skills`): the file-reading entry points resolve
    /// both from config and pass them here.
    pub fn build_default_with_skill_config(
        workspace_root: impl AsRef<Path>,
        skill_dirs: Vec<PathBuf>,
        merge_all_available_skills: bool,
    ) -> Result<String, crate::native::environment_probe::ProbeShellNotFound> {
        Self::new(workspace_root)
            .with_skill_dirs(skill_dirs)
            .with_merge_all_available_skills(merge_all_available_skills)
            .with_memory(true)
            .build()
    }

    /// [`Self::build_default_with_skill_config`] with an agent profile name
    /// (`--agent`, or the name a `--agent-file` defines). The profile supplies
    /// the role overlay (`${role_additional}`) and its tool-allowance list;
    /// an unknown name falls back to the default `agent` profile, matching
    /// [`crate::prompt::ProfileCatalog::get`]'s lookup.
    pub fn build_for_profile(
        workspace_root: impl AsRef<Path>,
        skill_dirs: Vec<PathBuf>,
        merge_all_available_skills: bool,
        profile_name: impl Into<String>,
    ) -> Result<String, crate::native::environment_probe::ProbeShellNotFound> {
        Self::new(workspace_root)
            .with_profile(profile_name)
            .with_skill_dirs(skill_dirs)
            .with_merge_all_available_skills(merge_all_available_skills)
            .with_memory(true)
            .build()
    }

    /// Build the full system prompt string.
    pub fn build(self) -> Result<String, crate::native::environment_probe::ProbeShellNotFound> {
        self.build_with_warnings().map(|(prompt, _)| prompt)
    }

    /// [`Self::build`] plus the user-facing warnings the load produced.
    ///
    /// Only the AGENTS.md loader reports one (v2 `agentsMdWarning`): the
    /// aggregate instruction size crossed the recommended budget, or a
    /// candidate exists but could not be read. v2's profile service publishes
    /// it as a `warning` event with code `agents-md-oversized`
    /// (`publishAgentsMdWarning`, `profileService.ts:874-884`); a caller with
    /// an event lane forwards these, everyone else keeps using [`Self::build`].
    pub fn build_with_warnings(
        self,
    ) -> Result<(String, Vec<String>), crate::native::environment_probe::ProbeShellNotFound> {
        let associative_memory_enabled = self.associative_memory_enabled();
        let git_memory_enabled = self.git_memory_enabled();
        // 1. Environment
        let env = collect_environment(&self.workspace_root, self.override_shell.as_deref())?;

        // 2. Profile role additional
        let catalog = ProfileCatalog::with_builtins();
        let role_additional = catalog
            .get(&self.profile_name)
            .map(|p| p.role_additional.clone())
            .unwrap_or_default();

        // 3. AGENTS.md
        let mut warnings: Vec<String> = Vec::new();
        let agents_md = if let Some(custom) = self.custom_agents_md {
            custom
        } else {
            let loaded = load_agents_md(&self.workspace_root, self.custom_brand_home.as_deref());
            if let Some(warning) = loaded.warning {
                warnings.push(warning);
            }
            loaded.content
        };

        // 4. Skills section
        let skills_section = if let Some(custom) = self.custom_skills_section {
            custom
        } else {
            generate_skills_section_with_options(
                Some(&self.workspace_root),
                &self.skill_dirs,
                &self.plugin_skill_dirs,
                self.merge_all_available_skills,
            )
        };

        // 5. Additional directories
        let additional_dirs_section = if self.additional_dirs.is_empty() {
            String::new()
        } else {
            let mut buf = String::from(
                "\n\n## Additional Directories\n\nThe following directories have been added to the workspace. You can read, write, search, and glob files in these directories as part of your workspace scope.\n\n",
            );
            for dir in &self.additional_dirs {
                let listing = generate_cwd_listing(dir, true);
                buf.push_str(&format!("### {}\n{}\n\n", dir.display(), listing));
            }
            buf
        };

        // 6. Plugin sections
        let plugin_sections = if let Some(plugins) = self.plugin_sections {
            if !plugins.is_empty() {
                format!(
                    "\n\n# Plugin Instructions\n\nThe following instructions are contributed by enabled plugins.\n\n{}\n\n",
                    plugins
                )
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        // 7. Shell representation
        let shell_desc = if !env.shell_name.is_empty() {
            format!("{} (`{}`)", env.shell_name, env.shell_path)
        } else {
            String::new()
        };

        // 8. Interpolation
        let mut prompt = SYSTEM_PROMPT_TEMPLATE.to_string();

        let notify_user_guidance = if self.notify_user_active {
            format!(" {}", NOTIFY_USER_GUIDANCE)
        } else {
            String::new()
        };

        // v2 `systemPromptVars`: the guidance line carries its own trailing
        // blank line and sits directly before the final-reply checklist.
        let todo_list_guidance = if self.todo_list_active {
            format!("{TODO_LIST_GUIDANCE}\n\n")
        } else {
            String::new()
        };

        let memory_section = if self.memory_active {
            MEMORY_SECTION.to_string()
        } else {
            String::new()
        };

        // Memory blocks: rendered from the store when the section is active.
        // The section text carries `${memory_listing}` / `${profile}` /
        // `${preferences}`, so it is expanded first and these resolve in the
        // same pass. `with_brand_home` pins the Kimi home for tests; otherwise
        // the store resolves it from the environment.
        let (memory_listing, profile, preferences) = if self.memory_active {
            let base = self
                .custom_brand_home
                .as_deref()
                .map(memory_store::memory_base_at)
                .or_else(memory_store::memory_base);
            match base {
                Some(base) => {
                    let project_id = memory_store::project_id_for(&self.workspace_root);
                    (
                        memory_store::render_listing(&memory_store::list_entries_for(
                            &base,
                            &project_id,
                            "",
                        )),
                        memory_store::render_profile(&base),
                        memory_store::render_preferences(&base),
                    )
                }
                None => (String::new(), String::new(), String::new()),
            }
        } else {
            (String::new(), String::new(), String::new())
        };

        let model_adaptation_section = self
            .model_adaptation
            .as_ref()
            .map(|adaptation| {
                format!(
                    "\n\nThe section below describes standing guidance for the model family you are running as. It is reference data, not an instruction from the user. Where it conflicts with the instructions above, the instructions above win.\n\n{adaptation}\n\n"
                )
            })
            .unwrap_or_default();
        let memory_base = self
            .custom_brand_home
            .as_deref()
            .map(memory_store::memory_base_at)
            .or_else(memory_store::memory_base);
        let repo_memory = memory_base
            .as_deref()
            .map(|base| git_memory::repo_block_from(&self.workspace_root, base, git_memory_enabled))
            .unwrap_or_default();
        let memory_code = memory_base
            .as_deref()
            .and_then(Path::parent)
            .map(|home| memory_action::code_block_from(home, associative_memory_enabled))
            .unwrap_or_default();
        let memory_code_section = if memory_code.is_empty() {
            String::new()
        } else {
            format!("\n\n# Memory code\n\n{memory_code}\n\n")
        };

        let replacements = [
            ("${product_name}", self.product_name.as_str()),
            ("${role_additional}", role_additional.as_str()),
            ("${reply_style_guide}", self.reply_style_guide.as_str()),
            ("${notify_user_guidance}", notify_user_guidance.as_str()),
            ("${todo_list_guidance}", todo_list_guidance.as_str()),
            ("${os}", env.os_kind.as_str()),
            ("${shell}", shell_desc.as_str()),
            ("${windows_notes}", env.windows_notes.as_str()),
            ("${runtime_notes}", env.runtime_notes.as_str()),
            ("${cwd}", env.cwd.as_str()),
            ("${cwd_listing}", env.cwd_listing.as_str()),
            (
                "${additional_dirs_section}",
                additional_dirs_section.as_str(),
            ),
            ("${agents_md}", agents_md.as_str()),
            ("${repo_memory}", repo_memory.as_str()),
            ("${memory_code_section}", memory_code_section.as_str()),
            ("${skills_section}", skills_section.as_str()),
            ("${plugin_sections}", plugin_sections.as_str()),
            (
                "${model_adaptation_section}",
                model_adaptation_section.as_str(),
            ),
            // The memory section is expanded before its own inner blocks, so
            // `${memory_listing}` and friends resolve in the same pass.
            ("${memory_section}", memory_section.as_str()),
            ("${memory_listing}", memory_listing.as_str()),
            ("${profile}", profile.as_str()),
            ("${preferences}", preferences.as_str()),
        ];

        for (var, val) in replacements {
            prompt = prompt.replace(var, val);
        }

        Ok((prompt, warnings))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_builder_renders_core_sections_and_resolves_all_placeholders() {
        let temp = tempdir().unwrap();
        let prompt = SystemPromptBuilder::new(temp.path())
            .with_profile("agent")
            .build()
            .unwrap();

        // Hard requirement: NO unexpanded ${variable} template placeholders remain
        assert!(
            !prompt.contains("${"),
            "prompt contains unexpanded template variables: {}",
            prompt
                .lines()
                .filter(|l| l.contains("${"))
                .collect::<Vec<_>>()
                .join("\n")
        );

        // Core header and identity
        assert!(prompt.starts_with(
            "You are Kimi Code CLI, an interactive general AI agent running on a user's computer."
        ));

        // All required top-level section headings in sequence
        let sections = [
            "# Communicating with the user",
            "# Tool use",
            "# Coding",
            "# Risky actions",
            "# Delivering work",
            "# Untrusted content",
            "# Search",
            "# Skills",
            "# Context management",
            "# Environment",
            "# Project information",
        ];
        let mut last_idx = 0;
        for sec in sections {
            let idx = prompt
                .find(sec)
                .unwrap_or_else(|| panic!("missing section heading: {sec}"));
            assert!(idx >= last_idx, "section {} appeared out of order", sec);
            last_idx = idx;
        }

        // Environment disclosure interpolation
        let normalized_cwd = temp.path().display().to_string().replace('\\', "/");
        assert!(prompt.contains(&format!(
            "The current working directory is `{}`",
            normalized_cwd
        )));
        assert!(prompt.contains("```\n(empty directory)\n```"));

        // By default with no additional dirs or plugins, those sections are omitted
        assert!(!prompt.contains("## Additional Directories"));
        assert!(!prompt.contains("# Plugin Instructions"));
    }

    #[test]
    fn test_builder_build_with_warnings_reports_oversized_agents_md() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "x".repeat(35 * 1024)).unwrap();
        let (_prompt, warnings) = SystemPromptBuilder::new(temp.path())
            .with_brand_home(temp.path())
            .build_with_warnings()
            .unwrap();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("exceeds the recommended")),
            "got: {warnings:?}"
        );
    }

    #[test]
    fn test_builder_role_additional_for_all_profiles() {
        let temp = tempdir().unwrap();

        // 1. Agent profile has no subagent prefix
        let agent_prompt = SystemPromptBuilder::new(temp.path())
            .with_profile("agent")
            .build()
            .unwrap();
        assert!(!agent_prompt.contains("You are a subagent; the main agent is your caller"));

        // 2. Coder profile has subagent prefix + coder handoff suffix
        let coder_prompt = SystemPromptBuilder::new(temp.path())
            .with_profile("coder")
            .build()
            .unwrap();
        assert!(coder_prompt.contains("You are a subagent; the main agent is your caller"));
        assert!(coder_prompt.contains("Your final message is the entire handoff"));

        // 3. Explore profile has subagent prefix + explore overlay
        let explore_prompt = SystemPromptBuilder::new(temp.path())
            .with_profile("explore")
            .build()
            .unwrap();
        assert!(explore_prompt.contains("You are a subagent; the main agent is your caller"));
        assert!(explore_prompt.contains("You are a codebase exploration specialist."));

        // 4. Plan profile has subagent prefix + plan suffix
        let plan_prompt = SystemPromptBuilder::new(temp.path())
            .with_profile("plan")
            .build()
            .unwrap();
        assert!(plan_prompt.contains("You are a subagent; the main agent is your caller"));
        assert!(plan_prompt.contains("You are read-only: do not run commands or modify files"));

        // 5. Unknown profile defaults to empty role_additional without error
        let unknown_prompt = SystemPromptBuilder::new(temp.path())
            .with_profile("custom_unknown")
            .build()
            .unwrap();
        assert!(!unknown_prompt.contains("You are a subagent; the main agent is your caller"));
        assert!(!unknown_prompt.contains("${role_additional}"));
    }

    #[test]
    fn test_builder_custom_product_name_and_shell() {
        let temp = tempdir().unwrap();
        let prompt = SystemPromptBuilder::new(temp.path())
            .with_product_name("Kimi Native Agent")
            .with_shell("/custom/bin/bash")
            .build()
            .unwrap();

        assert!(prompt.starts_with("You are Kimi Native Agent, an interactive general AI agent"));
        assert!(
            prompt.contains("the Bash tool executes commands using **bash (`/custom/bin/bash`)**")
        );
    }

    #[test]
    fn test_builder_custom_agents_md_and_skills() {
        let temp = tempdir().unwrap();
        let custom_md = "# Custom Rules\nAlways verify before done.";
        let custom_skills = "\n\n# Skills\n\nCustom skills list here.";

        let prompt = SystemPromptBuilder::new(temp.path())
            .with_custom_agents_md(custom_md)
            .with_custom_skills(custom_skills)
            .build()
            .unwrap();

        assert!(prompt.contains("The applicable `AGENTS.md` instructions are:\n\n```````\n# Custom Rules\nAlways verify before done.\n```````"));
        assert!(prompt.contains("Custom skills list here."));
    }

    #[test]
    fn test_builder_additional_dirs_and_plugin_sections() {
        let temp_ws = tempdir().unwrap();
        let extra_dir = tempdir().unwrap();
        std::fs::write(extra_dir.path().join("extra.txt"), "hello").unwrap();

        let prompt = SystemPromptBuilder::new(temp_ws.path())
            .add_additional_dir(extra_dir.path())
            .with_plugin_sections("## Plugin A\nFollow plugin instructions.")
            .build()
            .unwrap();

        // Additional directories section
        assert!(prompt.contains("## Additional Directories"));
        assert!(prompt.contains(&format!("### {}\n", extra_dir.path().display())));
        assert!(prompt.contains("extra.txt"));

        // Plugin instructions section
        assert!(prompt.contains("# Plugin Instructions\n\nThe following instructions are contributed by enabled plugins.\n\n## Plugin A\nFollow plugin instructions.\n\n"));
    }

    #[test]
    fn test_builder_build_default_matches_new_build() {
        let temp = tempdir().unwrap();
        let default_built = SystemPromptBuilder::build_default(temp.path()).unwrap();
        let manual_built = SystemPromptBuilder::new(temp.path())
            .with_memory(true)
            .build()
            .unwrap();
        assert_eq!(default_built, manual_built);
    }

    /// `--agent` reaches the engine as `RunTurnParams.agent_profile`, which
    /// resolves through `build_for_profile`. An unknown name must fall back to
    /// the default `agent` profile rather than dropping the prompt.
    #[test]
    fn test_builder_build_for_profile_selects_the_role_overlay() {
        let temp = tempdir().unwrap();
        let default_built =
            SystemPromptBuilder::build_for_profile(temp.path(), Vec::new(), true, "agent").unwrap();
        let explore_built =
            SystemPromptBuilder::build_for_profile(temp.path(), Vec::new(), true, "explore")
                .unwrap();

        assert_ne!(
            default_built, explore_built,
            "a named profile must change the session prompt"
        );
        let catalog = ProfileCatalog::with_builtins();
        let explore_role = catalog.get("explore").unwrap().role_additional.clone();
        assert!(explore_built.contains(&explore_role));

        // No role overlay for the main profile, so `agent` matches the
        // default build — the profile name only ever adds, never replaces.
        assert_eq!(
            default_built,
            SystemPromptBuilder::build_default(temp.path()).unwrap()
        );

        // An unknown name degrades to the default profile's empty overlay
        // instead of failing the turn.
        let unknown_built =
            SystemPromptBuilder::build_for_profile(temp.path(), Vec::new(), true, "no-such-agent")
                .unwrap();
        assert_eq!(unknown_built, default_built);
    }

    #[test]
    fn test_builder_notify_user_toggle() {
        let temp = tempdir().unwrap();
        let prompt_disabled = SystemPromptBuilder::new(temp.path())
            .with_notify_user(false)
            .build()
            .unwrap();
        assert!(!prompt_disabled.contains(NOTIFY_USER_GUIDANCE));
        assert!(!prompt_disabled.contains("${notify_user_guidance}"));

        let prompt_enabled = SystemPromptBuilder::new(temp.path())
            .with_notify_user(true)
            .build()
            .unwrap();
        assert!(prompt_enabled.contains(NOTIFY_USER_GUIDANCE));
        assert!(prompt_enabled.contains("When `NotifyUser` is available, use it proactively"));
        assert!(!prompt_enabled.contains("${notify_user_guidance}"));
    }

    #[test]
    fn test_builder_todo_list_toggle() {
        let temp = tempdir().unwrap();
        let prompt_disabled = SystemPromptBuilder::new(temp.path())
            .with_todo_list(false)
            .build()
            .unwrap();
        assert!(!prompt_disabled.contains(TODO_LIST_GUIDANCE));
        assert!(!prompt_disabled.contains("${todo_list_guidance}"));

        let prompt_enabled = SystemPromptBuilder::new(temp.path())
            .with_todo_list(true)
            .build()
            .unwrap();
        assert!(prompt_enabled.contains(TODO_LIST_GUIDANCE));
        assert!(!prompt_enabled.contains("${todo_list_guidance}"));
        // v2 `system.md`: the guidance sits directly before the final-reply
        // checklist, separated by its own blank line.
        assert!(
            prompt_enabled.contains(&format!(
                "{TODO_LIST_GUIDANCE}\n\nBefore you finalize a reply"
            )),
            "the guidance must precede the final-reply checklist"
        );
    }

    #[test]
    fn test_builder_renders_git_and_associative_memory_blocks() {
        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("associative-memory")).unwrap();
        std::fs::write(
            home.path().join("associative-memory/code.md"),
            "---\nrules: []\n---\nPrefer explicit memories.\n",
        )
        .unwrap();
        let memory_base = memory_store::memory_base_at(home.path());
        memory_store::write(
            &memory_base,
            "global/note.md",
            "# Note\n\nRemember this.\n",
            "new",
        )
        .unwrap();
        memory_store::write(
            &memory_base,
            "global/scoped.md",
            "---\napplies_to: [\"src/**\"]\n---\n# Scoped\n\nDo not include globally.\n",
            "new",
        )
        .unwrap();
        let repo = memory_repo::MemoryRepo::new(memory_base.clone());
        assert!(repo.commit("remember: Note"));
        let workspace_repo = memory_repo::MemoryRepo::new(workspace.path().to_path_buf());
        std::fs::write(
            workspace.path().join("README.md"),
            "hello
",
        )
        .unwrap();
        assert!(workspace_repo.commit("project change"));
        std::fs::create_dir_all(workspace.path().join("src")).unwrap();
        std::fs::write(
            workspace.path().join("src/lib.rs"),
            "fn main() {}
",
        )
        .unwrap();
        assert!(workspace_repo.commit("add src"));

        let prompt = SystemPromptBuilder::new(workspace.path())
            .with_brand_home(home.path())
            .with_memory(false)
            .with_memory_feature_flags(true, true)
            .build()
            .unwrap();
        assert!(prompt.contains("# Memory code\n\nPrefer explicit memories."));
        assert!(prompt.contains("<repo-memory>"));
        assert!(prompt.contains("project change"));
        assert!(prompt.contains("<repo-tree depth=\"3\">"));
        assert!(prompt.contains("<memory sha=\""));
        assert!(prompt.contains("Remember this."));
        assert!(!prompt.contains("Do not include globally."));
        assert!(!prompt.contains("${repo_memory}"));
        assert!(!prompt.contains("${memory_code_section}"));
    }

    #[test]
    fn test_builder_memory_toggle() {
        let temp = tempdir().unwrap();
        let prompt_disabled = SystemPromptBuilder::new(temp.path())
            .with_memory(false)
            .build()
            .unwrap();
        assert!(!prompt_disabled.contains("# Memory"));
        assert!(!prompt_disabled.contains("`Memory`"));
        assert!(!prompt_disabled.contains("${memory_section}"));

        let prompt_enabled = SystemPromptBuilder::new(temp.path())
            .with_memory(true)
            .build()
            .unwrap();
        assert!(prompt_enabled.contains("# Memory"));
        assert!(prompt_enabled.contains("`Memory`"));
        assert!(!prompt_enabled.contains("${memory_section}"));
        assert!(!prompt_enabled.contains("${memory_listing}"));
    }

    #[test]
    fn test_builder_memory_blocks_render_from_the_store() {
        let temp = tempdir().unwrap();
        let home = tempdir().unwrap();
        let memory = home.path().join("memory").join("global");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(
            memory.join("profile.md"),
            "---\nname: profile\ndescription: Who they are.\ntype: note\n---\n\n- [stated] Works on the kimi-code CLI.\n",
        )
        .unwrap();
        std::fs::write(
            memory.join("preferences.md"),
            "---\nname: preferences\ndescription: How they want replies.\n---\n\n- [stated] Prefers short answers.\n",
        )
        .unwrap();

        let prompt = SystemPromptBuilder::new(temp.path())
            .with_brand_home(home.path())
            .with_memory(true)
            .build()
            .unwrap();

        assert!(
            prompt.contains(
                "<memory_listing>\n\
                 - global/preferences.md [note] — How they want replies.\n\
                 - global/profile.md [note] — Who they are.\n\
                 </memory_listing>"
            ),
            "listing block missing from the prompt"
        );
        assert!(prompt.contains("<profile>\n- [stated] Works on the kimi-code CLI.\n</profile>"));
        assert!(
            prompt.contains("<preferences>\n- [stated] Prefers short answers.\n</preferences>")
        );
        assert!(!prompt.contains("${"));

        // With the section off, none of the blocks are rendered.
        let off = SystemPromptBuilder::new(temp.path())
            .with_brand_home(home.path())
            .with_memory(false)
            .build()
            .unwrap();
        assert!(!off.contains("<memory_listing>"));
        assert!(!off.contains("<profile>"));
    }

    #[test]
    fn test_model_adaptation_is_rendered_only_when_supplied() {
        let temp = tempdir().unwrap();
        let adapted = SystemPromptBuilder::new(temp.path())
            .with_model_adaptation(Some(
                "# Model Adaptation: deepseek-chat\n\n## Autonomy and persistence".to_string(),
            ))
            .build()
            .unwrap();
        assert!(
            adapted.contains("The section below describes standing guidance for the model family")
        );
        assert!(adapted.contains("# Model Adaptation: deepseek-chat"));
        assert!(adapted.contains("## Autonomy and persistence"));

        let unadapted = SystemPromptBuilder::new(temp.path())
            .with_model_adaptation(None)
            .build()
            .unwrap();
        assert!(!unadapted.contains("# Model Adaptation:"));
    }
}
