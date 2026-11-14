use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::Value;

use super::super::catalog::parser::{ParseSkillError, ParseSkillTextOptions, parse_skill_text};
use super::super::catalog::skillDiscovery::{ISkillDiscovery, SkillDiscoveryResult};
use super::super::catalog::types::{
    SkillDefinition, SkillRoot, SkillSource, SkippedSkill, normalize_skill_name,
};

pub use crate::skills::*;

const MAX_SKILL_SCAN_DEPTH: usize = 8;

pub struct EnvironmentSkillDiscovery;

#[async_trait]
impl ISkillDiscovery for EnvironmentSkillDiscovery {
    async fn discover(&self, roots: &[SkillRoot]) -> Result<SkillDiscoveryResult, String> {
        Ok(discover_environment_skills(roots))
    }
}

#[derive(Clone)]
struct SkillFileInput {
    skill_md_path: PathBuf,
    skill_dir_name: String,
    sub_skill_parent_name: Option<String>,
}

#[derive(Default)]
struct DiscoveryState {
    by_discovery_key: IndexMap<String, SkillDefinition>,
    by_skill_path: IndexMap<String, SkillDefinition>,
    seen_variant_keys: HashSet<String>,
    skipped: Vec<SkippedSkill>,
    scanned_directories: Vec<String>,
}

pub fn discover_environment_skills(roots: &[SkillRoot]) -> SkillDiscoveryResult {
    let mut state = DiscoveryState::default();
    for root in roots {
        walk(&PathBuf::from(&root.path), root, true, 0, None, &mut state);
    }
    let mut skills: Vec<_> = state.by_discovery_key.into_values().collect();
    skills.sort_by(|a, b| locale_compare(&a.name, &b.name));
    let mut variants: Vec<_> = state.by_skill_path.into_values().collect();
    variants.sort_by(|a, b| locale_compare(&a.name, &b.name));
    SkillDiscoveryResult {
        skills,
        variants,
        skipped: state.skipped,
        scanned_roots: roots.iter().map(|root| root.path.clone()).collect(),
        scanned_directories: state.scanned_directories,
    }
}

fn register(input: &SkillFileInput, root: &SkillRoot, state: &mut DiscoveryState) -> Option<SkillDefinition> {
    let text = match std::fs::read(&input.skill_md_path) {
        Ok(text) => String::from_utf8_lossy(&text).into_owned(),
        Err(error) => {
            tracing::warn!(path = %input.skill_md_path.display(), %error, "Skipping skill at {} due to unexpected error", input.skill_md_path.display());
            return None;
        }
    };
    let path = input.skill_md_path.to_string_lossy().replace('\\', "/");
    let options = ParseSkillTextOptions {
        skill_md_path: path.clone(),
        skill_dir_name: input.skill_dir_name.clone(),
        source: match root.source.as_str() {
            "project" => SkillSource::Project,
            "user" => SkillSource::User,
            "builtin" => SkillSource::Builtin,
            _ => SkillSource::Extra,
        },
        text,
    };
    let mut skill = match parse_skill_text(&options) {
        Ok(skill) => skill,
        Err(ParseSkillError::UnsupportedType(error)) => {
            state.skipped.push(SkippedSkill {
                path,
                reason: format!("unsupported skill type \"{}\"", error.skill_type),
                skill_type: error.skill_type,
            });
            return None;
        }
        Err(ParseSkillError::Parse(error)) => {
            tracing::warn!(path, reason = %error, "Skipping invalid skill at {path}: {error}");
            return None;
        }
    };
    skill.workspace_dir = root.workspace_dir.clone();
    if let Some(parent) = &input.sub_skill_parent_name {
        skill.name = qualify_sub_skill_name(parent, &skill.name);
        skill.metadata.insert("isSubSkill".to_string(), Value::Bool(true));
    }
    skill.plugin = root.plugin.clone();
    let key = match &root.plugin {
        None => normalize_skill_name(&skill.name),
        Some(plugin) => format!("{}\0{}", plugin.id, normalize_skill_name(&skill.name)),
    };
    state.by_discovery_key.entry(key.clone()).or_insert_with(|| skill.clone());
    let variant_key = format!("{key}\0{}", root.workspace_dir.as_deref().unwrap_or(""));
    if !state.seen_variant_keys.contains(&variant_key) && !state.by_skill_path.contains_key(&skill.path) {
        state.seen_variant_keys.insert(variant_key);
        state.by_skill_path.insert(skill.path.clone(), skill.clone());
    }
    Some(skill)
}

fn walk(
    dir_path: &Path,
    root: &SkillRoot,
    is_top_level: bool,
    depth: usize,
    sub_skill_parent_name: Option<String>,
    state: &mut DiscoveryState,
) {
    if depth > MAX_SKILL_SCAN_DEPTH {
        return;
    }
    if root.scan_mode.as_deref() == Some("root-skill-only") {
        let skill_md_path = dir_path.join("SKILL.md");
        if skill_md_path.is_file() {
            register(
                &SkillFileInput {
                    skill_md_path,
                    skill_dir_name: dir_path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                    sub_skill_parent_name,
                },
                root,
                state,
            );
        }
        return;
    }
    let mut entries: Vec<_> = match std::fs::read_dir(dir_path) {
        Ok(entries) => entries.flatten().collect(),
        Err(error) => {
            if !matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) {
                tracing::warn!(path = %dir_path.display(), %error, "Unable to scan skills in {}", dir_path.display());
            }
            return;
        }
    };
    entries.sort_by(|left, right| {
        left.file_name().to_string_lossy().encode_utf16().cmp(right.file_name().to_string_lossy().encode_utf16())
    });
    state.scanned_directories.push(dir_path.to_string_lossy().replace('\\', "/"));

    let mut directory_skills = Vec::new();
    let mut subdirs = Vec::new();
    for entry in &entries {
        let entry_path = entry.path();
        let directory = entry_path.is_dir();
        let name = entry.file_name().to_string_lossy().into_owned();
        let skill = directory && entry_path.join("SKILL.md").is_file();
        if skill {
            directory_skills.push(name.clone());
        }
        if name == "node_modules" || name.starts_with('.') {
            continue;
        }
        if directory {
            subdirs.push(name);
        }
    }

    let mut allowed_sub_skill_bundles = HashMap::new();
    for entry in &directory_skills {
        let skill = register(
            &SkillFileInput {
                skill_md_path: dir_path.join(entry).join("SKILL.md"),
                skill_dir_name: entry.clone(),
                sub_skill_parent_name: sub_skill_parent_name.clone(),
            },
            root,
            state,
        );
        if let Some(skill) = skill.filter(has_sub_skill_enabled) {
            allowed_sub_skill_bundles.insert(entry.clone(), skill.name);
        }
    }

    if is_top_level {
        if root.plugin.is_some() {
            let skill_md_path = dir_path.join("SKILL.md");
            if skill_md_path.is_file() {
                register(
                    &SkillFileInput {
                        skill_md_path,
                        skill_dir_name: dir_path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                        sub_skill_parent_name: sub_skill_parent_name.clone(),
                    },
                    root,
                    state,
                );
            }
        }
        for entry in &entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_file = entry.file_type().map(|kind| kind.is_file()).unwrap_or(false);
            if !is_file
                || !name.ends_with(".md")
                || name == "SKILL.md"
                || directory_skills.contains(&name[..name.len() - 3].to_string())
            {
                continue;
            }
            register(
                &SkillFileInput {
                    skill_md_path: entry.path(),
                    skill_dir_name: name[..name.len() - 3].to_string(),
                    sub_skill_parent_name: sub_skill_parent_name.clone(),
                },
                root,
                state,
            );
        }
    }

    for entry in subdirs {
        if directory_skills.contains(&entry) && !allowed_sub_skill_bundles.contains_key(&entry) {
            continue;
        }
        walk(
            &dir_path.join(&entry),
            root,
            false,
            depth + 1,
            allowed_sub_skill_bundles
                .get(&entry)
                .cloned()
                .or_else(|| sub_skill_parent_name.clone()),
            state,
        );
    }
}

fn qualify_sub_skill_name(parent_name: &str, skill_name: &str) -> String {
    if skill_name == parent_name || skill_name.starts_with(&format!("{parent_name}.")) {
        skill_name.to_string()
    } else {
        format!("{parent_name}.{skill_name}")
    }
}

fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    let script = format!(
        "String((function (left, right) {{ return left.localeCompare(right); }})({}, {}))",
        serde_json::to_string(left).unwrap(),
        serde_json::to_string(right).unwrap()
    );
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| ctx.eval::<String, _>(script))
            })
        })
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .map(|value| value.cmp(&0))
        .unwrap_or_else(|| left.encode_utf16().cmp(right.encode_utf16()))
}

fn has_sub_skill_enabled(skill: &SkillDefinition) -> bool {
    let direct = skill.metadata.get("has-sub-skill").and_then(Value::as_bool) == Some(true)
        || skill.metadata.get("hasSubSkill").and_then(Value::as_bool) == Some(true);
    let nested = skill
        .metadata
        .get("metadata")
        .and_then(Value::as_object)
        .is_some_and(|metadata| {
            metadata.get("has-sub-skill").and_then(Value::as_bool) == Some(true)
                || metadata.get("hasSubSkill").and_then(Value::as_bool) == Some(true)
        });
    direct || nested
}
