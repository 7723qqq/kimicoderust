pub mod progress_track;
pub mod retry;
pub mod run_turn;
pub mod tool_call_id;
pub mod tool_result_render;
pub mod tool_scheduler;
pub mod turn_step;
pub mod types;
pub mod wall_time;
pub mod watch;

/// The numeric turn index behind a wire turn id (`turn-3`, `subturn-7`).
///
/// v2's telemetry carries `turn_id` as a number — the per-agent turn index —
/// while this engine's wire ids are strings. Stripping the two known prefixes
/// is the whole mapping; anything else has no index to report and is omitted
/// rather than guessed.
pub fn turn_index(turn_id: &str) -> Option<u32> {
    turn_id
        .strip_prefix("subturn-")
        .or_else(|| turn_id.strip_prefix("turn-"))
        .and_then(|rest| rest.parse().ok())
}

#[cfg(test)]
mod turn_index_tests {
    use super::turn_index;

    /// v2's telemetry `turn_id` is a number; the engine's wire ids are strings
    /// with a known prefix, and anything else reports no index rather than a
    /// guess.
    #[test]
    fn a_wire_turn_id_maps_to_its_numeric_index() {
        assert_eq!(turn_index("turn-7"), Some(7));
        assert_eq!(turn_index("subturn-3"), Some(3));
        assert_eq!(turn_index("turn-0"), Some(0));
        assert_eq!(turn_index("turn-"), None);
        assert_eq!(turn_index("turn-x"), None);
        assert_eq!(turn_index("test-compaction-cap"), None);
        assert_eq!(turn_index(""), None);
    }
}
