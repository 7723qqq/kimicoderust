//! The `write` action's validation and non-git result shape, from v2
//! `app/memory/tools/memoryTool.ts:236-293`.
//!
//! Only the branch that applies when git memory is off: check `path` /
//! `content` / filename, resolve the scope, build the relative path, then
//! render `Memory written to … / title / type`. The `writeNeedsReference`,
//! `writtenNotCommitted` and `writtenCommitted` branches belong to the
//! git-memory subsystem and are not ported yet.

use crate::i18n::{LocalizedText, i18n_params};
use crate::tools::memory_paths::{
    MemoryScope, build_rel_path, detect_type, extract_title, sanitize_file_name,
};

/// Why a `write` call cannot proceed, in v2's check order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteRefusal {
    MissingPath,
    MissingContent,
    InvalidFilename(String),
}

impl WriteRefusal {
    pub fn message(&self) -> String {
        match self {
            Self::MissingPath => {
                LocalizedText::new("engine.tools.memory.writePathRequired").render()
            }
            Self::MissingContent => {
                LocalizedText::new("engine.tools.memory.writeContentRequired").render()
            }
            Self::InvalidFilename(path) => LocalizedText::with_params(
                "engine.tools.memory.invalidFilename",
                i18n_params!["path" => path],
            )
            .render(),
        }
    }
}

/// The parsed, safe parts of a `write` call. `scope` defaults to `project`,
/// matching `memoryTool.ts:244`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRequest {
    pub scope: MemoryScope,
    pub scope_id: String,
    pub file_name: String,
    pub rel_path: String,
    pub title: String,
    pub memory_type: String,
    pub content: String,
}

/// v2's three checks plus `sanitizeFileName` / `buildRelPath`. `scope_id` is
/// supplied by the caller (project identity or session id); this function
/// does no I/O.
pub fn prepare_write(
    path: Option<&str>,
    content: Option<&str>,
    scope: Option<MemoryScope>,
    scope_id: &str,
) -> Result<WriteRequest, WriteRefusal> {
    let Some(path) = path else {
        return Err(WriteRefusal::MissingPath);
    };
    let Some(content) = content else {
        return Err(WriteRefusal::MissingContent);
    };
    let Some(file_name) = sanitize_file_name(path) else {
        return Err(WriteRefusal::InvalidFilename(path.to_string()));
    };
    let scope = scope.unwrap_or(MemoryScope::Project);
    let rel_path = build_rel_path(scope, scope_id, &file_name);
    Ok(WriteRequest {
        scope,
        scope_id: scope_id.to_string(),
        title: extract_title(content, &file_name),
        memory_type: detect_type(content).as_str().to_string(),
        rel_path,
        file_name,
        content: content.to_string(),
    })
}

/// v2 `toolsV2.memory.written` (`:288-293`), the result when git memory is
/// off. Three lines: path, title, type.
pub fn written_message(request: &WriteRequest) -> String {
    LocalizedText::with_params(
        "engine.tools.memory.written",
        i18n_params![
            "path" => request.rel_path,
            "title" => request.title,
            "type" => request.memory_type
        ],
    )
    .render()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_path_or_content_is_refused_in_v2s_order() {
        assert_eq!(
            prepare_write(None, None, None, "id"),
            Err(WriteRefusal::MissingPath)
        );
        assert_eq!(
            prepare_write(Some("a.md"), None, None, "id"),
            Err(WriteRefusal::MissingContent)
        );
    }

    #[test]
    fn an_unsafe_filename_is_refused_before_the_scope_is_resolved() {
        for bad in ["../escape.md", "a/b.md", r"a\b.md", "   "] {
            assert_eq!(
                prepare_write(Some(bad), Some("body"), None, "id"),
                Err(WriteRefusal::InvalidFilename(bad.to_string()))
            );
        }
    }

    #[test]
    fn the_default_scope_is_project_and_the_suffix_is_added() {
        let request = prepare_write(Some("auth"), Some("# Auth\n\nbody"), None, "abc123").unwrap();
        assert_eq!(request.scope, MemoryScope::Project);
        assert_eq!(request.rel_path, "projects/abc123/auth.md");
        assert_eq!(request.title, "Auth");
        assert_eq!(request.memory_type, "note");
    }

    #[test]
    fn an_explicit_scope_chooses_its_own_directory() {
        let global =
            prepare_write(Some("std.md"), Some("## decision\nkeep"), Some(MemoryScope::Global), "")
                .unwrap();
        assert_eq!(global.rel_path, "global/std.md");
        assert_eq!(global.memory_type, "decision");
        let session = prepare_write(
            Some("note"),
            Some("body"),
            Some(MemoryScope::Session),
            "sess-1",
        )
        .unwrap();
        assert_eq!(session.rel_path, "sessions/sess-1/note.md");
    }

    #[test]
    fn the_written_result_names_path_title_and_type() {
        let request =
            prepare_write(Some("auth.md"), Some("# Authentication"), None, "abc123").unwrap();
        let text = written_message(&request);
        assert!(text.contains("projects/abc123/auth.md"), "{text}");
        assert!(text.contains("Authentication"), "{text}");
        assert!(text.contains("note"), "{text}");
    }

    #[test]
    fn refusals_render_the_catalog_sentences() {
        assert!(!WriteRefusal::MissingPath.message().is_empty());
        assert!(!WriteRefusal::MissingContent.message().is_empty());
        assert!(
            WriteRefusal::InvalidFilename("../x".to_string())
                .message()
                .contains("../x")
        );
    }
}
