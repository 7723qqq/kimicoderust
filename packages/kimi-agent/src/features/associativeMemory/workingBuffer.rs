use indexmap::IndexMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingItem {
    pub text: String,
    pub last_touched_at: i64,
    pub activations: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingBufferSnapshot {
    pub active: Vec<WorkingItem>,
    pub silent: Vec<WorkingItem>,
    pub size: usize,
}

pub struct WorkingBuffer {
    items: IndexMap<String, WorkingItem>,
    capacity: usize,
    ttl_ms: i64,
}

impl WorkingBuffer {
    pub fn new(capacity: usize, ttl_ms: i64) -> Self {
        Self {
            items: IndexMap::new(),
            capacity,
            ttl_ms,
        }
    }

    pub fn touch(&mut self, text: &str, now: i64) {
        let trimmed = trim_js(text);
        if trimmed.is_empty() {
            return;
        }
        let activations = self.items.get(trimmed).map_or(0, |item| item.activations) + 1;
        self.items.insert(
            trimmed.to_string(),
            WorkingItem {
                text: trimmed.to_string(),
                last_touched_at: now,
                activations,
            },
        );
        self.prune(now);
    }

    pub fn has(&self, text: &str, now: i64) -> bool {
        self.items
            .get(trim_js(text))
            .is_some_and(|item| now - item.last_touched_at <= self.ttl_ms)
    }

    pub fn reactivate(&mut self, cue: &str, now: i64) -> Vec<WorkingItem> {
        self.prune(now);
        let lowered_cue = cue.to_lowercase();
        let terms: Vec<&str> = lowered_cue
            .split(is_js_whitespace)
            .filter(|term| !term.is_empty() && term.encode_utf16().count() >= 3)
            .collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let half = self.ttl_ms as f64 / 2.0;
        let mut out: Vec<WorkingItem> = self
            .items
            .values()
            .filter(|item| (now - item.last_touched_at) as f64 > half)
            .filter(|item| {
                let lowered = item.text.to_lowercase();
                terms.iter().any(|term| lowered.contains(term))
            })
            .cloned()
            .collect();
        out.sort_by(|left, right| {
            right
                .activations
                .cmp(&left.activations)
                .then_with(|| left.text.encode_utf16().cmp(right.text.encode_utf16()))
        });
        out
    }

    pub fn snapshot(&mut self, now: i64) -> WorkingBufferSnapshot {
        self.prune(now);
        let all: Vec<WorkingItem> = self.items.values().cloned().collect();
        let half = self.ttl_ms as f64 / 2.0;
        let mut active: Vec<WorkingItem> = all
            .iter()
            .filter(|item| (now - item.last_touched_at) as f64 <= half)
            .cloned()
            .collect();
        active.sort_by_key(|item| std::cmp::Reverse(item.last_touched_at));
        let silent: Vec<WorkingItem> = all
            .into_iter()
            .filter(|item| (now - item.last_touched_at) as f64 > half)
            .collect();
        WorkingBufferSnapshot {
            active,
            silent,
            size: self.items.len(),
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    fn prune(&mut self, now: i64) {
        self.items
            .retain(|_, item| now - item.last_touched_at <= self.ttl_ms);
        while self.items.len() > self.capacity {
            let oldest = self
                .items
                .iter()
                .min_by(|left, right| {
                    left.1
                        .last_touched_at
                        .cmp(&right.1.last_touched_at)
                        .then_with(|| left.0.encode_utf16().cmp(right.0.encode_utf16()))
                })
                .map(|(key, _)| key.clone());
            let Some(key) = oldest else {
                break;
            };
            self.items.remove(&key);
        }
    }
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_accumulates_activations_and_ignores_blank_text() {
        let mut buffer = WorkingBuffer::new(8, 100);
        buffer.touch("  alpha  ", 10);
        buffer.touch("alpha", 20);
        buffer.touch("   ", 30);
        assert_eq!(buffer.items.len(), 1);
        assert_eq!(buffer.items["alpha"].activations, 2);
        assert_eq!(buffer.items["alpha"].last_touched_at, 20);
        assert_eq!(buffer.items["alpha"].text, "alpha");
    }

    #[test]
    fn has_respects_the_ttl_and_trims_the_query() {
        let mut buffer = WorkingBuffer::new(8, 100);
        buffer.touch("alpha", 10);
        assert!(buffer.has("alpha", 110));
        assert!(buffer.has("  alpha ", 110));
        assert!(!buffer.has("alpha", 111));
    }

    #[test]
    fn reactivate_only_returns_silent_items_matching_a_long_enough_term() {
        let mut buffer = WorkingBuffer::new(8, 100);
        buffer.touch("the auth service", 0);
        buffer.touch("a totally different note", 0);
        buffer.touch("auth retry policy", 0);
        let out = buffer.reactivate("AUTH", 80);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "auth retry policy", "more activations first");
        assert_eq!(out[1].text, "the auth service");
    }

    #[test]
    fn reactivate_ignores_terms_shorter_than_three_characters() {
        let mut buffer = WorkingBuffer::new(8, 100);
        buffer.touch("ab auth", 0);
        assert!(buffer.reactivate("ab", 80).is_empty());
        assert_eq!(buffer.reactivate("auth", 80).len(), 1);
    }

    #[test]
    fn reactivate_skips_items_still_inside_the_active_half() {
        let mut buffer = WorkingBuffer::new(8, 100);
        buffer.touch("auth retry", 95);
        assert!(buffer.reactivate("auth", 99).is_empty());
        assert_eq!(buffer.reactivate("auth", 150).len(), 1);
    }

    #[test]
    fn snapshot_splits_active_and_silent_by_the_half_ttl() {
        let mut buffer = WorkingBuffer::new(8, 100);
        buffer.touch("fresh", 95);
        buffer.touch("stale", 10);
        let snapshot = buffer.snapshot(100);
        assert_eq!(snapshot.active.len(), 1);
        assert_eq!(snapshot.active[0].text, "fresh");
        assert_eq!(snapshot.silent.len(), 1);
        assert_eq!(snapshot.size, 2);
    }

    #[test]
    fn prune_drops_expired_entries() {
        let mut buffer = WorkingBuffer::new(8, 100);
        buffer.touch("old", 0);
        buffer.touch("new", 150);
        assert_eq!(buffer.items.len(), 1);
        assert!(buffer.items.contains_key("new"));
    }

    #[test]
    fn prune_evicts_the_oldest_once_capacity_is_exceeded() {
        let mut buffer = WorkingBuffer::new(2, 100);
        buffer.touch("first", 10);
        buffer.touch("second", 20);
        buffer.touch("third", 30);
        assert_eq!(buffer.items.len(), 2);
        assert!(!buffer.items.contains_key("first"));
        assert_eq!(buffer.items["third"].text, "third");
    }

    #[test]
    fn clear_empties_the_buffer() {
        let mut buffer = WorkingBuffer::new(8, 100);
        buffer.touch("alpha", 10);
        buffer.clear();
        assert!(buffer.items.is_empty());
        assert!(!buffer.has("alpha", 10));
    }
}
