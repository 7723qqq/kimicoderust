use chrono::Local;

pub const TOWER_ROOT: &str = ".tower";
pub const COMMS_DIR: &str = ".tower/comms";
pub const INBOX_DIR: &str = ".tower/comms/inbox";
pub const FINDINGS_DIR: &str = ".tower/comms/findings";
pub const REVIEWS_DIR: &str = ".tower/comms/reviews";
pub const MISSIONS_DIR: &str = ".tower/comms/missions";
pub const LOG_DIR: &str = ".tower/comms/log";
pub const WORKTREES_DIR: &str = ".tower/worktrees";

pub const STATE_FILE: &str = ".tower/comms/state.json";
pub const ACTIVITY_LOG: &str = ".tower/comms/log/activity.log";
pub const MISSIONS_INDEX: &str = ".tower/comms/MISSIONS.md";

pub const TOWER_NAME: &str = "tower";
pub const BROADCAST_NAME: &str = "all";

pub const TOWER_ENV_SWITCH: &str = "KIMI_CODE_EXPERIMENTAL_TOWER";
pub const TOWER_CONFIG_KEY: &str = "tower";

pub fn is_reserved_tower_agent_name(name: &str) -> bool {
    name == TOWER_NAME || name == BROADCAST_NAME
}

pub fn date_stamp() -> String {
    Local::now().format("%Y%m%d").to_string()
}

pub fn date_dash() -> String {
    let stamp = date_stamp();
    format!("{}-{}-{}", &stamp[..4], &stamp[4..6], &stamp[6..8])
}

pub fn slugify(text: &str, max_length: usize) -> String {
    let lower = text.to_lowercase();
    let mut slug = String::new();
    let mut pending_dash = false;
    for character in lower.chars() {
        if character.is_ascii_alphanumeric() {
            if pending_dash {
                slug.push('-');
                pending_dash = false;
            }
            slug.push(character);
        } else {
            pending_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    let sliced: String = trimmed.chars().take(max_length).collect();
    let final_slug = sliced.trim_end_matches('-');
    if final_slug.is_empty() {
        "item".to_string()
    } else {
        final_slug.to_string()
    }
}

pub fn has_non_ascii_characters(text: &str) -> bool {
    text.chars().any(|character| !(' '..='~').contains(&character))
}

pub fn target_slug(target: &str) -> String {
    let trimmed = trim_js(target);
    let replaced = if let Some(rest) = trimmed.strip_prefix('#') {
        format!("pr{rest}")
    } else {
        trimmed.to_string()
    };
    slugify(&collapse_target_separators(&replaced), 60)
}

fn collapse_target_separators(text: &str) -> String {
    let mut collapsed = String::new();
    let mut pending_dash = false;
    for character in text.chars() {
        if character == '/' || character == '#' {
            if !pending_dash {
                collapsed.push('-');
                pending_dash = true;
            }
        } else {
            pending_dash = false;
            collapsed.push(character);
        }
    }
    collapsed
}

pub fn inbox_file_name(from: &str, to: &str, subject: &str) -> String {
    format!(
        "{}-{}-{}-{}.md",
        date_stamp(),
        slugify(from, 30),
        slugify(to, 30),
        slugify(subject, 60)
    )
}

pub fn finding_file_name(agent: &str, finding_type: &str, slug: &str) -> String {
    format!(
        "{}-{}-{}-{}.md",
        date_stamp(),
        slugify(agent, 30),
        slugify(finding_type, 12),
        slugify(slug, 60)
    )
}

pub fn review_file_name(target: &str, reviewer: &str, round: u32) -> String {
    format!(
        "review-{}-{}-r{round}.md",
        target_slug(target),
        slugify(reviewer, 30)
    )
}

pub fn mission_file_name(id: &str, slug: &str) -> String {
    format!("{id}-{}.md", slugify(slug, 60))
}

pub fn unique_slug(base: &str, taken: &mut std::collections::HashSet<String>) -> String {
    if taken.insert(base.to_string()) {
        return base.to_string();
    }
    let mut counter = 2u32;
    loop {
        let candidate = format!("{base}-{counter}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        counter += 1;
    }
}

pub fn resolve_tower_repo_root(cwd: &str) -> String {
    let normalized = cwd.replace('\\', "/");
    let marker = format!("/{WORKTREES_DIR}/");
    if let Some(position) = normalized.find(&marker) {
        cwd[..position].to_string()
    } else {
        cwd.to_string()
    }
}

pub fn tower_env_switch() -> Option<bool> {
    std::env::var(TOWER_ENV_SWITCH)
        .ok()
        .map(|value| !value.is_empty() && value != "false" && value != "0")
}

pub fn tower_config_flag(config: &crate::config::KimiConfig) -> Option<bool> {
    match config.experimental.get(TOWER_CONFIG_KEY) {
        Some(crate::config::ExperimentalValue::Bool(value)) => Some(*value),
        Some(crate::config::ExperimentalValue::String(value)) => {
            Some(!value.is_empty() && value != "false")
        }
        None => None,
    }
}

pub fn tower_enabled(env: Option<bool>, config: Option<bool>) -> bool {
    env.or(config).unwrap_or(false)
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}
