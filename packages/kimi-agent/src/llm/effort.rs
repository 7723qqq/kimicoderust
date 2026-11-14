pub(crate) fn wire_reasoning_effort(effort: Option<&str>) -> Option<&str> {
    effort.filter(|value| !value.is_empty() && *value != "off" && *value != "on")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_declared_efforts_verbatim() {
        for effort in [
            "low", "medium", "high", "xhigh", "max", "minimal", "none", "4096",
        ] {
            assert_eq!(
                wire_reasoning_effort(Some(effort)),
                Some(effort),
                "{effort} is a declared effort and must reach the wire unchanged"
            );
        }
    }

    #[test]
    fn drops_the_host_level_tokens() {
        assert_eq!(wire_reasoning_effort(Some("on")), None);
        assert_eq!(wire_reasoning_effort(Some("off")), None);
        assert_eq!(wire_reasoning_effort(Some("")), None);
        assert_eq!(wire_reasoning_effort(None), None);
    }

    #[test]
    fn does_not_trim_or_lowercase() {
        assert_eq!(wire_reasoning_effort(Some("On")), Some("On"));
        assert_eq!(wire_reasoning_effort(Some(" on ")), Some(" on "));
    }
}
