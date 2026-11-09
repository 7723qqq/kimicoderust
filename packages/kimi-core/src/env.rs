
pub fn parse_bool_env(value: Option<&str>) -> Option<bool> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub fn env_switch_default_on(name: &str) -> bool {
    parse_bool_env(std::env::var(name).ok().as_deref()) != Some(false)
}

pub fn env_switch_default_off(name: &str) -> bool {
    parse_bool_env(std::env::var(name).ok().as_deref()) == Some(true)
}

#[cfg(any(test, feature = "test-util"))]
pub static ENV_TEST_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bool_env_matches_v2_parse_boolean_env() {
        for value in ["1", "true", " TRUE ", "yes", "on", "On"] {
            assert_eq!(parse_bool_env(Some(value)), Some(true), "{value:?}");
        }
        for value in ["0", "false", " FALSE ", "no", "off", "Off"] {
            assert_eq!(parse_bool_env(Some(value)), Some(false), "{value:?}");
        }
        for value in [None, Some(""), Some("  "), Some("banana"), Some("2")] {
            assert_eq!(parse_bool_env(value), None, "{value:?}");
        }
    }

    #[test]
    fn default_on_only_bows_to_an_explicit_falsy() {
        for value in [None, Some(""), Some("1"), Some("banana"), Some("on")] {
            assert!(
                parse_bool_env(value) != Some(false),
                "{value:?} keeps a default-on switch on"
            );
        }
        for value in ["0", "false", "no", "off"] {
            assert_eq!(parse_bool_env(Some(value)), Some(false), "{value:?}");
        }
    }
}
