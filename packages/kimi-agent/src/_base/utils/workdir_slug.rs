use once_cell::sync::Lazy;
use regex::Regex;
use sha2::{Digest, Sha256};

const MAX_WORKDIR_SLUG_LENGTH: usize = 40;
pub const WORKDIR_KEY_PREFIX: &str = "wd_";
const HASH_LENGTH: usize = 12;

static INVALID_SLUG_CHARS: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-z0-9._-]+").unwrap());
static EDGE_DASHES: Lazy<Regex> = Lazy::new(|| Regex::new(r"^-+|-+$").unwrap());
static TRAILING_SLASHES: Lazy<Regex> = Lazy::new(|| Regex::new(r"/+$").unwrap());
static WIN_SHAPED: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(?:[A-Za-z]:[\\/]|\\\\|//)").unwrap());

pub fn slugify_work_dir_name(name: &str) -> String {
    let lowered = name.to_lowercase();
    let replaced = INVALID_SLUG_CHARS.replace_all(&lowered, "-");
    let trimmed = EDGE_DASHES.replace_all(&replaced, "");
    let sliced = trimmed.chars().take(MAX_WORKDIR_SLUG_LENGTH).collect::<String>();
    let slug = EDGE_DASHES.replace_all(&sliced, "").to_string();
    if slug.is_empty() || slug == "." || slug == ".." {
        "workspace".to_string()
    } else {
        slug
    }
}

pub fn encode_work_dir_key(work_dir: &str) -> String {
    let slashed = work_dir.replace('\\', "/");
    let normalized = TRAILING_SLASHES.replace(&slashed, "").to_string();
    let base = normalized.split('/').next_back().unwrap_or(&normalized);
    let slug = slugify_work_dir_name(base);
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    format!("{WORKDIR_KEY_PREFIX}{slug}_{}", &hash[..HASH_LENGTH])
}

pub fn workspace_root_key(root: &str) -> String {
    let slashed = root.replace('\\', "/");
    let shaped = WIN_SHAPED.is_match(&slashed);
    let normalized = TRAILING_SLASHES.replace(&slashed, "").to_string();
    if shaped {
        normalized.to_lowercase()
    } else {
        normalized
    }
}

pub fn is_work_dir_key(id: &str) -> bool {
    id.starts_with(WORKDIR_KEY_PREFIX)
}
