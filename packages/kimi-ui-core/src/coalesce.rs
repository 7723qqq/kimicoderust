//! Engine-side delta coalescing policy: the tunables behind the merged-delta
//! batching contract in [`crate::entry_op`].
//!
//! Decisions baked into the contract (fixed):
//!
//! - **Where merging happens**: engine side, before emission. One merger, every
//!   UI benefits; merging in each UI's projection would fork the policy.
//! - **Merge unit**: the pending buffer flushes when *any* trigger fires —
//!   `flush_interval` elapsed, `max_pending_bytes` exceeded, or a turn/step
//!   lifecycle boundary (`flush_on_boundary`). Segments are therefore sized by
//!   time *or* volume, whichever comes first, and always closed at boundaries.
//! - **Ordering**: by [`crate::entry_op::SeqSpan`], not arrival order.
//! - **Tail**: boundary flushing emits the `is_final` segment before the
//!   sealing snapshot event, so no bytes are left in the buffer at seal time.
//!
//! The numbers below are *defaults*, not commitments — batching parameters are
//! explicitly deferred to measurement-driven tuning (the 2026-11 measurement:
//! ~3860 Rust→JS crossings / ~232KB serialized per typical turn). Hosts
//! override them through the constructor or serde.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Tunables for the engine-side delta coalescer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoalescingPolicy {
    /// Flush the pending buffer at least this often while deltas keep
    /// arriving. Default 16ms ≈ one terminal frame.
    pub flush_interval: Duration,
    /// Flush immediately once pending text exceeds this many bytes. Default
    /// 8 KiB.
    pub max_pending_bytes: usize,
    /// Always flush at turn/step lifecycle boundaries. This is what delivers
    /// the `is_final` segment before the sealing event; disabling it
    /// reintroduces tail loss and should never ship.
    pub flush_on_boundary: bool,
}

impl Default for CoalescingPolicy {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_millis(16),
            max_pending_bytes: 8 * 1024,
            flush_on_boundary: true,
        }
    }
}

impl CoalescingPolicy {
    /// Full configuration entry point for hosts.
    pub const fn new(
        flush_interval: Duration,
        max_pending_bytes: usize,
        flush_on_boundary: bool,
    ) -> Self {
        Self {
            flush_interval,
            max_pending_bytes,
            flush_on_boundary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane_and_tunable() {
        let policy = CoalescingPolicy::default();
        assert!(policy.flush_interval > Duration::ZERO);
        assert!(policy.max_pending_bytes > 0);
        assert!(policy.flush_on_boundary);

        let custom = CoalescingPolicy::new(Duration::from_millis(50), 64 * 1024, true);
        assert_eq!(custom.flush_interval, Duration::from_millis(50));
        assert_eq!(custom.max_pending_bytes, 64 * 1024);
    }

    #[test]
    fn serde_round_trip() {
        let policy = CoalescingPolicy::default();
        let json = serde_json::to_value(policy).unwrap();
        let back: CoalescingPolicy = serde_json::from_value(json).unwrap();
        assert_eq!(back, policy);
    }
}
