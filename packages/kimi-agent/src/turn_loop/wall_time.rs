const WALL_TIME_TOOL_NAMES: &[&str] = &[
    "Agent",
    "AgentSwarm",
    "Bash",
    "FetchURL",
    "Glob",
    "Grep",
    "WebSearch",
];

pub fn should_render_wall_time(tool_name: &str) -> bool {
    WALL_TIME_TOOL_NAMES.contains(&tool_name) || tool_name.starts_with("mcp__")
}

pub fn format_wall_time_ms(duration_ms: u64) -> String {
    format!("{:.3} seconds", duration_ms as f64 / 1000.0)
}

pub fn wall_time_header(duration_ms: u64) -> String {
    format!("Wall time: {}", format_wall_time_ms(duration_ms))
}

pub fn prepend_wall_time(content: &str, duration_ms: u64) -> String {
    format!("{}\n{}", wall_time_header(duration_ms), content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_render_wall_time_set_and_mcp() {
        for name in [
            "Agent",
            "AgentSwarm",
            "Bash",
            "FetchURL",
            "Glob",
            "Grep",
            "WebSearch",
        ] {
            assert!(should_render_wall_time(name), "{name}");
        }
        assert!(should_render_wall_time("mcp__github__search"));
        assert!(!should_render_wall_time("Read"));
        assert!(!should_render_wall_time("Write"));
        assert!(!should_render_wall_time("TaskList"));
        assert!(!should_render_wall_time("mcp"));
    }

    #[test]
    fn test_format_wall_time_ms() {
        assert_eq!(format_wall_time_ms(0), "0.000 seconds");
        assert_eq!(format_wall_time_ms(1), "0.001 seconds");
        assert_eq!(format_wall_time_ms(1234), "1.234 seconds");
        assert_eq!(format_wall_time_ms(60_000), "60.000 seconds");
    }

    #[test]
    fn test_prepend_wall_time() {
        assert_eq!(
            prepend_wall_time("output", 1500),
            "Wall time: 1.500 seconds\noutput"
        );
        assert_eq!(prepend_wall_time("", 0), "Wall time: 0.000 seconds\n");
    }
}
