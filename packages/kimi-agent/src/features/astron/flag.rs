pub const XUNFEI_CODING_PLAN_FLAG_ID: &str = "xunfei_coding_plan";
pub const XUNFEI_CODING_PLAN_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_XUNFEI_CODING_PLAN";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XunfeiCodingPlanFlag {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub env: &'static str,
    pub default: bool,
    pub surface: &'static str,
}

pub const XUNFEI_CODING_PLAN_FLAG: XunfeiCodingPlanFlag = XunfeiCodingPlanFlag {
    id: XUNFEI_CODING_PLAN_FLAG_ID,
    title: "Astron (Xunfei coding plan)",
    description: "Show the Astron provider settings in /settings and enable the Astron login flow. The astron entry under [providers] stays functional regardless of this flag.",
    env: XUNFEI_CODING_PLAN_FLAG_ENV,
    default: true,
    surface: "both",
};
