
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

pub const TURN_WATCH_CHECK_MS: u64 = 60_000;

pub const TURN_WATCH_WARN_MS: u64 = 10 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TurnPhase {
    Llm = 0,
    Tools = 1,
    Compacting = 2,
}

impl TurnPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnPhase::Llm => "llm",
            TurnPhase::Tools => "tools",
            TurnPhase::Compacting => "compacting",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => TurnPhase::Tools,
            2 => TurnPhase::Compacting,
            _ => TurnPhase::Llm,
        }
    }
}

#[derive(Default)]
pub struct TurnWatchState {
    steps: AtomicU64,
    partials: AtomicU64,
    tool_marks: AtomicU64,
    phase: AtomicU8,
}

impl TurnWatchState {
    pub fn note_steps(&self, steps: u64) {
        self.steps.store(steps, Ordering::Relaxed);
    }

    pub fn note_partial(&self) {
        self.partials.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_tool_activity(&self) {
        self.tool_marks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_phase(&self, phase: TurnPhase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Signature {
        Signature {
            steps: self.steps.load(Ordering::Relaxed),
            partials: self.partials.load(Ordering::Relaxed),
            tool_marks: self.tool_marks.load(Ordering::Relaxed),
            phase: self.phase.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Signature {
    steps: u64,
    partials: u64,
    tool_marks: u64,
    phase: u8,
}

struct WatchCore {
    signature: Option<Signature>,
    since_ms: i64,
    last_warn_ms: i64,
}

impl WatchCore {
    fn new() -> Self {
        Self {
            signature: None,
            since_ms: 0,
            last_warn_ms: 0,
        }
    }

    fn check(&mut self, signature: Signature, now_ms: i64) -> Option<i64> {
        let Some(last) = self.signature else {
            self.signature = Some(signature);
            self.since_ms = now_ms;
            return None;
        };
        if last != signature {
            self.signature = Some(signature);
            self.since_ms = now_ms;
            return None;
        }
        let silent_ms = now_ms - self.since_ms;
        if silent_ms < TURN_WATCH_WARN_MS as i64 {
            return None;
        }
        if now_ms - self.last_warn_ms < TURN_WATCH_WARN_MS as i64 {
            return None;
        }
        self.last_warn_ms = now_ms;
        Some(silent_ms)
    }
}

pub struct TurnWatchdog {
    task: tokio::task::JoinHandle<()>,
}

impl TurnWatchdog {
    pub fn arm(agent_id: &str, turn_id: &str, state: Arc<TurnWatchState>) -> Self {
        let agent_id = agent_id.to_string();
        let turn_id = turn_id.to_string();
        let task = tokio::spawn(async move {
            let mut core = WatchCore::new();
            let clock = std::time::Instant::now();
            let mut interval =
                tokio::time::interval(std::time::Duration::from_millis(TURN_WATCH_CHECK_MS));
            loop {
                interval.tick().await;
                let now_ms = clock.elapsed().as_millis().min(i64::MAX as u128) as i64;
                let signature = state.snapshot();
                if let Some(silent_ms) = core.check(signature, now_ms) {
                    tracing::warn!(
                        agent_id = %agent_id,
                        turn_id = %turn_id,
                        silent_minutes = (silent_ms as f64 / 60_000.0).round() as i64,
                        steps = signature.steps,
                        phase = TurnPhase::from_u8(signature.phase).as_str(),
                        "turn watchdog: no new events from the running turn"
                    );
                }
            }
        });
        Self { task }
    }
}

impl Drop for TurnWatchdog {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WARN: i64 = TURN_WATCH_WARN_MS as i64;

    fn signature(steps: u64, partials: u64, tool_marks: u64, phase: TurnPhase) -> Signature {
        Signature {
            steps,
            partials,
            tool_marks,
            phase: phase as u8,
        }
    }

    #[test]
    fn the_first_sample_only_seeds_the_clock() {
        let mut core = WatchCore::new();
        assert_eq!(core.check(signature(1, 0, 0, TurnPhase::Llm), 0), None);
        // Immediately re-sampling the same signature stays silent.
        assert_eq!(core.check(signature(1, 0, 0, TurnPhase::Llm), 1), None);
    }

    #[test]
    fn progress_resets_the_silence_clock() {
        let mut core = WatchCore::new();
        assert_eq!(core.check(signature(1, 0, 0, TurnPhase::Llm), 0), None);
        // Almost a full window passes, then a streamed delta lands.
        assert_eq!(
            core.check(signature(1, 0, 0, TurnPhase::Llm), WARN - 1),
            None
        );
        assert_eq!(core.check(signature(1, 1, 0, TurnPhase::Llm), WARN), None);
        // The window starts over from the delta: no warn one window later
        // measured from the ORIGINAL start, only from the last progress.
        assert_eq!(
            core.check(signature(1, 1, 0, TurnPhase::Llm), 2 * WARN - 1),
            None
        );
        assert_eq!(
            core.check(signature(1, 1, 0, TurnPhase::Llm), 2 * WARN),
            Some(WARN)
        );
    }

    #[test]
    fn every_signature_arm_counts_as_progress() {
        for (steps, partials, tool_marks, phase) in [
            (2, 0, 0, TurnPhase::Llm),   // step transition
            (1, 1, 0, TurnPhase::Llm),   // streamed delta
            (1, 0, 1, TurnPhase::Llm),   // tool activity
            (1, 0, 0, TurnPhase::Tools), // phase change
        ] {
            let mut core = WatchCore::new();
            assert_eq!(core.check(signature(1, 0, 0, TurnPhase::Llm), 0), None);
            assert_eq!(
                core.check(signature(steps, partials, tool_marks, phase), WARN),
                None,
                "({steps}, {partials}, {tool_marks}, {phase:?}) must reset the clock"
            );
        }
    }

    #[test]
    fn the_warn_repeats_once_per_silent_window() {
        let mut core = WatchCore::new();
        let sig = signature(3, 5, 2, TurnPhase::Tools);
        assert_eq!(core.check(sig, 0), None);
        // Below the threshold: silent.
        assert_eq!(core.check(sig, WARN - 1), None);
        // At the threshold: first warn, reporting the full silence.
        assert_eq!(core.check(sig, WARN), Some(WARN));
        // The throttle holds for another full window even though the turn is
        // still silent.
        assert_eq!(core.check(sig, WARN + 1), None);
        assert_eq!(core.check(sig, 2 * WARN - 1), None);
        assert_eq!(core.check(sig, 2 * WARN), Some(2 * WARN));
        // Progress after the warnings re-arms the whole cycle.
        assert_eq!(
            core.check(signature(3, 6, 2, TurnPhase::Tools), 2 * WARN + 1),
            None
        );
        assert_eq!(
            core.check(signature(3, 6, 2, TurnPhase::Tools), 3 * WARN),
            None
        );
        assert_eq!(
            core.check(signature(3, 6, 2, TurnPhase::Tools), 3 * WARN + 1),
            Some(WARN)
        );
    }

    #[test]
    fn the_state_surfaces_every_producer_in_its_snapshot() {
        let state = TurnWatchState::default();
        state.note_steps(4);
        state.note_partial();
        state.note_partial();
        state.note_tool_activity();
        state.set_phase(TurnPhase::Compacting);
        assert_eq!(state.snapshot(), signature(4, 2, 1, TurnPhase::Compacting));
    }
}
