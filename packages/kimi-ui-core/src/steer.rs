//! Steer delivery state (v2 #4627): the reader's side of a steered message.
//!
//! A host submits a steer optimistically and shows it immediately; the engine
//! may consume it at the next step, queue it behind another turn, or refuse it.
//! Until the engine says it was read, the row stays pending — dimmed — and an
//! acknowledgement clears it.
//!
//! The state machine is here rather than in a host because it must survive the
//! ordering race that actually happens: the engine can report a steer read
//! *before* the submission promise resolves, so an acknowledgement for a
//! not-yet-registered prompt id has to be remembered and applied when the
//! receipt arrives. Every method returns the [`EntryOp`]s the change implies;
//! the caller owns the store and the lock, and this module never touches
//! either.
//!
//! v2 emits `turn.steer` when the steer nudge is consumed into the running
//! turn's context (not when it is submitted), which is what makes the dimmed
//! row mean "the model has not seen this yet" rather than "the host has not
//! sent this yet".
//!
//! ## Two things this module deliberately does not carry
//!
//! v2's TUI can also match a delivered steer by its text (`steerEventParts` /
//! `takeDeliveredSteerBatches`). This engine never needs that: `turn.steer` is
//! emitted only from the drain, for messages that carry a prompt id, so
//! `prompt_ids` is always present — pinned by
//! `session::tests::draining_a_steer_reports_it_read_with_its_prompt_id`.
//!
//! Pending state survives a turn boundary on purpose: the engine's steer queue
//! is per session, and a steer its turn did not reach is drained by the next
//! one — pinned by
//! `session::tests::a_steer_that_missed_its_turn_is_read_by_the_next_one`.
//! That is why there is no "turn ended, clear everything" reset: it would
//! undim a message the model has not seen yet. `reset` exists for session
//! teardown, not for turn boundaries.

use std::collections::BTreeSet;

use crate::entry_id::EntryId;
use crate::entry_op::EntryOp;

/// The state an engine reports for a submitted prompt (v2 `SteerResult.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionState {
    /// It launched or steered a live turn.
    Running,
    /// It waits behind an active turn; its own turn starts later.
    Queued,
    /// A hook or a quiescence guard refused to launch it.
    Blocked,
}

/// One submitted message whose delivery has not been settled.
#[derive(Debug)]
struct Batch {
    entries: Vec<EntryId>,
    /// Known only after the submission's receipt resolves.
    prompt_id: Option<String>,
}

impl Batch {
    fn holds(&self, entries: &[EntryId]) -> bool {
        self.entries.iter().any(|id| entries.contains(id))
    }
}

/// Pending steer batches, keyed by the entries they own.
#[derive(Debug, Default)]
pub struct SteerDelivery {
    batches: Vec<Batch>,
    /// Prompt ids acknowledged before their submission was registered.
    consumed: BTreeSet<String>,
}

impl SteerDelivery {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a submitted steer. Its entries are pending until acknowledged.
    pub fn note(&mut self, entries: Vec<EntryId>) -> Vec<EntryOp> {
        let ops = pending_ops(&entries, true);
        self.batches.push(Batch {
            entries,
            prompt_id: None,
        });
        ops
    }

    /// Apply the engine's receipt for a batch.
    ///
    /// A prompt that did not join a turn — it launched its own, or was blocked
    /// — is not a steer: nothing will report it read, so it stops being
    /// pending. A queued steer stays pending until its own turn starts.
    pub fn apply_submission(
        &mut self,
        entries: &[EntryId],
        prompt_id: &str,
        state: SubmissionState,
        steered: bool,
    ) -> Vec<EntryOp> {
        let Some(index) = self.batches.iter().position(|batch| batch.holds(entries)) else {
            return Vec::new();
        };
        if state == SubmissionState::Blocked || (state == SubmissionState::Running && !steered) {
            return self.release_at(index);
        }
        self.batches[index].prompt_id = Some(prompt_id.to_string());
        if self.consumed.remove(prompt_id) {
            // The engine reported this steer read while the receipt was still
            // in flight; the row must not stay dimmed.
            return self.release_at(index);
        }
        Vec::new()
    }

    /// The engine consumed these steers into the running turn (`turn.steer`).
    pub fn acknowledge(&mut self, prompt_ids: &[String]) -> Vec<EntryOp> {
        let mut ops = Vec::new();
        let mut delivered = Vec::new();
        for index in (0..self.batches.len()).rev() {
            let matched = self.batches[index]
                .prompt_id
                .as_ref()
                .filter(|prompt_id| prompt_ids.contains(prompt_id))
                .cloned();
            if let Some(prompt_id) = matched {
                let batch = self.batches.remove(index);
                ops.extend(pending_ops(&batch.entries, false));
                delivered.push(prompt_id);
            }
        }
        self.remember_undelivered(prompt_ids, &delivered);
        ops
    }

    /// A queued steer's own turn started, so the engine has read it.
    pub fn acknowledge_started(&mut self, prompt_id: Option<&str>) -> Vec<EntryOp> {
        match prompt_id {
            Some(prompt_id) => self.acknowledge(&[prompt_id.to_string()]),
            None => Vec::new(),
        }
    }

    /// A pending steer stopped without being read (`prompt.completed`).
    pub fn release_prompt(&mut self, prompt_id: &str) -> Vec<EntryOp> {
        let Some(index) = self
            .batches
            .iter()
            .position(|batch| batch.prompt_id.as_deref() == Some(prompt_id))
        else {
            return Vec::new();
        };
        self.release_at(index)
    }

    /// Stop showing a batch as pending after a local dispatch failure.
    pub fn release_entries(&mut self, entries: &[EntryId]) -> Vec<EntryOp> {
        let Some(index) = self.batches.iter().position(|batch| batch.holds(entries)) else {
            return Vec::new();
        };
        self.release_at(index)
    }

    /// Drop every batch (session reset / replay).
    pub fn reset(&mut self) -> Vec<EntryOp> {
        let mut ops = Vec::new();
        for batch in self.batches.drain(..) {
            ops.extend(pending_ops(&batch.entries, false));
        }
        self.consumed.clear();
        ops
    }

    /// Whether any steer is still waiting to be read. Diagnostics and tests.
    pub fn is_pending(&self) -> bool {
        !self.batches.is_empty()
    }

    fn release_at(&mut self, index: usize) -> Vec<EntryOp> {
        let batch = self.batches.remove(index);
        if self.batches.is_empty() {
            self.consumed.clear();
        }
        pending_ops(&batch.entries, false)
    }

    /// Remember acknowledgements that named an id no batch has claimed yet,
    /// but only while some batch is still waiting for its id: a `turn.started`
    /// for an ordinary prompt must not accumulate there.
    fn remember_undelivered(&mut self, prompt_ids: &[String], delivered: &[String]) {
        if self.batches.is_empty() {
            self.consumed.clear();
            return;
        }
        for prompt_id in prompt_ids {
            if !delivered.contains(prompt_id) {
                self.consumed.insert(prompt_id.clone());
            }
        }
    }
}

/// One hop a steered message passed through.
///
/// The stages are a closed vocabulary on purpose: a journal that contains a
/// stage outside a caller's expectation means the message took a detour the
/// caller did not know about (a quiescence park, a second admission, another
/// agent's queue), which is exactly what the journal exists to make visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerStage {
    /// The host turned input into a steer command.
    CommandAccepted,
    /// Admission accepted it (engine). `steered` says whether it joined the
    /// live turn instead of opening its own.
    Admitted { turn_id: u64, steered: bool },
    /// It was parked by a quiescence guard instead of admitted (engine).
    Parked { turn_id: u64 },
    /// It was pushed onto the session steer queue (engine). Emitted by the
    /// admission itself, before `Admitted`: the push is part of admitting.
    QueuePushed,
    /// The pump claimed a turn carrying this prompt id (engine). A joined
    /// steer never gets this stage — the turn it joined was claimed before it.
    TurnClaimed { turn_id: u64 },
    /// The drain consumed it into a running turn's context (engine).
    Drained,
    /// The host received the admission receipt.
    Receipt {
        turn_id: u64,
        state: SubmissionState,
        steered: bool,
    },
    /// The host allocated the optimistic row.
    RowAllocated { entry: EntryId },
    /// The engine's `turn.steer` reached the host and cleared the row.
    Acknowledged,
    /// The host stopped showing it without a read (refused or failed).
    Released,
    /// The host could not submit it at all (admission refused).
    Rejected,
}

/// One journal entry: which prompt reached which stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteerTrace {
    pub prompt_id: String,
    pub stage: SteerStage,
}

/// Append-only, process-shared record of every steer hop.
///
/// Both sides hold a clone: the engine records `Admitted` / `QueuePushed` /
/// `TurnClaimed` / `Drained`, the host records `CommandAccepted` / `Receipt` /
/// `RowAllocated` / `Acknowledged` / `Released`. One mutex orders them across
/// the two, so a reader can answer "where is prompt X now" without inferring
/// from the transcript or the event bus.
#[derive(Debug, Clone, Default)]
pub struct SteerJournal {
    entries: std::sync::Arc<std::sync::Mutex<Vec<SteerTrace>>>,
}

impl SteerJournal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, prompt_id: impl Into<String>, stage: SteerStage) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(SteerTrace {
                prompt_id: prompt_id.into(),
                stage,
            });
    }

    /// Every hop, in arrival order.
    pub fn snapshot(&self) -> Vec<SteerTrace> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The hops of one prompt, in order.
    pub fn stages_of(&self, prompt_id: &str) -> Vec<SteerStage> {
        self.snapshot()
            .into_iter()
            .filter(|trace| trace.prompt_id == prompt_id)
            .map(|trace| trace.stage)
            .collect()
    }

    /// Where a prompt is now, or `None` if this journal never saw it.
    pub fn current(&self, prompt_id: &str) -> Option<SteerStage> {
        self.stages_of(prompt_id).pop()
    }
}

fn pending_ops(entries: &[EntryId], pending: bool) -> Vec<EntryOp> {
    entries
        .iter()
        .map(|id| EntryOp::SetPending {
            id: id.clone(),
            pending,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry_id::EntryKind;

    fn entry(seq: u64) -> EntryId {
        EntryId::engine("turn-1", seq, EntryKind::UserMsg)
    }

    fn is_pending(op: &EntryOp) -> bool {
        matches!(op, EntryOp::SetPending { pending: true, .. })
    }

    #[test]
    fn a_submitted_steer_is_pending_until_the_engine_reports_it_read() {
        let mut delivery = SteerDelivery::new();
        let id = entry(0);
        assert!(is_pending(&delivery.note(vec![id.clone()])[0]));

        // Running + steered: it joined the live turn and stays pending.
        assert!(
            delivery
                .apply_submission(
                    std::slice::from_ref(&id),
                    "prompt-1",
                    SubmissionState::Running,
                    true
                )
                .is_empty()
        );
        assert!(delivery.is_pending());

        let ops = delivery.acknowledge(&["prompt-1".to_string()]);
        assert_eq!(ops, vec![EntryOp::SetPending { id, pending: false }]);
        assert!(!delivery.is_pending());
    }

    #[test]
    fn a_queued_steer_clears_when_its_own_turn_starts() {
        let mut delivery = SteerDelivery::new();
        let id = entry(0);
        delivery.note(vec![id.clone()]);
        assert!(
            delivery
                .apply_submission(
                    std::slice::from_ref(&id),
                    "prompt-2",
                    SubmissionState::Queued,
                    false
                )
                .is_empty()
        );
        let ops = delivery.acknowledge_started(Some("prompt-2"));
        assert_eq!(ops, vec![EntryOp::SetPending { id, pending: false }]);
    }

    #[test]
    fn an_acknowledgement_that_races_the_receipt_is_remembered() {
        let mut delivery = SteerDelivery::new();
        let id = entry(0);
        delivery.note(vec![id.clone()]);
        // turn.steer arrives before the submission promise resolves.
        assert!(delivery.acknowledge(&["prompt-9".to_string()]).is_empty());
        let ops = delivery.apply_submission(
            std::slice::from_ref(&id),
            "prompt-9",
            SubmissionState::Running,
            true,
        );
        assert_eq!(ops, vec![EntryOp::SetPending { id, pending: false }]);
        assert!(!delivery.is_pending());
    }

    #[test]
    fn a_prompt_that_did_not_join_a_turn_is_released_immediately() {
        let mut delivery = SteerDelivery::new();
        let id = entry(0);
        delivery.note(vec![id.clone()]);
        let ops = delivery.apply_submission(
            std::slice::from_ref(&id),
            "prompt-3",
            SubmissionState::Running,
            false,
        );
        assert_eq!(ops, vec![EntryOp::SetPending { id, pending: false }]);
        assert!(!delivery.is_pending());
    }

    #[test]
    fn a_blocked_prompt_is_released_and_its_id_is_not_remembered() {
        let mut delivery = SteerDelivery::new();
        let id = entry(0);
        delivery.note(vec![id.clone()]);
        delivery.apply_submission(
            std::slice::from_ref(&id),
            "prompt-4",
            SubmissionState::Blocked,
            false,
        );
        assert!(delivery.consumed.is_empty());
        // A later turn.started for an unrelated id is not accumulated.
        assert!(delivery.acknowledge(&["prompt-5".to_string()]).is_empty());
        assert!(delivery.consumed.is_empty());
    }

    #[test]
    fn a_local_failure_and_a_session_reset_release_the_rows() {
        let mut delivery = SteerDelivery::new();
        let first = entry(0);
        let second = entry(1);
        delivery.note(vec![first.clone()]);
        let ops = delivery.release_entries(std::slice::from_ref(&first));
        assert_eq!(
            ops,
            vec![EntryOp::SetPending {
                id: first,
                pending: false
            }]
        );

        delivery.note(vec![second.clone()]);
        let ops = delivery.reset();
        assert_eq!(
            ops,
            vec![EntryOp::SetPending {
                id: second,
                pending: false
            }]
        );
        assert!(!delivery.is_pending());
    }

    #[test]
    fn a_released_prompt_is_found_by_its_id() {
        let mut delivery = SteerDelivery::new();
        let id = entry(0);
        delivery.note(vec![id.clone()]);
        delivery.apply_submission(
            std::slice::from_ref(&id),
            "prompt-6",
            SubmissionState::Queued,
            false,
        );
        let ops = delivery.release_prompt("prompt-6");
        assert_eq!(ops, vec![EntryOp::SetPending { id, pending: false }]);
        assert!(delivery.release_prompt("prompt-6").is_empty());
    }

    #[test]
    fn the_journal_answers_where_a_prompt_is() {
        let journal = SteerJournal::new();
        journal.record(
            "prompt-1",
            SteerStage::Admitted {
                turn_id: 0,
                steered: true,
            },
        );
        journal.record("prompt-2", SteerStage::QueuePushed);
        journal.record("prompt-1", SteerStage::Drained);

        assert_eq!(
            journal.stages_of("prompt-1"),
            vec![
                SteerStage::Admitted {
                    turn_id: 0,
                    steered: true
                },
                SteerStage::Drained
            ]
        );
        assert_eq!(journal.current("prompt-1"), Some(SteerStage::Drained));
        assert_eq!(journal.current("prompt-2"), Some(SteerStage::QueuePushed));
        assert_eq!(journal.current("prompt-3"), None);
        assert_eq!(journal.snapshot().len(), 3, "both prompts share one order");
    }
}
