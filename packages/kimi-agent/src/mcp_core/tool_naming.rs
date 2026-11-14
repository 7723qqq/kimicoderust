const MCP_NAME_PREFIX: &str = "mcp__";
const MCP_NAME_SEPARATOR: &str = "__";
const MAX_QUALIFIED_LENGTH: usize = 64;

pub fn sanitize_mcp_name_part(part: &str) -> String {
    let mut output = String::new();
    let mut underscore = false;
    for ch in part.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            output.push(ch);
            underscore = false;
        } else if !underscore {
            output.push('_');
            underscore = true;
        }
    }
    output
}

pub fn qualify_mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    let full = format!(
        "{MCP_NAME_PREFIX}{}{MCP_NAME_SEPARATOR}{}",
        sanitize_mcp_name_part(server_name),
        sanitize_mcp_name_part(tool_name)
    );
    if full.encode_utf16().count() <= MAX_QUALIFIED_LENGTH {
        return full;
    }
    let hash = stable_hash8(&full);
    let units = full
        .encode_utf16()
        .take(MAX_QUALIFIED_LENGTH - hash.len() - 1)
        .collect::<Vec<_>>();
    let head = String::from_utf16_lossy(&units);
    format!("{head}_{hash}")
}

fn stable_hash8(input: &str) -> String {
    let mut hash = 0x811c9dc5i32;
    for ch in input.chars() {
        hash ^= ch as i32;
        hash = hash.wrapping_mul(0x01000193);
    }
    let rendered = if hash < 0 {
        format!("-{:x}", hash.unsigned_abs())
    } else {
        format!("{:x}", hash)
    };
    if rendered.len() < 8 {
        format!("{}{}", "0".repeat(8 - rendered.len()), rendered)
    } else {
        rendered
    }
}
