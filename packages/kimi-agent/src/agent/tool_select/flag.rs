use std::sync::LazyLock;

use crate::app::flag::flag_registry::{FlagDefinitionInput, FlagSurface, register_flag_definition};

pub use crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME;

pub const TOOL_SELECT_FLAG_ID: &str = "tool_select";
pub const TOOL_SELECT_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_TOOL_SELECT";

pub static TOOL_SELECT_FLAG: LazyLock<FlagDefinitionInput> = LazyLock::new(|| FlagDefinitionInput {
    id: TOOL_SELECT_FLAG_ID.to_string(),
    title: "Tool select (progressive tool disclosure)".to_string(),
    description: "Keep MCP tool schemas out of the immutable top-level tools[]; the model loads them on demand via the select_tools tool. Only takes effect on models whose capability catalog declares dynamically loaded tools.".to_string(),
    env: TOOL_SELECT_FLAG_ENV.to_string(),
    default: true,
    surface: FlagSurface::Core,
    is_exposed: None,
});

pub fn register_tool_select_flag() {
    register_flag_definition(TOOL_SELECT_FLAG.clone());
}
