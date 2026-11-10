use sha2::{Digest, Sha256};

pub fn encode_workdir_key(work_dir: &str) -> String {
    let normalized = work_dir.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    let base = normalized.split('/').next_back().unwrap_or(normalized);

    let mut slug = String::new();
    for c in base.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            slug.push(c);
        } else {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.len() > 40 {
        slug[..40].trim_matches('-').to_string()
    } else {
        slug
    };
    let final_slug = if slug.is_empty() || slug == "." || slug == ".." {
        "workspace".to_string()
    } else {
        slug
    };

    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let hash12 = &hash[..12];

    format!("wd_{final_slug}_{hash12}")
}
