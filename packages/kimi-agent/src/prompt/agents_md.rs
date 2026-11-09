//! AGENTS.md cascading and discovery for Kimi Agent system prompts.
//!
//! Discovers and loads user-global (`~/.kimi-code/AGENTS.md`, `~/.agents/AGENTS.md`)
//! and workspace-local (`.kimi-code/AGENTS.md`, `AGENTS.md`, `agents.md`) instructions,
//! wrapping each section with `<!-- From: <path> -->` and enforcing byte warnings.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const AGENTS_MD_RECOMMENDED_MAX_BYTES: usize = 32 * 1024;
pub const AGENTS_MD_PLAIN_NAMES: &[&str] = &["AGENTS.md", "agents.md"];

/// Result of loading and cascading AGENTS.md instruction files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadedAgentsMd {
    pub content: String,
    pub warning: Option<String>,
    pub paths: Vec<String>,
}

/// Discovered single AGENTS.md file.
#[derive(Debug, Clone)]
struct AgentFile {
    path: String,
    content: String,
}

/// Get user home directory in a cross-platform way.
fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Normalize path string with forward slashes.
fn normalize_path_display(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Key used for path deduplication (case-insensitive on Windows).
fn dedupe_key(path_str: &str) -> String {
    if cfg!(target_os = "windows") {
        path_str.to_lowercase()
    } else {
        path_str.to_string()
    }
}

/// Read an instruction file if it is non-empty.
///
/// v2 `readAgentFile` (`profile/context.ts:300-320`): a path that exists but
/// is not a regular file, or that cannot be read, is skipped *and* reported —
/// silently dropping a broken instruction file hides a configuration problem
/// from the user.
fn read_agent_file(path: &Path, warnings: &mut Vec<String>) -> Option<AgentFile> {
    if !path.is_file() {
        if path.symlink_metadata().is_ok() {
            warnings.push(format!(
                "Instruction file at {} exists but is not a readable regular file; skipping.",
                normalize_path_display(path)
            ));
        }
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(AgentFile {
                    path: normalize_path_display(path),
                    content: trimmed.to_string(),
                })
            }
        }
        Err(_) => {
            warnings.push(format!(
                "Instruction file at {} could not be read; skipping.",
                normalize_path_display(path)
            ));
            None
        }
    }
}

/// Every directory from the git work-tree root down to `work_dir`, inclusive
/// (v2 `dirsRootToLeaf` in `loadAgentsMdForRoots`, `profile/context.ts:198-208`).
///
/// A session started in `packages/foo` of a monorepo must see the repository
/// root's `AGENTS.md` as well as its own; before this the root's instructions
/// were silently dropped whenever the work directory was a subdirectory.
/// Without a work-tree root the chain is just `work_dir`, and a work directory
/// outside the root keeps its single directory.
pub(crate) fn instruction_dirs(work_dir: &Path) -> Vec<PathBuf> {
    let single = || vec![work_dir.to_path_buf()];
    let Some(root) = crate::permission::find_git_work_tree(&work_dir.display().to_string()) else {
        return single();
    };
    let Ok(relative) = work_dir.strip_prefix(&root) else {
        return single();
    };
    let mut chain = vec![root.clone()];
    let mut current = root;
    for component in relative.components() {
        current.push(component);
        chain.push(current.clone());
    }
    chain
}

/// Every directory from `root` down to `leaf`, inclusive. `leaf` must be
/// inside `root`; the caller checks that, so a workspace-root chain never
/// escapes into an unrelated path.
pub(crate) fn dirs_root_to_leaf(root: &Path, leaf: &Path) -> Vec<PathBuf> {
    let mut chain = vec![root.to_path_buf()];
    let Ok(relative) = leaf.strip_prefix(root) else {
        return vec![leaf.to_path_buf()];
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        chain.push(current.clone());
    }
    chain
}

/// The AGENTS.md paths embedded in a rendered system prompt (v2
/// `extractAgentsMdPathsFromSystemPrompt`, `profile/context.ts:113-130`).
///
/// The loader prefixes every section with `<!-- From: <path> -->`; the
/// reminder seeds those paths as already disclosed so it never suggests a file
/// the model was handed in its instructions (`seedInjected`,
/// `agentsMdReminderService.ts:98-106, 117-124`).
pub fn extract_agents_md_paths_from_prompt(system_prompt: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for line in system_prompt.lines() {
        let Some(inner) = line
            .trim()
            .strip_prefix("<!-- From: ")
            .and_then(|rest| rest.strip_suffix(" -->"))
        else {
            continue;
        };
        let path = inner.trim();
        if path.is_empty() {
            continue;
        }
        let is_agents_md = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| AGENTS_MD_PLAIN_NAMES.contains(&name));
        if !is_agents_md {
            continue;
        }
        let normalized = path.replace('\\', "/");
        if !paths.iter().any(|seen| seen == &normalized) {
            paths.push(normalized);
        }
    }
    paths
}

/// Load and aggregate all relevant AGENTS.md files for a workspace.
pub fn load_agents_md(workspace_root: &Path, custom_brand_home: Option<&Path>) -> LoadedAgentsMd {
    load_agents_md_with_home(
        workspace_root,
        custom_brand_home,
        user_home_dir().as_deref(),
    )
}

/// [`load_agents_md`] with an explicit home directory, so tests can pin the
/// `~/.agents` fallback instead of reading the machine's real home.
fn load_agents_md_with_home(
    workspace_root: &Path,
    custom_brand_home: Option<&Path>,
    home: Option<&Path>,
) -> LoadedAgentsMd {
    let mut discovered: Vec<AgentFile> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut warnings: Vec<String> = Vec::new();

    let mut collect = |path: PathBuf| -> bool {
        if let Some(file) = read_agent_file(&path, &mut warnings) {
            let key = dedupe_key(&file.path);
            if seen.insert(key) {
                discovered.push(file);
                return true;
            }
        }
        false
    };

    // 1. User brand home: <brandHome ?? ~/.kimi-code>/AGENTS.md
    if let Some(bh) = custom_brand_home {
        collect(bh.join("AGENTS.md"));
    } else if let Some(home) = home {
        collect(home.join(".kimi-code").join("AGENTS.md"));
    }

    // Generic fallbacks: ~/.agents/AGENTS.md. v2 applies these even when a
    // brand home overrides the personal directory (`profile/context.ts:190-196`).
    if let Some(home) = home {
        for name in AGENTS_MD_PLAIN_NAMES {
            if collect(home.join(".agents").join(name)) {
                break;
            }
        }
    }

    // 2. Every directory from the work-tree root down to the workspace root
    //    (v2 `dirsRootToLeaf`): a session started in a subdirectory still sees
    //    the repository root's instructions, root first.
    for dir in instruction_dirs(workspace_root) {
        collect(dir.join(".kimi-code").join("AGENTS.md"));
        for name in AGENTS_MD_PLAIN_NAMES {
            if collect(dir.join(name)) {
                break;
            }
        }
    }

    // Render joined content with provenance headers
    let mut rendered_sections: Vec<String> = Vec::new();
    let mut paths = Vec::new();

    for file in &discovered {
        paths.push(file.path.clone());
        rendered_sections.push(format!("<!-- From: {} -->\n{}", file.path, file.content));
    }

    let content = rendered_sections.join("\n\n");
    let total_bytes = content.len();

    if total_bytes > AGENTS_MD_RECOMMENDED_MAX_BYTES {
        let kb = (total_bytes as f64) / 1024.0;
        let max_kb = (AGENTS_MD_RECOMMENDED_MAX_BYTES as f64) / 1024.0;
        warnings.push(format!(
            "AGENTS.md total {:.1} KB exceeds the recommended {:.0} KB. Large instruction files increase cost and may impact performance; consider trimming.",
            kb, max_kb
        ));
    }

    let warning = if warnings.is_empty() {
        None
    } else {
        Some(warnings.join("\n"))
    };

    LoadedAgentsMd {
        content,
        warning,
        paths,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// v2 `extractAgentsMdPathsFromSystemPrompt`: only AGENTS.md sections
    /// count, duplicates collapse, and other provenance headers are ignored.
    #[test]
    fn test_extract_agents_md_paths_from_prompt() {
        let prompt = "intro\n\
             <!-- From: /repo/AGENTS.md -->\n# Root\n\n\
             <!-- From: /repo/sub/agents.md -->\n# Sub\n\n\
             <!-- From: /repo/src/main.rs -->\nfn main() {}\n\n\
             <!-- From: /repo/AGENTS.md -->\n# Root again\n";
        assert_eq!(
            extract_agents_md_paths_from_prompt(prompt),
            vec![
                "/repo/AGENTS.md".to_string(),
                "/repo/sub/agents.md".to_string()
            ]
        );
        assert!(extract_agents_md_paths_from_prompt("no headers here").is_empty());
    }

    #[test]
    fn test_load_agents_md_empty_workspace() {
        let temp = tempdir().unwrap();
        let loaded = load_agents_md_with_home(temp.path(), Some(temp.path()), Some(temp.path()));
        assert_eq!(
            loaded,
            LoadedAgentsMd {
                content: String::new(),
                warning: None,
                paths: Vec::new(),
            }
        );
    }

    #[test]
    fn test_load_agents_md_ignores_empty_and_whitespace_only_files() {
        let temp_ws = tempdir().unwrap();
        let kimi_code_dir = temp_ws.path().join(".kimi-code");
        std::fs::create_dir(&kimi_code_dir).unwrap();
        std::fs::write(kimi_code_dir.join("AGENTS.md"), "   \n\t\r\n  ").unwrap();
        std::fs::write(temp_ws.path().join("AGENTS.md"), "").unwrap();

        let loaded =
            load_agents_md_with_home(temp_ws.path(), Some(temp_ws.path()), Some(temp_ws.path()));
        assert_eq!(loaded.content, "");
        assert!(loaded.paths.is_empty());
        assert_eq!(loaded.warning, None);
    }

    #[test]
    fn test_load_agents_md_cascades_home_and_workspace() {
        let temp_home = tempdir().unwrap();
        let temp_ws = tempdir().unwrap();

        let home_file = temp_home.path().join("AGENTS.md");
        let ws_file = temp_ws.path().join("AGENTS.md");
        std::fs::write(&home_file, "# Global Rules\nBe nice.").unwrap();
        std::fs::write(&ws_file, "# Local Rules\nUse Rust.").unwrap();

        let loaded = load_agents_md_with_home(
            temp_ws.path(),
            Some(temp_home.path()),
            Some(temp_home.path()),
        );
        let expected_home_path = normalize_path_display(&home_file);
        let expected_ws_path = normalize_path_display(&ws_file);

        assert_eq!(
            loaded.paths,
            vec![expected_home_path.clone(), expected_ws_path.clone()]
        );
        let expected_content = format!(
            "<!-- From: {} -->\n# Global Rules\nBe nice.\n\n<!-- From: {} -->\n# Local Rules\nUse Rust.",
            expected_home_path, expected_ws_path
        );
        assert_eq!(loaded.content, expected_content);
        assert_eq!(loaded.warning, None);
    }

    /// v2 `loadAgentsMdForRoots` walks from the work-tree root down to the work
    /// directory (`dirsRootToLeaf`): a session started in `packages/app` of a
    /// repository still reads the repository's own `AGENTS.md`, root first.
    #[test]
    fn test_load_agents_md_walks_from_the_work_tree_root() {
        let repo = tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let nested = repo.path().join("packages").join("app");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(repo.path().join("AGENTS.md"), "# Root rules").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "# Nested rules").unwrap();
        let brand = tempdir().unwrap();

        let loaded = load_agents_md_with_home(&nested, Some(brand.path()), Some(brand.path()));
        assert_eq!(
            loaded.paths,
            vec![
                normalize_path_display(&repo.path().join("AGENTS.md")),
                normalize_path_display(&nested.join("AGENTS.md")),
            ],
            "the chain runs root first, then the leaf"
        );
        assert!(
            loaded.content.find("# Root rules").unwrap()
                < loaded.content.find("# Nested rules").unwrap(),
            "root instructions must precede the leaf's"
        );
    }

    #[test]
    fn test_load_agents_md_workspace_kimi_code_and_plain_cascade() {
        let temp_ws = tempdir().unwrap();
        let kimi_code_dir = temp_ws.path().join(".kimi-code");
        std::fs::create_dir(&kimi_code_dir).unwrap();

        let sub_agents = kimi_code_dir.join("AGENTS.md");
        let root_agents = temp_ws.path().join("AGENTS.md");
        std::fs::write(&sub_agents, "Specific workspace config").unwrap();
        std::fs::write(&root_agents, "General workspace config").unwrap();

        let empty_home = tempdir().unwrap();
        let loaded = load_agents_md_with_home(
            temp_ws.path(),
            Some(empty_home.path()),
            Some(empty_home.path()),
        );

        let expected_sub_path = normalize_path_display(&sub_agents);
        let expected_root_path = normalize_path_display(&root_agents);

        assert_eq!(
            loaded.paths,
            vec![expected_sub_path.clone(), expected_root_path.clone()]
        );
        let expected_content = format!(
            "<!-- From: {} -->\nSpecific workspace config\n\n<!-- From: {} -->\nGeneral workspace config",
            expected_sub_path, expected_root_path
        );
        assert_eq!(loaded.content, expected_content);
        assert_eq!(loaded.warning, None);
    }

    #[test]
    fn test_load_agents_md_fallback_to_lowercase_agents() {
        let temp_ws = tempdir().unwrap();
        let lower_agents = temp_ws.path().join("agents.md");
        std::fs::write(&lower_agents, "Fallback lowercase config").unwrap();

        let empty_home = tempdir().unwrap();
        let loaded = load_agents_md_with_home(
            temp_ws.path(),
            Some(empty_home.path()),
            Some(empty_home.path()),
        );

        // On case-insensitive filesystems (Windows), checking the first candidate "AGENTS.md"
        // in AGENTS_MD_PLAIN_NAMES succeeds because the filesystem resolves it to the existing file.
        // On case-sensitive filesystems (Linux), "AGENTS.md" does not exist and it falls back to "agents.md".
        let expected_name = if cfg!(target_os = "windows") {
            "AGENTS.md"
        } else {
            "agents.md"
        };
        let expected_path = normalize_path_display(&temp_ws.path().join(expected_name));
        assert_eq!(loaded.paths, vec![expected_path.clone()]);
        assert_eq!(
            loaded.content,
            format!(
                "<!-- From: {} -->\nFallback lowercase config",
                expected_path
            )
        );

        // Deduplication test: when both .kimi-code/AGENTS.md and root AGENTS.md exist,
        // each distinct location is loaded once and not duplicated.
        let upper_agents = temp_ws.path().join("AGENTS.md");
        std::fs::write(&upper_agents, "Primary uppercase config").unwrap();

        let loaded2 = load_agents_md(temp_ws.path(), Some(empty_home.path()));
        let expected_upper_path = normalize_path_display(&upper_agents);
        assert_eq!(loaded2.paths, vec![expected_upper_path.clone()]);
        assert_eq!(
            loaded2.content,
            format!(
                "<!-- From: {} -->\nPrimary uppercase config",
                expected_upper_path
            )
        );
    }

    #[test]
    fn test_load_agents_md_deduplicates_same_path() {
        let temp_ws = tempdir().unwrap();
        let agents_file = temp_ws.path().join("AGENTS.md");
        std::fs::write(&agents_file, "Duplicate test").unwrap();

        // Pass same directory as brand_home and workspace_root
        let loaded =
            load_agents_md_with_home(temp_ws.path(), Some(temp_ws.path()), Some(temp_ws.path()));
        let expected_path = normalize_path_display(&agents_file);

        assert_eq!(loaded.paths, vec![expected_path.clone()]);
        assert_eq!(
            loaded.content,
            format!("<!-- From: {} -->\nDuplicate test", expected_path)
        );
    }

    #[test]
    fn test_load_agents_md_warning_on_exceeded_budget() {
        let temp_ws = tempdir().unwrap();
        let empty_home = tempdir().unwrap();

        // 1. Content exactly within 32 KB budget has no warning
        let safe_content = "x".repeat(30 * 1024);
        std::fs::write(temp_ws.path().join("AGENTS.md"), &safe_content).unwrap();
        let safe_loaded = load_agents_md(temp_ws.path(), Some(empty_home.path()));
        assert_eq!(safe_loaded.warning, None);

        // 2. Content exceeding 32 KB threshold generates exact formatted warning
        let big_content = "a".repeat(35 * 1024);
        std::fs::write(temp_ws.path().join("AGENTS.md"), &big_content).unwrap();

        let loaded = load_agents_md_with_home(
            temp_ws.path(),
            Some(empty_home.path()),
            Some(empty_home.path()),
        );
        let total_bytes = loaded.content.len();
        assert!(total_bytes > AGENTS_MD_RECOMMENDED_MAX_BYTES);

        let kb = (total_bytes as f64) / 1024.0;
        let max_kb = (AGENTS_MD_RECOMMENDED_MAX_BYTES as f64) / 1024.0;
        let expected_warning = format!(
            "AGENTS.md total {:.1} KB exceeds the recommended {:.0} KB. Large instruction files increase cost and may impact performance; consider trimming.",
            kb, max_kb
        );

        assert_eq!(loaded.warning, Some(expected_warning));
    }
}
