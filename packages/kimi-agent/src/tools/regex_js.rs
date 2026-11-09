use std::cell::RefCell;
use std::collections::HashMap;

use rquickjs::{Context, Runtime};

const BOOTSTRAP: &str = r#"
globalThis.__kimiRegex = [];
globalThis.__kimiRegexTest = function (index, text) {
  var regex = globalThis.__kimiRegex[index];
  regex.lastIndex = 0;
  return regex.test(text);
};
"#;

struct Engine {
    context: Context,
    cache: HashMap<String, Result<usize, String>>,
}

impl Engine {
    fn new() -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|error| error.to_string())?;
        let context = Context::full(&runtime).map_err(|error| error.to_string())?;
        context
            .with(|ctx| ctx.eval::<rquickjs::Value, _>(BOOTSTRAP).map(|_| ()))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            context,
            cache: HashMap::new(),
        })
    }

    fn compile(&mut self, pattern: &str, flags: &str) -> Result<usize, String> {
        let key = format!("{flags}\u{0}{pattern}");
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }
        let source = serde_json::to_string(pattern).map_err(|error| error.to_string())?;
        let flags = serde_json::to_string(flags).map_err(|error| error.to_string())?;
        let script = format!(
            "(function (source, flags) {{ try {{ globalThis.__kimiRegex.push(new RegExp(source, flags)); return \"ok:\" + (globalThis.__kimiRegex.length - 1); }} catch (error) {{ return \"err:\" + String(error && error.message); }} }})({source}, {flags})"
        );
        let outcome = self
            .context
            .with(|ctx| ctx.eval::<String, _>(script))
            .map_err(|error| error.to_string())
            .and_then(|result| match result.strip_prefix("ok:") {
                Some(index) => index.parse::<usize>().map_err(|error| error.to_string()),
                None => Err(result
                    .strip_prefix("err:")
                    .unwrap_or(result.as_str())
                    .to_string()),
            });
        self.cache.insert(key, outcome.clone());
        outcome
    }
}

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

fn with_engine<T>(f: impl FnOnce(&mut Engine) -> Result<T, String>) -> Result<T, String> {
    ENGINE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(Engine::new()?);
        }
        match slot.as_mut() {
            Some(engine) => f(engine),
            None => Err("the regular expression engine is unavailable".to_string()),
        }
    })
}

pub(crate) fn validate(pattern: &str) -> Result<(), String> {
    validate_with_flags(pattern, "u")
}

pub(crate) fn validate_with_flags(pattern: &str, flags: &str) -> Result<(), String> {
    with_engine(|engine| engine.compile(pattern, flags).map(|_| ()))
}

pub(crate) fn is_match(pattern: &str, text: &str) -> Result<bool, String> {
    is_match_with_flags(pattern, text, "u")
}

pub(crate) fn is_match_with_flags(pattern: &str, text: &str, flags: &str) -> Result<bool, String> {
    with_engine(|engine| {
        let index = engine.compile(pattern, flags)?;
        let source = serde_json::to_string(text).map_err(|error| error.to_string())?;
        let script = format!("globalThis.__kimiRegexTest({index}, {source})");
        engine
            .context
            .with(|ctx| ctx.eval::<bool, _>(script))
            .map_err(|error| error.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::{is_match, is_match_with_flags, validate};

    fn matches(pattern: &str, text: &str) -> bool {
        is_match(pattern, text).expect("pattern must compile")
    }

    #[test]
    fn js_only_constructs_resolve_the_way_ajv_resolves_them() {
        assert!(matches("^(?=.*\\d)[a-z\\d]+$", "abc1"));
        assert!(!matches("^(?=.*\\d)[a-z\\d]+$", "abcdef"));
        assert!(matches("^(?!.*\\d)[a-z]+$", "abcdef"));
        assert!(!matches("^(?!.*\\d)[a-z]+$", "abc1"));
        assert!(matches("(?<=a)b", "ab"));
        assert!(!matches("(?<=a)b", "cb"));
        assert!(matches("(?<!a)b", "cb"));
        assert!(!matches("(?<!a)b", "ab"));
        assert!(matches("^(a)\\1$", "aa"));
        assert!(!matches("^(a)\\1$", "ab"));
        assert!(matches("^(?<x>a)\\k<x>$", "aa"));
    }

    #[test]
    fn js_character_classes_and_boundaries_follow_ascii_rules() {
        assert!(matches("^\\d$", "5"));
        assert!(!matches("^\\d$", "\u{0663}"));
        assert!(!matches("^\\d$", "\u{FF15}"));
        assert!(matches("^\\s$", " "));
        assert!(matches("^\\s$", "\u{FEFF}"));
        assert!(!matches("^\\s$", "\u{0085}"));
        assert!(matches("^\\s$", "\u{00A0}"));
        assert!(matches("\\bfoo\\b", "foo"));
        assert!(matches("\\bfoo\\b", "\u{00E9}foo"));
        assert!(!matches("\\bfoo\\b", "_foo"));
        assert!(matches("^\\u0041$", "A"));
        assert!(!matches("^\\u0041$", "a"));
    }

    #[test]
    fn no_flags_are_applied_beyond_the_unicode_flag_ajv_uses() {
        assert!(!matches("^abc$", "ABC"));
        assert!(!matches("^a.b$", "a\nb"));
        assert!(!matches("^b$", "a\nb"));
        assert!(!matches("^ab$", "ab\n"));
        assert!(matches("^a{2,3}$", "aaa"));
        assert!(matches("^[^a]$", "b"));
    }

    #[test]
    fn explicit_flags_follow_javascript_case_folding_and_anchoring() {
        assert!(is_match_with_flags("^abc$", "ABC", "iu").expect("compiles"));
        assert!(is_match_with_flags("^\u{00E9}$", "\u{00C9}", "iu").expect("compiles"));
        assert!(is_match_with_flags("^k$", "\u{212A}", "iu").expect("compiles"));
        assert!(is_match_with_flags("^b$", "a\nb", "mu").expect("compiles"));
        assert!(is_match_with_flags("^a.b$", "a\nb", "su").expect("compiles"));
        assert!(!is_match_with_flags("^a.b$", "a\nb", "u").expect("compiles"));
    }

    #[test]
    fn ajv_rejects_the_same_invalid_patterns() {
        for pattern in ["(?<=a", "[", "(?i)abc"] {
            assert!(validate(pattern).is_err(), "{pattern}");
        }
    }

    #[test]
    fn a_compiled_pattern_stays_cached_across_lookups() {
        for _ in 0..3 {
            assert!(validate("(?<=a)b").is_ok());
            assert!(matches("(?<=a)b", "ab"));
            assert!(!matches("(?<=a)b", "cb"));
        }
    }
}
