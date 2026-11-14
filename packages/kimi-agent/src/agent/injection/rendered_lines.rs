use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

pub const MAX_TRACKED_LINES: usize = 1024;
pub const MAX_TRACKED_SESSIONS: usize = 32;

pub fn line_fingerprint(line: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(line.trim().to_lowercase().as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[derive(Default)]
struct TrackedLines {
    order: Vec<String>,
    seen: HashSet<String>,
}

#[derive(Default)]
pub struct RenderedLines {
    sessions: HashMap<String, TrackedLines>,
    session_order: Vec<String>,
}

impl RenderedLines {
    fn set_for(&mut self, session_key: &str) -> &mut TrackedLines {
        if !self.sessions.contains_key(session_key) {
            self.sessions
                .insert(session_key.to_string(), TrackedLines::default());
            self.session_order.push(session_key.to_string());
            while self.session_order.len() > MAX_TRACKED_SESSIONS {
                let oldest = self.session_order.remove(0);
                self.sessions.remove(&oldest);
            }
        }
        self.sessions.get_mut(session_key).expect("just inserted")
    }

    pub fn already_rendered(&self, session_key: &str, line: &str) -> bool {
        self.sessions
            .get(session_key)
            .is_some_and(|set| set.seen.contains(&line_fingerprint(line)))
    }

    pub fn mark_rendered(&mut self, session_key: &str, lines: &[String]) {
        let set = self.set_for(session_key);
        for line in lines {
            let fingerprint = line_fingerprint(line);
            if set.seen.insert(fingerprint.clone()) {
                set.order.push(fingerprint);
            }
            if set.order.len() <= MAX_TRACKED_LINES {
                continue;
            }
            let oldest = set.order.remove(0);
            set.seen.remove(&oldest);
        }
    }

    pub fn reset(&mut self, session_key: Option<&str>) {
        match session_key {
            Some(key) => {
                self.sessions.remove(key);
                self.session_order.retain(|item| item != key);
            }
            None => {
                self.sessions.clear();
                self.session_order.clear();
            }
        }
    }

    pub fn tracked_session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn tracked_line_count(&self, session_key: &str) -> usize {
        self.sessions
            .get(session_key)
            .map_or(0, |set| set.seen.len())
    }
}

static RENDERED: LazyLock<Mutex<RenderedLines>> =
    LazyLock::new(|| Mutex::new(RenderedLines::default()));

pub fn already_rendered(session_key: &str, line: &str) -> bool {
    RENDERED.lock().already_rendered(session_key, line)
}

pub fn mark_rendered(session_key: &str, lines: &[String]) {
    RENDERED.lock().mark_rendered(session_key, lines);
}

pub fn reset_rendered(session_key: Option<&str>) {
    RENDERED.lock().reset(session_key);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_normalizes_case_and_surrounding_space() {
        assert_eq!(
            line_fingerprint("  Hello World \n"),
            line_fingerprint("hello world")
        );
        assert_eq!(line_fingerprint("hello world").len(), 16);
        assert_ne!(
            line_fingerprint("hello world"),
            line_fingerprint("hello worlds")
        );
    }

    #[test]
    fn mark_then_probe_reports_rendered() {
        let mut rendered = RenderedLines::default();
        assert!(!rendered.already_rendered("s1", "note"));
        rendered.mark_rendered("s1", &["note".to_string()]);
        assert!(rendered.already_rendered("s1", " NOTE "));
        assert!(!rendered.already_rendered("s2", "note"));
    }

    #[test]
    fn line_budget_evicts_the_oldest_fingerprint() {
        let mut rendered = RenderedLines::default();
        let lines: Vec<String> = (0..=MAX_TRACKED_LINES)
            .map(|index| format!("line {index}"))
            .collect();
        rendered.mark_rendered("s1", &lines);

        assert_eq!(rendered.tracked_line_count("s1"), MAX_TRACKED_LINES);
        assert!(!rendered.already_rendered("s1", "line 0"));
        assert!(rendered.already_rendered("s1", "line 1"));
        assert!(rendered.already_rendered("s1", &format!("line {MAX_TRACKED_LINES}")));
    }

    #[test]
    fn session_budget_evicts_the_oldest_session() {
        let mut rendered = RenderedLines::default();
        for index in 0..=MAX_TRACKED_SESSIONS {
            rendered.mark_rendered(&format!("s{index}"), &["x".to_string()]);
        }
        assert_eq!(rendered.tracked_session_count(), MAX_TRACKED_SESSIONS);
        assert!(!rendered.already_rendered("s0", "x"));
        assert!(rendered.already_rendered(&format!("s{MAX_TRACKED_SESSIONS}"), "x"));
    }

    #[test]
    fn revisiting_a_tracked_session_does_not_refresh_its_age() {
        let mut rendered = RenderedLines::default();
        rendered.mark_rendered("s0", &["x".to_string()]);
        for index in 1..MAX_TRACKED_SESSIONS {
            rendered.mark_rendered(&format!("s{index}"), &["x".to_string()]);
        }
        rendered.mark_rendered("s0", &["y".to_string()]);
        rendered.mark_rendered("s-new", &["x".to_string()]);

        assert!(!rendered.already_rendered("s0", "x"));
        assert!(rendered.already_rendered("s1", "x"));
    }

    #[test]
    fn reset_drops_one_session_or_all() {
        let mut rendered = RenderedLines::default();
        rendered.mark_rendered("s1", &["a".to_string()]);
        rendered.mark_rendered("s2", &["b".to_string()]);
        rendered.reset(Some("s1"));
        assert!(!rendered.already_rendered("s1", "a"));
        assert!(rendered.already_rendered("s2", "b"));
        rendered.reset(None);
        assert_eq!(rendered.tracked_session_count(), 0);
    }
}
