use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveConfigPathInput {
    pub home_dir: Option<String>,
    pub config_path: Option<String>,
}

pub fn resolve_kimi_home(
    home_dir: Option<&str>,
    env: Option<&HashMap<String, String>>,
    os_home_dir: Option<&str>,
) -> String {
    let default_env;
    let env = match env {
        Some(env) => env,
        None => {
            default_env = process_env();
            &default_env
        }
    };
    let default_os_home_dir;
    let os_home_dir = match os_home_dir {
        Some(home_dir) => home_dir,
        None => {
            default_os_home_dir = os_home_dir();
            &default_os_home_dir
        }
    };
    if let Some(home_dir) = home_dir {
        return home_dir.to_string();
    }
    if let Some(home_dir) = env.get("KIMI_CODE_HOME") {
        return home_dir.clone();
    }
    join(os_home_dir, ".kimi-code")
}

pub fn resolve_config_path(input: ResolveConfigPathInput) -> String {
    let ResolveConfigPathInput {
        home_dir,
        config_path,
    } = input;
    config_path.unwrap_or_else(|| {
        join(
            &resolve_kimi_home(home_dir.as_deref(), None, None),
            "config.toml",
        )
    })
}

pub fn ensure_kimi_home(home_dir: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(home_dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(home_dir)
    }
}

pub fn join(base: &str, segment: &str) -> String {
    pathe::join(base, segment)
}

fn process_env() -> HashMap<String, String> {
    std::env::vars_os()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

fn os_home_dir() -> String {
    std::env::home_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

mod pathe {
    pub fn is_absolute(path: &str) -> bool {
        let bytes: Vec<char> = path.chars().collect();
        if bytes.is_empty() {
            return false;
        }
        if matches!(bytes[0], '/' | '\\') {
            if bytes.len() == 1 || !matches!(bytes[1], '/' | '\\') {
                return true;
            }
            return bytes.len() == 2 || bytes[2] != '.';
        }
        bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == ':'
            && matches!(bytes[2], '/' | '\\')
    }

    fn is_unc(path: &str) -> bool {
        let bytes: Vec<char> = path.chars().collect();
        bytes.len() >= 2 && matches!(bytes[0], '/' | '\\') && matches!(bytes[1], '/' | '\\')
    }

    fn normalize_windows_path(input: &str) -> String {
        if input.is_empty() {
            return String::new();
        }
        let replaced: Vec<char> = input
            .chars()
            .map(|character| if character == '\\' { '/' } else { character })
            .collect();
        let mut out = replaced.iter().collect::<String>();
        if replaced.len() >= 2 && replaced[0].is_ascii_alphabetic() && replaced[1] == ':' {
            let upper = replaced[0].to_ascii_uppercase();
            out = format!("{upper}{}", &out[replaced[0].len_utf8()..]);
        }
        out
    }

    fn is_drive_letter(path: &str) -> bool {
        let chars: Vec<char> = path.chars().collect();
        chars.len() == 2 && chars[0].is_ascii_alphabetic() && chars[1] == ':'
    }

    fn normalize_string(path: &str, allow_above_root: bool) -> String {
        let chars: Vec<char> = path.chars().collect();
        let mut res = String::new();
        let mut last_segment_length = 0usize;
        let mut last_slash: isize = -1;
        let mut dots: isize = 0;
        let mut previous: Option<char> = None;
        let mut index = 0usize;
        while index <= chars.len() {
            let current = if index < chars.len() {
                chars[index]
            } else if previous == Some('/') {
                break;
            } else {
                '/'
            };
            if current == '/' {
                if last_slash == index as isize - 1 || dots == 1 {
                } else if dots == 2 {
                    let ends_with_dotdot = res.len() >= 2
                        && last_segment_length == 2
                        && res.ends_with('.')
                        && res.chars().rev().nth(1) == Some('.');
                    if !ends_with_dotdot {
                        if res.chars().count() > 2 {
                            if let Some(slash_index) = res.rfind('/') {
                                res.truncate(slash_index);
                                last_segment_length = res
                                    .chars()
                                    .count()
                                    .saturating_sub(1)
                                    .saturating_sub(res.rfind('/').map(|index| index + 1).unwrap_or(0));
                            } else {
                                res.clear();
                                last_segment_length = 0;
                            }
                            last_slash = index as isize;
                            dots = 0;
                            index += 1;
                            previous = Some(current);
                            continue;
                        } else if !res.is_empty() {
                            res.clear();
                            last_segment_length = 0;
                            last_slash = index as isize;
                            dots = 0;
                            index += 1;
                            previous = Some(current);
                            continue;
                        }
                    }
                    if allow_above_root {
                        if res.is_empty() {
                            res.push_str("..");
                        } else {
                            res.push_str("/..");
                        }
                        last_segment_length = 2;
                    }
                } else {
                    let start = (last_slash + 1) as usize;
                    let segment = chars[start..index].iter().collect::<String>();
                    if res.is_empty() {
                        res = segment;
                    } else {
                        res.push('/');
                        res.push_str(&segment);
                    }
                    last_segment_length = index.saturating_sub(start);
                }
                last_slash = index as isize;
                dots = 0;
            } else if current == '.' && dots != -1 {
                dots += 1;
            } else {
                dots = -1;
            }
            previous = Some(current);
            index += 1;
        }
        res
    }

    pub fn normalize(path: &str) -> String {
        if path.is_empty() {
            return ".".to_string();
        }
        let path = normalize_windows_path(path);
        let unc = is_unc(&path);
        let absolute = is_absolute(&path);
        let trailing_separator = path.ends_with('/');
        let mut out = normalize_string(&path, !absolute);
        if out.is_empty() {
            return if absolute {
                "/".to_string()
            } else if trailing_separator {
                "./".to_string()
            } else {
                ".".to_string()
            };
        }
        if trailing_separator {
            out.push('/');
        }
        if is_drive_letter(&out) {
            out.push('/');
        }
        if unc {
            return if absolute {
                format!("//{out}")
            } else {
                format!("//./{out}")
            };
        }
        if absolute && !is_absolute(&out) {
            return format!("/{out}");
        }
        out
    }

    pub fn join(base: &str, segment: &str) -> String {
        let mut path = String::new();
        for piece in [base, segment] {
            if piece.is_empty() {
                continue;
            }
            if path.is_empty() {
                path.push_str(piece);
                continue;
            }
            let trailing = path.ends_with('/');
            let leading = piece.starts_with('/');
            if trailing && leading {
                path.push_str(&piece[1..]);
            } else if trailing || leading {
                path.push_str(piece);
            } else {
                path.push('/');
                path.push_str(piece);
            }
        }
        normalize(&path)
    }
}
