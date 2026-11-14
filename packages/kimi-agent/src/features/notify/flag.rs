pub const NOTIFY_USER_FLAG_ID: &str = "notify_user";
pub const NOTIFY_USER_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_NOTIFY_USER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotifyUserFlag {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub env: &'static str,
    pub default: bool,
    pub surface: &'static str,
}

pub const NOTIFY_USER_FLAG: NotifyUserFlag = NotifyUserFlag {
    id: NOTIFY_USER_FLAG_ID,
    title: "NotifyUser tool",
    description: "Show live progress updates from the main agent and subagents: the Updates panel in the TUI and progress rows in desktop and web.",
    env: NOTIFY_USER_FLAG_ENV,
    default: false,
    surface: "core",
};
