use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
pub struct ThinkingGuardConfig {
    pub window_chars: usize,

    pub ngram_chars: usize,

    pub repeat_threshold: usize,

    pub min_chars: usize,

    pub check_every_chars: usize,
}

impl Default for ThinkingGuardConfig {
    fn default() -> Self {
        Self {
            window_chars: 2000,
            ngram_chars: 16,
            repeat_threshold: 6,
            min_chars: 240,
            check_every_chars: 256,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingVerdict {
    Forward,

    Suppressed,
}

pub const GUARD_ENV: &str = "KIMI_AGENT_THINKING_GUARD";

pub const GUARD_CONFIG_KEY: &str = "thinking_repeat_guard";

static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

pub fn enabled() -> bool {
    match crate::env::parse_bool_env(std::env::var(GUARD_ENV).ok().as_deref()) {
        Some(from_env) => from_env,

        None => ENABLED.load(std::sync::atomic::Ordering::Relaxed),
    }
}

#[derive(Debug)]
pub struct ThinkingGuard {
    config: ThinkingGuardConfig,
    tail: String,
    tripped: bool,
    chars_since_check: usize,
    disabled: bool,
}

impl ThinkingGuard {
    pub fn new(config: ThinkingGuardConfig) -> Self {
        Self {
            config,
            tail: String::new(),
            tripped: false,
            chars_since_check: 0,
            disabled: false,
        }
    }

    pub fn from_settings() -> Self {
        let mut guard = Self::new(ThinkingGuardConfig::default());
        guard.disabled = !enabled();
        guard
    }

    pub fn tripped(&self) -> bool {
        self.tripped
    }

    pub fn observe(&mut self, delta: &str) -> ThinkingVerdict {
        if self.disabled {
            return ThinkingVerdict::Forward;
        }
        if self.tripped {
            return ThinkingVerdict::Suppressed;
        }
        self.tail.push_str(delta);
        self.trim_tail();
        self.chars_since_check += delta.chars().count();
        if self.tail.chars().count() < self.config.min_chars
            || self.chars_since_check < self.config.check_every_chars
        {
            return ThinkingVerdict::Forward;
        }
        self.chars_since_check = 0;
        if self.most_repeated_window() >= self.config.repeat_threshold {
            self.tripped = true;
            return ThinkingVerdict::Suppressed;
        }
        ThinkingVerdict::Forward
    }

    pub fn reset(&mut self) {
        self.tail.clear();
        self.tripped = false;
        self.chars_since_check = 0;
    }

    fn trim_tail(&mut self) {
        let limit = self.config.window_chars;
        let len = self.tail.chars().count();
        if len <= limit {
            return;
        }
        let cut = len - limit;
        let byte = self
            .tail
            .char_indices()
            .nth(cut)
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.tail.drain(..byte);
    }

    fn most_repeated_window(&self) -> usize {
        let n = self.config.ngram_chars;
        if n == 0 {
            return 0;
        }
        let chars: Vec<char> = self.tail.chars().collect();
        if chars.len() < n {
            return 0;
        }
        let mut counts: HashMap<&[char], usize> = HashMap::new();
        let mut max = 0;
        for window in chars.windows(n) {
            let count = counts.entry(window).or_insert(0);
            *count += 1;
            if *count > max {
                max = *count;
            }
        }
        max
    }
}

pub fn gate_delta(
    guard: &mut ThinkingGuard,
    delta: crate::llm::wire::StreamDelta,
) -> Option<crate::llm::wire::StreamDelta> {
    use crate::llm::wire::StreamDelta;
    match delta {
        StreamDelta::Think(text) => match guard.observe(&text) {
            ThinkingVerdict::Forward => Some(StreamDelta::Think(text)),
            ThinkingVerdict::Suppressed => None,
        },
        StreamDelta::Text(text) => {
            guard.reset();
            Some(StreamDelta::Text(text))
        }

        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::wire::StreamDelta;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn guard() -> ThinkingGuard {
        ThinkingGuard::new(ThinkingGuardConfig::default())
    }

    fn feed(guard: &mut ThinkingGuard, text: &str) -> usize {
        let mut suppressed = 0;
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(16) {
            let piece: String = chunk.iter().collect();
            if guard.observe(&piece) == ThinkingVerdict::Suppressed {
                suppressed += 1;
            }
        }
        suppressed
    }

    #[test]
    fn ordinary_reasoning_is_forwarded() {
        let mut g = guard();
        let text = "The user wants the failing test fixed. I should read the \
                    assertion first, then check whether the fixture is stale. \
                    The error mentions a missing field, so the wire shape is \
                    probably wrong. Let me look at the struct definition and \
                    compare it with what the callback returns. If the field \
                    names differ, the fix belongs on the callback side.";
        assert_eq!(feed(&mut g, text), 0);
        assert!(!g.tripped());
    }

    #[test]
    fn a_repeated_phrase_trips_the_guard() {
        let mut g = guard();
        let text = "Let me check. ".repeat(60);
        assert!(feed(&mut g, &text) > 0);
        assert!(g.tripped());
    }

    #[test]
    fn a_cjk_loop_trips_the_guard() {
        let mut g = guard();

        let text = "让我检查一下这个文件的内容是否正确".repeat(40);
        assert!(feed(&mut g, &text) > 0);
        assert!(g.tripped());
    }

    #[test]
    fn once_tripped_every_later_delta_is_suppressed() {
        let mut g = guard();
        feed(&mut g, &"Let me check. ".repeat(60));
        assert!(g.tripped());
        assert_eq!(
            g.observe("and now something entirely new"),
            ThinkingVerdict::Suppressed
        );
    }

    #[test]
    fn reset_clears_the_trip() {
        let mut g = guard();
        feed(&mut g, &"Let me check. ".repeat(60));
        assert!(g.tripped());
        g.reset();
        assert!(!g.tripped());
        assert_eq!(
            g.observe("A fresh thinking block begins here."),
            ThinkingVerdict::Forward
        );
    }

    #[test]
    fn short_thinking_is_never_judged() {
        let mut g = guard();

        assert_eq!(feed(&mut g, &"Let me check. ".repeat(8)), 0);
        assert!(!g.tripped());
    }

    #[test]
    fn the_tail_is_bounded() {
        let mut g = guard();
        feed(&mut g, &"word ".repeat(2000));
        assert!(g.tail.chars().count() <= ThinkingGuardConfig::default().window_chars);
    }

    #[test]
    fn the_env_switch_follows_the_shared_v2_parse_boolean_env_rule() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var(GUARD_ENV).ok();

        set_enabled(true);
        for value in ["0", "false", "no", "off", " FALSE "] {
            unsafe { std::env::set_var(GUARD_ENV, value) };
            assert!(!enabled(), "{value:?} must disable the guard");
        }

        set_enabled(false);
        for value in ["1", "true", "yes", "on", " ON "] {
            unsafe { std::env::set_var(GUARD_ENV, value) };
            assert!(enabled(), "{value:?} must enable the guard");
        }

        set_enabled(true);
        for value in ["", "banana", "2"] {
            unsafe { std::env::set_var(GUARD_ENV, value) };
            assert!(enabled(), "{value:?} must fall back to the config value");
        }

        match saved {
            Some(v) => unsafe { std::env::set_var(GUARD_ENV, v) },
            None => unsafe { std::env::remove_var(GUARD_ENV) },
        }
        set_enabled(true);
    }

    #[test]
    fn a_repeat_that_never_reaches_the_threshold_is_forwarded() {
        let mut g = guard();

        let text = format!(
            "{}and then it moved on to other work entirely.",
            "Let me check. ".repeat(5)
        );
        assert_eq!(feed(&mut g, &text), 0);
        assert!(!g.tripped());
    }

    #[test]
    fn a_disabled_guard_forwards_everything() {
        let mut g = guard();
        g.disabled = true;
        assert_eq!(feed(&mut g, &"Let me check. ".repeat(60)), 0);
        assert!(!g.tripped());
    }

    #[test]
    fn gate_delta_drops_the_loop_and_keeps_the_answer() {
        let mut g = guard();
        let mut forwarded = 0;
        let mut dropped = 0;
        for _ in 0..60 {
            match gate_delta(&mut g, StreamDelta::Think("Let me check. ".into())) {
                Some(_) => forwarded += 1,
                None => dropped += 1,
            }
        }
        assert!(g.tripped());
        assert!(dropped > 0, "the loop must stop reaching the host");
        assert!(forwarded > 0, "the opening of the block is still shown");

        assert!(gate_delta(&mut g, StreamDelta::Text("Here is the fix.".into())).is_some());
        assert!(!g.tripped());
        assert!(gate_delta(&mut g, StreamDelta::Think("A fresh block.".into())).is_some());
    }
}
