//! The `delete` action's validation and non-git result, from v2
//! `app/memory/tools/memoryTool.ts:442-483`.
//!
//! Pure layer only: the store-wiring handler is the next slice. The
//! git-memory branches (`deletedNotCommitted` and the `forget:` commit) are
//! not ported yet and are named in the write layer's module header too.

use crate::i18n::{LocalizedText, i18n_params};
use crate::tools::memory_paths::sanitize_file_name;
use crate::tools::memory_read::{normalize_path, not_found};

/// Why a `delete` call cannot proceed, in v2's check order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteRefusal {
    MissingPath,
    NotFound(String),
    InvalidFilename(String),
}

impl DeleteRefusal {
    pub fn message(&self) -> String {
        match self {
            Self::MissingPath => {
                LocalizedText::new("engine.tools.memory.deletePathRequired").render()
            }
            Self::NotFound(path) => not_found(path),
            Self::InvalidFilename(path) => LocalizedText::with_params(
                "engine.tools.memory.invalidFilename",
                i18n_params!["path" => path],
            )
            .render(),
        }
    }
}

/// The normalized parts v2 derives before unlinking: an optional `.md` suffix
/// is added, and the last path segment must be a plain filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteRequest {
    pub rel_path: String,
    pub file_name: String,
}

/// v2 `normalizePath` followed by `sanitizeFileName` on the last segment
/// (`:446`, `:452-455`). Existence is checked by the store layer, not here.
pub fn prepare_delete(
    path: Option<&str>,
    exists: bool,
) -> Result<DeleteRequest, DeleteRefusal> {
    let Some(path) = path else {
        return Err(DeleteRefusal::MissingPath);
    };
    let rel_path = normalize_path(path);
    if !exists {
        return Err(DeleteRefusal::NotFound(rel_path));
    }
    let last = rel_path.rsplit('/').next().unwrap_or(&rel_path);
    let Some(file_name) = sanitize_file_name(last) else {
        return Err(DeleteRefusal::InvalidFilename(path.to_string()));
    };
    Ok(DeleteRequest {
        rel_path,
        file_name,
    })
}

/// v2 `toolsV2.memory.deleted` (`:470-475`), the git-memory-off result.
pub fn deleted_message(rel_path: &str) -> String {
    LocalizedText::with_params(
        "engine.tools.memory.deleted",
        i18n_params!["path" => rel_path],
    )
    .render()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_path_is_refused_first() {
        assert_eq!(
            prepare_delete(None, true),
            Err(DeleteRefusal::MissingPath)
        );
    }

    #[test]
    fn a_missing_file_is_reported_with_the_normalized_path() {
        assert_eq!(
            prepare_delete(Some("global/gone"), false),
            Err(DeleteRefusal::NotFound("global/gone.md".to_string()))
        );
    }

    #[test]
    fn the_suffix_is_added_and_the_last_segment_is_validated() {
        let request = prepare_delete(Some("projects/abc/note"), true).unwrap();
        assert_eq!(request.rel_path, "projects/abc/note.md");
        assert_eq!(request.file_name, "note.md");
    }

    /// v2 only sanitizes the **last** segment; the store's own path resolution
    /// is what rejects a `..` earlier in the path. This pure layer mirrors v2.
    #[test]
    fn only_the_last_segment_is_sanitized_here() {
        let request = prepare_delete(Some("global/../escape"), true).unwrap();
        assert_eq!(request.file_name, "escape.md");
        assert!(prepare_delete(Some("global/.."), true).is_err());
    }

    #[test]
    fn the_deleted_result_names_the_path() {
        assert!(
            deleted_message("global/a.md").contains("global/a.md"),
            "{}",
            deleted_message("global/a.md")
        );
    }

    #[test]
    fn refusals_render_catalog_sentences() {
        assert!(!DeleteRefusal::MissingPath.message().is_empty());
        assert!(
            DeleteRefusal::NotFound("global/a.md".to_string())
                .message()
                .contains("global/a.md")
        );
        assert!(
            DeleteRefusal::InvalidFilename("x/y".to_string())
                .message()
                .contains("x/y")
        );
    }
}
