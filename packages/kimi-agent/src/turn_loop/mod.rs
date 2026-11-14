pub use crate::agent::progress_track::progress_tracker_service as progress_track;
pub(crate) mod interrupted;
pub mod retry;
pub mod run_turn;
pub use crate::agent::test_scope_guard::test_scope_guard;
pub use kimi_core::tool_call_id;
pub mod tool_result_render;
pub mod tool_scheduler;
pub mod turn_step;
pub mod types;
pub mod wall_time;
pub use kimi_core::watch;

pub fn turn_index(turn_id: &str) -> Option<u32> {
    turn_id
        .strip_prefix("subturn-")
        .or_else(|| turn_id.strip_prefix("turn-"))
        .and_then(|rest| rest.parse().ok())
}

#[cfg(test)]
mod turn_index_tests {
    use super::turn_index;

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
