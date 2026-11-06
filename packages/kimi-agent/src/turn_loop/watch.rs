//! Per-turn watchdog (v2 `AgentLoopService.armTurnWatch` /
//! `disarmTurnWatch`, upstream #4280 diagnostics half, `67325c1b25`).
//!
//! A running turn that produces no observable progress for
//! [`TURN_WATCH_WARN_MS`] is reported once per silent window, so a wedged
//! loop surfaces in the log instead of stalling for hours with zero
//! diagnostics — the incident the upstream change answers. The sampler wakes
//! every [`TURN_WATCH_CHECK_MS`]; a signature change resets the silence
//! clock, and the warn throttle reuses the warn window, mirroring upstream's
//! repeat period.
//!
//! The signature (v2 `signatureOf`) is the turn's observable progress: step
//! transitions, streamed deltas, tool-call activity, and the loop's phase.
//! Producers are plain atomic stores on [`TurnWatchState`], so the hot paths
//! (one store per streamed delta) never take a lock.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// How often the watchdog samples the turn's progress signature (v2
/// `TURN_WATCH_CHECK_MS`).
pub const TURN_WATCH_CHECK_MS: u64 = 60_000;

/// How long the signature may stay unchanged before a warning fires — and the
/// minimum gap between repeated warnings (upstream reuses
/// `TURN_WATCH_WARN_MS` for the repeat throttle).
pub const TURN_WATCH_WARN_MS: u64 = 10 * 60_000;

/// What the running turn is doing right now (v2 `engine.turn.phase`): part of
/// the progress signature, and reported in the warning so a stalled turn
/// says where it stalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TurnPhase {
    /// Awaiting (or streaming from) the provider's response.
    Llm = 0,
    /// Executing the step's scheduled tool calls.
    Tools = 1,
    /// Compacting the working history before the next request.
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

/// The turn's observable-progress signature, written by the loop and the LLM
/// transport and sampled by the watchdog (v2 `signatureOf`'s arms: steps,
/// streaming partials, machine phase, active tool calls).
#[derive(Default)]
pub struct TurnWatchState {
    steps: AtomicU64,
    partials: AtomicU64,
    tool_marks: AtomicU64,
    phase: AtomicU8,
}

impl TurnWatchState {
    /// The loop reached a new step (v2 `active.steps`).
    pub fn note_steps(&self, steps: u64) {
        self.steps.store(steps, Ordering::Relaxed);
    }

    /// The transport streamed one more fragment for this turn (v2
    /// `active.partials.length`). Installed on the transport per turn through
    /// [`crate::turn_loop::types::LLM::set_turn_watch`].
    pub fn note_partial(&self) {
        self.partials.fetch_add(1, Ordering::Relaxed);
    }

    /// A tool batch started or finished (the churn v2's
    /// `activeToolCalls.length` would show).
    pub fn note_tool_activity(&self) {
        self.tool_marks.fetch_add(1, Ordering::Relaxed);
    }

    /// The loop entered a new phase (v2 `engine.turn.phase`).
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

/// One sample of the signature; any field change is progress (v2 joins the
/// arms into one comparable string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Signature {
    steps: u64,
    partials: u64,
    tool_marks: u64,
    phase: u8,
}

/// The warn decision, kept pure so the tests drive it with a fake clock; the
/// driver below only feeds it a monotonic millisecond source.
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

    /// Sample the current signature at `now_ms`. Returns the silence duration
    /// when a warning should fire this sample.
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

/// The running watchdog for one turn (v2 `turnWatch`). Dropping aborts the
/// sampler — the loop keeps one per `run_turn` invocation and lets it drop at
/// every exit path, which is `disarmTurnWatch` on `endTurn` / dispose.
pub struct TurnWatchdog {
    task: tokio::task::JoinHandle<()>,
}

impl TurnWatchdog {
    /// Arm the watchdog for the turn now starting (v2 `beginActiveTurn` →
    /// `armTurnWatch`).
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
