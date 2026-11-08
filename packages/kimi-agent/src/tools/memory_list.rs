//! The `list` action's filtering and rendering, from v2
//! `app/memory/tools/memoryTool.ts:411-440`.
//!
//! Pure layer only: the caller supplies the store's paths plus an optional
//! per-path entry (title and type). The next slice wires it to
//! `memory_store::list_entries_for`.

use crate::i18n::{LocalizedText, i18n_params};
use crate::tools::memory_paths::MemoryScope;

/// One path's optional metadata, the Rust shape of v2's `store.get(p)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub path: String,
    pub title: Option<String>,
    pub memory_type: Option<String>,
}

/// v2's scope prefix (`:414-421`). `None` means no filter — every scope of
/// every project and session is listed.
pub fn scope_prefix(
    scope: Option<MemoryScope>,
    project_id: &str,
    session_id: &str,
) -> Option<String> {
    match scope {
        None => None,
        Some(MemoryScope::Global) => Some("global/".to_string()),
        Some(MemoryScope::Project) => Some(format!("projects/{project_id}/")),
        Some(MemoryScope::Session) => Some(format!("sessions/{session_id}/")),
    }
}

/// v2 `toolsV2.memory.noFilesFound`.
pub fn no_files_found() -> String {
    LocalizedText::new("engine.tools.memory.noFilesFound").render()
}

/// The result body: a header with a trailing newline, then one sorted line
/// per path. A path with no entry renders as just `  <path>` (`:436`).
pub fn render_list(entries: &[ListEntry], prefix: Option<&str>) -> String {
    let mut paths: Vec<&ListEntry> = entries.iter().collect();
    if let Some(prefix) = prefix {
        paths.retain(|entry| entry.path.starts_with(prefix));
    }
    if paths.is_empty() {
        return no_files_found();
    }
    paths.sort_by(|a, b| a.path.cmp(&b.path));
    let mut lines = vec![
        LocalizedText::with_params(
            "engine.tools.memory.listHeader",
            i18n_params!["count" => paths.len()],
        )
        .render(),
    ];
    for entry in paths {
        match (&entry.title, &entry.memory_type) {
            (Some(title), Some(memory_type)) => {
                lines.push(format!("  {} — {} [{}]", entry.path, title, memory_type));
            }
            _ => lines.push(format!("  {}", entry.path)),
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, title: &str, memory_type: &str) -> ListEntry {
        ListEntry {
            path: path.to_string(),
            title: Some(title.to_string()),
            memory_type: Some(memory_type.to_string()),
        }
    }

    #[test]
    fn scope_prefixes_follow_v2s_layout() {
        assert_eq!(scope_prefix(None, "p", "s"), None);
        assert_eq!(
            scope_prefix(Some(MemoryScope::Global), "p", "s"),
            Some("global/".to_string())
        );
        assert_eq!(
            scope_prefix(Some(MemoryScope::Project), "p", "s"),
            Some("projects/p/".to_string())
        );
        assert_eq!(
            scope_prefix(Some(MemoryScope::Session), "p", "s"),
            Some("sessions/s/".to_string())
        );
    }

    #[test]
    fn an_empty_list_uses_v2s_refusal() {
        assert_eq!(render_list(&[], None), no_files_found());
    }

    #[test]
    fn the_header_counts_and_lines_are_sorted() {
        let entries = vec![
            entry("projects/p/z.md", "Z", "note"),
            entry("global/a.md", "A", "decision"),
        ];
        let rendered = render_list(&entries, None);
        assert!(rendered.starts_with("Memory files (2):\n"), "{rendered}");
        assert!(rendered.find("global/a.md").unwrap() < rendered.find("projects/p/z.md").unwrap());
        assert!(rendered.contains("  global/a.md — A [decision]"));
    }

    #[test]
    fn a_scope_prefix_filters_before_counting() {
        let entries = vec![
            entry("global/a.md", "A", "note"),
            entry("projects/p/b.md", "B", "note"),
        ];
        let rendered = render_list(&entries, Some("projects/p/"));
        assert!(rendered.contains("Memory files (1):"));
        assert!(rendered.contains("projects/p/b.md"));
        assert!(!rendered.contains("global/a.md"));
    }

    #[test]
    fn a_path_without_metadata_renders_bare() {
        let entries = vec![ListEntry {
            path: "global/raw.md".to_string(),
            title: None,
            memory_type: None,
        }];
        let rendered = render_list(&entries, None);
        assert!(rendered.contains("  global/raw.md\n") || rendered.ends_with("  global/raw.md"));
        assert!(!rendered.contains(" — "));
    }
}
