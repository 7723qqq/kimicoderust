const MCP_NAME_PREFIX: &str = "mcp__";
const MCP_NAME_SEPARATOR: &str = "__";
const MAX_QUALIFIED_LENGTH: usize = 64;

pub fn sanitize_mcp_name_part(part: &str) -> String {
    let mut out = String::with_capacity(part.len());
    let mut prev_underscore = false;
    for &b in part.as_bytes() {
        let safe = b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
        if safe {
            if b == b'_' && prev_underscore {
                continue;
            }
            out.push(b as char);
            prev_underscore = b == b'_';
        } else {
            if !prev_underscore {
                out.push('_');
                prev_underscore = true;
            }
        }
    }
    out
}

pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with(MCP_NAME_PREFIX)
}

pub fn qualify_mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    let sanitized_server = sanitize_mcp_name_part(server_name);
    let sanitized_tool = sanitize_mcp_name_part(tool_name);
    let full = format!(
        "{}{}{}{}",
        MCP_NAME_PREFIX, sanitized_server, MCP_NAME_SEPARATOR, sanitized_tool
    );

    if full.len() <= MAX_QUALIFIED_LENGTH {
        return full;
    }

    let hash = stable_hash8(&full);
    let keep = MAX_QUALIFIED_LENGTH - hash.len() - 1;
    format!("{}_{}", &full[..keep], hash)
}

fn stable_hash8(input: &str) -> String {
    let mut bits: u32 = 0x811c_9dc5;
    for ch in input.chars() {
        bits ^= ch as u32;
        bits = bits.wrapping_mul(0x0100_0193);
    }

    if input.is_empty() {
        return format!("{bits:08x}");
    }
    let signed = bits as i32;
    let text = if signed < 0 {
        format!("-{:x}", signed.unsigned_abs())
    } else {
        format!("{signed:x}")
    };
    format!("{text:0>8}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_basic() {
        assert_eq!(sanitize_mcp_name_part("hello"), "hello");
        assert_eq!(sanitize_mcp_name_part("my-tool"), "my-tool");
        assert_eq!(sanitize_mcp_name_part("tool_123"), "tool_123");
    }

    #[test]
    fn test_sanitize_special_chars() {
        assert_eq!(sanitize_mcp_name_part("my.tool"), "my_tool");
        assert_eq!(sanitize_mcp_name_part("my tool"), "my_tool");
        assert_eq!(sanitize_mcp_name_part("a@b#c"), "a_b_c");
    }

    #[test]
    fn test_sanitize_collapse_underscores() {
        assert_eq!(sanitize_mcp_name_part("a..b"), "a_b");
        assert_eq!(sanitize_mcp_name_part("a@#b"), "a_b");
        assert_eq!(sanitize_mcp_name_part("a b c"), "a_b_c");
    }

    #[test]
    fn test_sanitize_cjk() {
        assert_eq!(sanitize_mcp_name_part("工具"), "_");
        assert_eq!(sanitize_mcp_name_part("my工具"), "my_");
    }

    #[test]
    fn test_sanitize_matches_ts_contract() {
        assert_eq!(sanitize_mcp_name_part("github_v2-alpha"), "github_v2-alpha");
        assert_eq!(sanitize_mcp_name_part("My Search/Tool!"), "My_Search_Tool_");
        assert_eq!(sanitize_mcp_name_part("@scope/pkg.tool"), "_scope_pkg_tool");
        assert_eq!(sanitize_mcp_name_part("my__server"), "my_server");
        assert_eq!(sanitize_mcp_name_part("a   b"), "a_b");
        assert_eq!(sanitize_mcp_name_part("list..__issues"), "list_issues");
    }

    #[test]
    fn test_is_mcp_tool_name() {
        assert!(is_mcp_tool_name("mcp__server__tool"));
        assert!(!is_mcp_tool_name("read"));
        assert!(!is_mcp_tool_name("mcp_tool"));
        assert!(is_mcp_tool_name("mcp__s__t"));
    }

    #[test]
    fn test_qualify_short() {
        let result = qualify_mcp_tool_name("myserver", "mytool");
        assert_eq!(result, "mcp__myserver__mytool");
        assert!(result.len() <= MAX_QUALIFIED_LENGTH);
    }

    #[test]
    fn test_qualify_matches_ts_contract() {
        assert_eq!(
            qualify_mcp_tool_name("github", "list_issues"),
            "mcp__github__list_issues"
        );
        assert_eq!(
            qualify_mcp_tool_name("My Search", "do.thing"),
            "mcp__My_Search__do_thing"
        );

        assert_eq!(
            qualify_mcp_tool_name("my__server", "foo"),
            "mcp__my_server__foo"
        );
        assert_eq!(
            qualify_mcp_tool_name("gh", "list__issues"),
            "mcp__gh__list_issues"
        );
    }

    #[test]
    fn test_qualify_long() {
        let long_server = "a".repeat(40);
        let long_tool = "b".repeat(40);
        let result = qualify_mcp_tool_name(&long_server, &long_tool);
        assert_eq!(
            result,
            "mcp__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa__bbbbbbb_-785fd989"
        );
        assert_eq!(result.len(), MAX_QUALIFIED_LENGTH);
        assert!(result.starts_with(MCP_NAME_PREFIX));
    }

    #[test]
    fn test_qualify_long_differentiates_servers() {
        let tool = "x".repeat(40);
        let from_a = qualify_mcp_tool_name(&"a".repeat(40), &tool);
        let from_b = qualify_mcp_tool_name(&"b".repeat(40), &tool);
        assert_eq!(
            from_a,
            "mcp__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa__xxxxxxx_-42797fe9"
        );
        assert_eq!(
            from_b,
            "mcp__bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__xxxxxxx_-7dafadb1"
        );
        assert_ne!(from_a, from_b);
    }

    #[test]
    fn test_stable_hash_pads_a_short_negative_like_js() {
        assert_eq!(stable_hash8("mcp__s190__t190"), "0-53fcf0");

        let long_full = format!("mcp__{}__{}", "a".repeat(40), "b".repeat(40));
        assert_eq!(stable_hash8(&long_full), "-785fd989");
    }

    #[test]
    fn test_qualify_deterministic() {
        let server = "very_long_server_name_that_exceeds_limit";
        let tool = "very_long_tool_name_that_also_exceeds";
        let r1 = qualify_mcp_tool_name(server, tool);
        let r2 = qualify_mcp_tool_name(server, tool);
        assert_eq!(r1, r2, "hash must be deterministic");
    }

    #[test]
    fn test_qualify_sanitizes() {
        let result = qualify_mcp_tool_name("my.server", "my.tool");
        assert_eq!(result, "mcp__my_server__my_tool");
    }

    #[test]
    fn test_hash_format() {
        assert_eq!(stable_hash8(""), "811c9dc5");

        assert!(stable_hash8("mcp__a__b").starts_with('-'));
    }
}
