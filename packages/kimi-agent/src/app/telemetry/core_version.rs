use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

const PACKAGE_NAME: &str = "@moonshot-ai/agent-core-v2";
const UNKNOWN_VERSION: &str = "unknown";
const MAX_WALK_UP: usize = 8;

static CACHED_CORE_VERSION: OnceLock<String> = OnceLock::new();

#[derive(Deserialize)]
struct PackageManifest {
    name: Option<String>,
    version: Option<String>,
}

pub fn resolve_core_version() -> String {
    CACHED_CORE_VERSION.get_or_init(walk_for_package_version).clone()
}

fn walk_for_package_version() -> String {
    walk_for_package_version_inner().unwrap_or_else(|| UNKNOWN_VERSION.to_string())
}

fn walk_for_package_version_inner() -> Option<String> {
    let source_dir = Path::new(file!()).parent().unwrap_or_else(|| Path::new("."));
    let mut dir = if source_dir.is_absolute() {
        source_dir.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(source_dir)
    };
    for _ in 0..MAX_WALK_UP {
        let candidate = dir.join("package.json");
        if candidate.exists() {
            let text = std::fs::read_to_string(candidate).ok()?;
            let package = serde_json::from_str::<PackageManifest>(&text).ok()?;
            if package.name.as_deref() == Some(PACKAGE_NAME)
                && let Some(version) = package.version
            {
                return Some(version);
            }
        }
        let Some(parent) = dir.parent() else {
            break;
        };
        if parent == dir {
            break;
        }
        dir = PathBuf::from(parent);
    }
    Some(UNKNOWN_VERSION.to_string())
}
