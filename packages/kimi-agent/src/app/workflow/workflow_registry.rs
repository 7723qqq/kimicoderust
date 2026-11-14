use std::collections::HashMap;
use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;

use super::workflow_types::{WorkflowEntry, WorkflowMeta};

const BUILTIN_SCRIPTS: &[(&str, &str)] = &[
    ("deep-research.js", include_str!("../../workflow/builtin/deep-research.js")),
    ("code-review.js", include_str!("../../workflow/builtin/code-review.js")),
    ("test-generator.js", include_str!("../../workflow/builtin/test-generator.js")),
    ("refactor-planner.js", include_str!("../../workflow/builtin/refactor-planner.js")),
    ("bug-triage.js", include_str!("../../workflow/builtin/bug-triage.js")),
    ("pr-description.js", include_str!("../../workflow/builtin/pr-description.js")),
    ("architecture-review.js", include_str!("../../workflow/builtin/architecture-review.js")),
    ("security-audit.js", include_str!("../../workflow/builtin/security-audit.js")),
    ("migration-planner.js", include_str!("../../workflow/builtin/migration-planner.js")),
];

static META_BODY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)export\s+const\s+meta\s*=\s*\{([\s\S]*?)\}").expect("valid meta regex"));
static FIELD: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(\w+)\s*:\s*["']([^"']*?)["']"#).expect("valid field regex"));
static PHASES: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)phases\s*:\s*\[([\s\S]*?)\]").expect("valid phases regex"));
static PHASE_ITEM: Lazy<Regex> = Lazy::new(|| Regex::new(r#"["']([^"']*?)["']"#).expect("valid phase regex"));

static REGISTRY: Lazy<HashMap<String, WorkflowEntry>> = Lazy::new(|| {
    let mut registry = HashMap::new();
    for (file, script) in BUILTIN_SCRIPTS {
        let meta = parse_meta(script).unwrap_or_else(|| panic!("Built-in workflow {file} has invalid or missing meta"));
        registry.insert(meta.name.clone(), WorkflowEntry {
            meta,
            script: (*script).to_string(),
        });
    }
    registry
});

pub fn list_builtins() -> Vec<WorkflowMeta> {
    let mut metas = REGISTRY.values().map(|entry| entry.meta.clone()).collect::<Vec<_>>();
    metas.sort_by(|left, right| left.name.cmp(&right.name));
    metas
}

pub fn get_builtin(name: &str) -> Option<&'static WorkflowEntry> {
    REGISTRY.get(name)
}

pub async fn resolve_user_workflow(home_dir: &str, name: &str) -> Option<WorkflowEntry> {
    if name.is_empty() || !name.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')) {
        return None;
    }
    let file_path = Path::new(home_dir).join("workflows").join(format!("{name}.js"));
    let script = tokio::fs::read_to_string(file_path).await.ok()?;
    let meta = parse_meta(&script)?;
    Some(WorkflowEntry { meta, script })
}

pub fn parse_meta(script: &str) -> Option<WorkflowMeta> {
    let body = META_BODY.captures(script)?.get(1)?.as_str();
    let mut fields = HashMap::new();
    for capture in FIELD.captures_iter(body) {
        let key = capture.get(1)?;
        let value = capture.get(2)?;
        if !key.as_str().is_empty() && !value.as_str().is_empty() {
            fields.insert(key.as_str().to_string(), value.as_str().to_string());
        }
    }
    let phases = PHASES.captures(body).and_then(|capture| capture.get(1)).map(|items| {
        PHASE_ITEM.captures_iter(items.as_str()).filter_map(|capture| {
            capture.get(1).and_then(|value| {
                if value.as_str().is_empty() {
                    None
                } else {
                    Some(value.as_str().to_string())
                }
            })
        }).collect::<Vec<_>>()
    }).filter(|phases| !phases.is_empty());
    Some(WorkflowMeta {
        name: fields.get("name")?.clone(),
        description: fields.get("description")?.clone(),
        when_to_use: fields.get("whenToUse").cloned(),
        phases,
    })
}
