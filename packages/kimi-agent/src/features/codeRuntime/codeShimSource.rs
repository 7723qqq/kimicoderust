pub const CODE_SHIM_SUBCOMMAND: &str = "__run_code";
pub const CODE_SHIM_SOURCE: &str = include_str!("../../workflow/run_code_prelude.js");
pub const CODE_SHIM_BODY_MARKER: &str = "/*__RUN_CODE_BODY__*/";

pub fn run_code_shim_main(code: &str) -> String {
    CODE_SHIM_SOURCE.replace(CODE_SHIM_BODY_MARKER, code)
}
