use std::path::{Path, PathBuf};

use super::types::{SkillRoot, SkillSource};

pub use crate::skills::{PluginSkillDir, SkillScanRoots, roots_in_precedence_order};

const USER_BRAND_DIRS: &[&str] = &["skills"];
const USER_GENERIC_DIRS: &[&str] = &[".agents/skills"];
const PROJECT_BRAND_DIRS: &[&str] = &[".kimi-code/skills"];
const PROJECT_GENERIC_DIRS: &[&str] = &[".agents/skills"];

#[derive(Debug, Clone, Copy)]
pub struct SkillRootsOptions {
    pub merge_all_available_skills: bool,
}

impl Default for SkillRootsOptions {
    fn default() -> Self {
        Self { merge_all_available_skills: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSkillRootCandidates {
    pub project_root: String,
    pub candidates: Vec<String>,
}

pub async fn user_roots(
    home_dir: &str,
    os_home_dir: &str,
    options: SkillRootsOptions,
) -> Result<Vec<SkillRoot>, String> {
    let mut roots = Vec::new();
    push_brand_group(
        &mut roots,
        USER_BRAND_DIRS,
        Path::new(home_dir),
        SkillSource::User,
        options.merge_all_available_skills,
        None,
    )?;
    push_first_existing(
        &mut roots,
        USER_GENERIC_DIRS,
        Path::new(os_home_dir),
        SkillSource::User,
        None,
    )?;
    Ok(roots)
}

pub async fn project_roots(
    work_dir: &str,
    options: SkillRootsOptions,
) -> Result<Vec<SkillRoot>, String> {
    let project_root = find_project_root(work_dir);
    let workspace_dir = realpath_or_self(&project_root);
    let mut roots = Vec::new();
    push_brand_group(
        &mut roots,
        PROJECT_BRAND_DIRS,
        &project_root,
        SkillSource::Project,
        options.merge_all_available_skills,
        Some(workspace_dir.clone()),
    )?;
    push_first_existing(
        &mut roots,
        PROJECT_GENERIC_DIRS,
        &project_root,
        SkillSource::Project,
        Some(workspace_dir),
    )?;
    Ok(roots)
}

pub async fn project_roots_for_dirs(
    work_dirs: &[String],
    options: SkillRootsOptions,
) -> Result<Vec<SkillRoot>, String> {
    let mut roots = Vec::new();
    for work_dir in work_dirs {
        for root in project_roots(work_dir, options).await? {
            if !roots.iter().any(|kept: &SkillRoot| kept.path == root.path) {
                roots.push(root);
            }
        }
    }
    Ok(roots)
}

pub async fn project_skill_root_candidates(work_dir: &str) -> Result<ProjectSkillRootCandidates, String> {
    let project_root = realpath_or_self(&find_project_root(work_dir));
    let candidates = PROJECT_BRAND_DIRS
        .iter()
        .chain(PROJECT_GENERIC_DIRS)
        .map(|dir| normalize(Path::new(&project_root).join(dir)))
        .collect();
    Ok(ProjectSkillRootCandidates { project_root, candidates })
}

pub async fn configured_roots(
    dirs: &[String],
    work_dir: &str,
    os_home_dir: &str,
    source: SkillSource,
) -> Result<Vec<SkillRoot>, String> {
    let mut roots = Vec::new();
    for dir in configured_root_candidates(dirs, work_dir, os_home_dir).await? {
        push_existing_root(&mut roots, Path::new(&dir), source, None)?;
    }
    Ok(roots)
}

pub async fn configured_root_candidates(
    dirs: &[String],
    work_dir: &str,
    os_home_dir: &str,
) -> Result<Vec<String>, String> {
    let project_root = find_project_root(work_dir);
    Ok(dirs
        .iter()
        .map(|dir| resolve_configured_dir(dir, &project_root, os_home_dir))
        .collect())
}

fn find_project_root(work_dir: &str) -> PathBuf {
    let work_dir = PathBuf::from(work_dir);
    let start = if work_dir.is_absolute() {
        work_dir
    } else {
        std::env::current_dir().unwrap_or_default().join(work_dir)
    };
    let mut normalized = PathBuf::new();
    for component in start.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    for ancestor in normalized.ancestors() {
        if ancestor.join(".git").exists() {
            return ancestor.to_path_buf();
        }
    }
    normalized
}

fn push_first_existing(
    out: &mut Vec<SkillRoot>,
    dirs: &[&str],
    base: &Path,
    source: SkillSource,
    workspace_dir: Option<String>,
) -> Result<(), String> {
    for dir in dirs {
        if push_existing_root(out, &base.join(dir), source, workspace_dir.clone())? {
            return Ok(());
        }
    }
    Ok(())
}

fn push_brand_group(
    out: &mut Vec<SkillRoot>,
    dirs: &[&str],
    base: &Path,
    source: SkillSource,
    merge_all_available_skills: bool,
    workspace_dir: Option<String>,
) -> Result<(), String> {
    if !merge_all_available_skills {
        return push_first_existing(out, dirs, base, source, workspace_dir);
    }
    for dir in dirs {
        push_existing_root(out, &base.join(dir), source, workspace_dir.clone())?;
    }
    Ok(())
}

fn push_existing_root(
    out: &mut Vec<SkillRoot>,
    dir: &Path,
    source: SkillSource,
    workspace_dir: Option<String>,
) -> Result<bool, String> {
    if !dir.is_dir() {
        return Ok(false);
    }
    let resolved = dir
        .canonicalize()
        .map_err(|error| error.to_string())?
        .to_string_lossy()
        .replace('\\', "/");
    if !out.iter().any(|root| root.path == resolved) {
        out.push(SkillRoot {
            path: resolved,
            source: source.as_str().to_string(),
            workspace_dir,
            plugin: None,
            scan_mode: None,
        });
    }
    Ok(true)
}

fn resolve_configured_dir(dir: &str, project_root: &Path, os_home_dir: &str) -> String {
    if dir == "~" {
        return os_home_dir.to_string();
    }
    if let Some(rest) = dir.strip_prefix("~/") {
        return normalize(PathBuf::from(os_home_dir).join(rest));
    }
    let path = PathBuf::from(dir);
    if path.is_absolute() {
        dir.to_string()
    } else {
        normalize(project_root.join(path))
    }
}

fn realpath_or_self(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalize(path: PathBuf) -> String {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized.to_string_lossy().replace('\\', "/")
}
