pub const DEFAULT_INIT_PROMPT: &str = include_str!("init.md");

pub fn init_completion_reminder(agents_md: &str) -> String {
    let latest = if agents_md.trim().is_empty() {
        "No AGENTS.md content was found after `/init` completed."
    } else {
        agents_md
    };
    format!(
        "The user just ran `/init` slash command.\nThe system has analyzed the codebase and generated an `AGENTS.md` file.\n\nLatest AGENTS.md file content:\n{latest}"
    )
}
