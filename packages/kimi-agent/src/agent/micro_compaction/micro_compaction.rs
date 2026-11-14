pub const DEFAULT_TRUNCATED_MARKER: &str = "[Old tool result content cleared]";

#[derive(Debug, Clone, PartialEq)]
pub struct MicroCompactionConfig {
    pub keep_recent_messages: usize,
    pub min_content_tokens: usize,
    pub truncated_marker: String,
    pub cache_missed_threshold_ms: u64,
    pub min_context_usage_ratio: f64,
}

impl Default for MicroCompactionConfig {
    fn default() -> Self {
        Self {
            keep_recent_messages: 20,
            min_content_tokens: 100,
            truncated_marker: DEFAULT_TRUNCATED_MARKER.to_string(),
            cache_missed_threshold_ms: 60 * 60 * 1000,
            min_context_usage_ratio: 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MicroCompactionState {
    #[serde(default)]
    pub cutoff: usize,
}

impl MicroCompactionState {
    pub fn advance(&mut self, history_len: usize, keep_recent_messages: usize) -> bool {
        let next = history_len.saturating_sub(keep_recent_messages);
        if next == self.cutoff {
            return false;
        }
        self.cutoff = next;
        true
    }

    pub fn clamp(&mut self, history_len: usize) {
        self.cutoff = self.cutoff.min(history_len);
    }

    pub fn reset(&mut self) {
        self.cutoff = 0;
    }
}
