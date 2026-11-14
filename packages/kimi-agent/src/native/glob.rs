pub const MAX_MATCHES: usize = 100;

pub fn glob_matches_any(globs: &[String], path: &str) -> bool {
    if globs.is_empty() {
        return false;
    }
    let mut builder = globset::GlobSetBuilder::new();
    for g in globs {
        if let Ok(glob) = globset::GlobBuilder::new(g).literal_separator(true).build() {
            builder.add(glob);
        }
    }
    match builder.build() {
        Ok(set) => set.is_match(path),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn globs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn matches_single_pattern() {
        assert!(glob_matches_any(&globs(&["*.ts"]), "main.ts"));
        assert!(!glob_matches_any(&globs(&["*.ts"]), "src/main.ts"));
        assert!(glob_matches_any(&globs(&["**/*.ts"]), "src/main.ts"));
        assert!(!glob_matches_any(&globs(&["*.rs"]), "src/main.ts"));
    }

    #[test]
    fn literal_separator_keeps_star_below_dir_boundary() {
        assert!(!glob_matches_any(&globs(&["src/*.ts"]), "src/a/b.ts"));
        assert!(glob_matches_any(&globs(&["src/**/*.ts"]), "src/a/b.ts"));
    }

    #[test]
    fn empty_pattern_list_never_matches() {
        assert!(!glob_matches_any(&[], "anything.ts"));
    }
}
