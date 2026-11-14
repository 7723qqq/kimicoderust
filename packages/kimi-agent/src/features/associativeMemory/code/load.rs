use tokio::fs;

use super::super::types::MemoryCode;
use super::parse_code;

pub async fn load_code_file(path: &std::path::Path, defaults: &MemoryCode) -> MemoryCode {
    let Ok(content) = fs::read(path).await else {
        return defaults.clone();
    };
    parse_code(&String::from_utf8_lossy(&content), defaults)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::code::defaults::default_code;
    use crate::features::associativeMemory::types::MemorySource;

    #[tokio::test]
    async fn a_missing_file_returns_the_defaults() {
        let defaults = default_code();
        let loaded = load_code_file(std::path::Path::new("/nonexistent/code.md"), &defaults).await;
        assert_eq!(loaded.rules.len(), defaults.rules.len());
        assert_eq!(loaded.guidance, defaults.guidance);
    }

    #[tokio::test]
    async fn a_real_file_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.md");
        std::fs::write(
            &path,
            "---\nrules:\n  - id: extra\n    pattern: forbidden\n    action: block_write\n---\nGuidance body.\n",
        )
        .unwrap();
        let defaults = default_code();
        let loaded = load_code_file(&path, &defaults).await;
        assert_eq!(loaded.rules.len(), defaults.rules.len() + 1);
        assert_eq!(loaded.guidance, "Guidance body.");
        let _ = MemorySource::User;
    }
}
