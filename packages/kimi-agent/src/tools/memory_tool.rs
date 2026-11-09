use std::path::{Path, PathBuf};

use crate::tools::memory_store;

pub struct MemoryContext {
    pub base: PathBuf,
    pub project_id: String,

    pub workspace_root: PathBuf,
}

impl MemoryContext {
    pub fn new(base: PathBuf, workspace_root: &Path) -> Self {
        Self {
            base,
            project_id: memory_store::project_id_for(workspace_root),
            workspace_root: workspace_root.to_path_buf(),
        }
    }

    pub fn resolve(workspace_root: &Path) -> Result<Self, String> {
        let base = memory_store::memory_base().ok_or_else(|| {
            "The memory store is unavailable: no home directory is set (KIMI_CODE_HOME, USERPROFILE, or HOME)."
                .to_string()
        })?;
        Ok(Self::new(base, workspace_root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_project_id_follows_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let a = MemoryContext::new(dir.path().join("memory"), Path::new("G:/kimi/kimi-code"));
        let b = MemoryContext::new(dir.path().join("memory"), Path::new("G:\\kimi\\kimi-code"));
        assert_eq!(a.project_id, b.project_id);
        assert_eq!(a.project_id.len(), 12);
    }
}
