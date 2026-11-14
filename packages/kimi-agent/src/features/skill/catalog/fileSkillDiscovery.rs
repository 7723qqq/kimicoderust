use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use indexmap::{IndexMap, IndexSet};
use serde_json::Value;

use crate::_base::log::log::ILogService;

use super::parser::{ParseSkillError, ParseSkillTextOptions, parse_skill_text};
use super::skillDiscovery::{ISkillDiscovery, SkillDiscoveryResult};
use super::types::{
    SkillDefinition, SkillRoot, SkillSource, SkippedSkill, normalize_skill_name,
};

pub use crate::skills::*;

pub const MAX_SKILL_SCAN_DEPTH: usize = 8;

pub type SkillDiscoveryWarn = Arc<dyn Fn(&str, Option<Value>) + Send + Sync>;

pub fn is_skill_scan_excluded_entry(entry_name: &str) -> bool {
    entry_name == "node_modules" || entry_name.starts_with('.')
}

pub struct FileSkillDiscovery {
    log: Arc<dyn ILogService>,
}

impl FileSkillDiscovery {
    pub fn new(log: Arc<dyn ILogService>) -> Self {
        Self { log }
    }
}

#[async_trait]
impl ISkillDiscovery for FileSkillDiscovery {
    async fn discover(&self, roots: &[SkillRoot]) -> Result<SkillDiscoveryResult, String> {
        let log = self.log.clone();
        let warn: SkillDiscoveryWarn = Arc::new(move |message, payload| {
            log.warn(message, payload);
        });
        Ok(discover_file_skills(roots, Some(warn)).await)
    }
}

#[derive(Default)]
struct DiscoveryState {
    by_discovery_key: IndexMap<String, SkillDefinition>,
    by_skill_path: IndexMap<String, SkillDefinition>,
    seen_variant_keys: HashSet<String>,
    skipped: Vec<SkippedSkill>,
    scanned_directories: Vec<String>,
}

pub async fn discover_file_skills(
    roots: &[SkillRoot],
    warn: Option<SkillDiscoveryWarn>,
) -> SkillDiscoveryResult {
    let mut state = DiscoveryState::default();
    for root in roots {
        walk_skill_dir(
            PathBuf::from(&root.path),
            root,
            true,
            0,
            None,
            &mut state,
            warn.as_ref(),
        )
        .await;
    }
    SkillDiscoveryResult {
        skills: sort_skills(state.by_discovery_key.into_values().collect()),
        variants: sort_skills(state.by_skill_path.into_values().collect()),
        skipped: state.skipped,
        scanned_roots: roots.iter().map(|root| root.path.clone()).collect(),
        scanned_directories: state.scanned_directories,
    }
}

fn walk_skill_dir<'a>(
    dir_path: PathBuf,
    root: &'a SkillRoot,
    is_top_level: bool,
    depth: usize,
    sub_skill_parent_name: Option<String>,
    state: &'a mut DiscoveryState,
    warn: Option<&'a SkillDiscoveryWarn>,
) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        if depth > MAX_SKILL_SCAN_DEPTH {
            return;
        }

        if root.scan_mode.as_deref() == Some("root-skill-only") {
            let root_skill_md = dir_path.join("SKILL.md");
            if is_file(&root_skill_md).await {
                parse_and_register(
                    &root_skill_md,
                    &basename(&dir_path),
                    root,
                    None,
                    state,
                    warn,
                )
                .await;
            }
            return;
        }

        let mut entries = match read_dir_names(&dir_path).await {
            Some(names) => names,
            None => return,
        };
        entries.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        state.scanned_directories.push(slash_path(&dir_path));

        let mut directory_skills: IndexSet<String> = IndexSet::new();
        let mut subdirs: Vec<String> = Vec::new();
        for entry in &entries {
            let entry_path = dir_path.join(entry);
            if is_file(&entry_path.join("SKILL.md")).await {
                directory_skills.insert(entry.clone());
            }
            if is_skill_scan_excluded_entry(entry) {
                continue;
            }
            if is_dir(&entry_path).await {
                subdirs.push(entry.clone());
            }
        }

        let mut allowed_sub_skill_bundles: IndexMap<String, String> = IndexMap::new();
        for entry in &directory_skills {
            let skill = parse_and_register(
                &dir_path.join(entry).join("SKILL.md"),
                entry,
                root,
                sub_skill_parent_name.as_deref(),
                state,
                warn,
            )
            .await;
            if let Some(skill) = skill.filter(has_sub_skill_enabled) {
                allowed_sub_skill_bundles.insert(entry.clone(), skill.name);
            }
        }

        if is_top_level {
            if root.plugin.is_some() {
                let root_skill_md = dir_path.join("SKILL.md");
                if is_file(&root_skill_md).await {
                    parse_and_register(
                        &root_skill_md,
                        &basename(&dir_path),
                        root,
                        None,
                        state,
                        warn,
                    )
                    .await;
                }
            }

            for entry in &entries {
                if !entry.ends_with(".md") {
                    continue;
                }
                if entry == "SKILL.md" {
                    continue;
                }
                let skill_name = entry[..entry.len() - ".md".len()].to_string();
                if directory_skills.contains(&skill_name) {
                    continue;
                }
                let skill_md_path = dir_path.join(entry);
                if !is_file(&skill_md_path).await {
                    continue;
                }
                parse_and_register(&skill_md_path, &skill_name, root, None, state, warn).await;
            }
        }

        for entry in subdirs {
            if directory_skills.contains(&entry) && !allowed_sub_skill_bundles.contains_key(&entry)
            {
                continue;
            }
            let allowed_sub_skill_parent_name = allowed_sub_skill_bundles
                .get(&entry)
                .cloned()
                .or_else(|| sub_skill_parent_name.clone());
            walk_skill_dir(
                dir_path.join(&entry),
                root,
                false,
                depth + 1,
                allowed_sub_skill_parent_name,
                state,
                warn,
            )
            .await;
        }
    })
}

async fn parse_and_register(
    skill_md_path: &Path,
    skill_dir_name: &str,
    root: &SkillRoot,
    sub_skill_parent_name: Option<&str>,
    state: &mut DiscoveryState,
    warn: Option<&SkillDiscoveryWarn>,
) -> Option<SkillDefinition> {
    let path = slash_path(skill_md_path);
    let text = match tokio::fs::read(skill_md_path).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => {
            warn_skill(warn, &format!("Skipping skill at {path} due to unexpected error"));
            return None;
        }
    };
    let mut skill = match parse_skill_text(&ParseSkillTextOptions {
        skill_md_path: path.clone(),
        skill_dir_name: skill_dir_name.to_string(),
        source: skill_source_of(root),
        text,
    }) {
        Ok(skill) => skill,
        Err(ParseSkillError::UnsupportedType(error)) => {
            state.skipped.push(SkippedSkill {
                path,
                skill_type: error.skill_type.clone(),
                reason: format!("unsupported skill type \"{}\"", error.skill_type),
            });
            return None;
        }
        Err(ParseSkillError::Parse(error)) => {
            warn_skill(warn, &format!("Skipping invalid skill at {path}: {error}"));
            return None;
        }
    };
    skill.workspace_dir = root.workspace_dir.clone();
    if let Some(parent_name) = sub_skill_parent_name {
        skill.name = qualify_sub_skill_name(parent_name, &skill.name);
        skill
            .metadata
            .insert("isSubSkill".to_string(), Value::Bool(true));
    }
    skill.plugin = root.plugin.clone();
    let key = skill_discovery_key(root, &skill.name);
    state
        .by_discovery_key
        .entry(key.clone())
        .or_insert_with(|| skill.clone());
    let variant_key = format!("{key}\0{}", root.workspace_dir.as_deref().unwrap_or(""));
    if !state.seen_variant_keys.contains(&variant_key)
        && !state.by_skill_path.contains_key(&skill.path)
    {
        state.seen_variant_keys.insert(variant_key);
        state.by_skill_path.insert(skill.path.clone(), skill.clone());
    }
    Some(skill)
}

fn skill_source_of(root: &SkillRoot) -> SkillSource {
    match root.source.as_str() {
        "project" => SkillSource::Project,
        "user" => SkillSource::User,
        "builtin" => SkillSource::Builtin,
        _ => SkillSource::Extra,
    }
}

fn skill_discovery_key(root: &SkillRoot, name: &str) -> String {
    let normalized_name = normalize_skill_name(name);
    match &root.plugin {
        None => normalized_name,
        Some(plugin) => format!("{}\0{}", plugin.id, normalized_name),
    }
}

fn sort_skills(skills: Vec<SkillDefinition>) -> Vec<SkillDefinition> {
    let mut skills = skills;
    skills.sort_by(|left, right| locale_compare(&left.name, &right.name));
    skills
}

fn qualify_sub_skill_name(parent_name: &str, skill_name: &str) -> String {
    if skill_name == parent_name || skill_name.starts_with(&format!("{parent_name}.")) {
        return skill_name.to_string();
    }
    format!("{parent_name}.{skill_name}")
}

fn has_sub_skill_enabled(skill: &SkillDefinition) -> bool {
    let nested_flag = skill
        .metadata
        .get("metadata")
        .and_then(Value::as_object)
        .is_some_and(|nested| {
            nested.get("has-sub-skill").and_then(Value::as_bool) == Some(true)
                || nested.get("hasSubSkill").and_then(Value::as_bool) == Some(true)
        });
    skill.metadata.get("has-sub-skill").and_then(Value::as_bool) == Some(true)
        || skill.metadata.get("hasSubSkill").and_then(Value::as_bool) == Some(true)
        || nested_flag
}

fn warn_skill(warn: Option<&SkillDiscoveryWarn>, message: &str) {
    if let Some(warn) = warn {
        warn(message, None);
    }
}

fn basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

async fn is_dir(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

async fn read_dir_names(dir_path: &Path) -> Option<Vec<String>> {
    let mut reader = tokio::fs::read_dir(dir_path).await.ok()?;
    let mut names = Vec::new();
    loop {
        match reader.next_entry().await {
            Ok(Some(entry)) => names.push(entry.file_name().to_string_lossy().into_owned()),
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    Some(names)
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    let script = format!(
        "String((function (left, right) {{ return left.localeCompare(right); }})({}, {}))",
        serde_json::to_string(left).unwrap(),
        serde_json::to_string(right).unwrap()
    );
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime)
                .and_then(|context| context.with(|ctx| ctx.eval::<String, _>(script)))
        })
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .map(|value| value.cmp(&0))
        .unwrap_or_else(|| left.encode_utf16().cmp(right.encode_utf16()))
}
