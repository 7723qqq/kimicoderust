use serde::{Deserialize, Serialize};

use crate::i18n::{LocalizedText, i18n_params};
use crate::turn_loop::types::{FileOperation, ToolAccesses, ToolResourceAccess};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    #[default]
    Off,
    ReadOnly,
    WorkspaceWrite,
}

impl<'de> Deserialize<'de> for SandboxMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        SandboxMode::validate(&raw).map_err(serde::de::Error::custom)
    }
}

impl SandboxMode {
    const ACCEPTED_SPELLINGS: &str =
        "\"off\", \"read-only\" (or \"readonly\"), \"workspace-write\" (or \"workspacewrite\")";

    pub fn validate(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(SandboxMode::Off),
            "read-only" | "readonly" => Ok(SandboxMode::ReadOnly),
            "workspace-write" | "workspacewrite" => Ok(SandboxMode::WorkspaceWrite),
            _ => Err(format!(
                "unknown sandbox mode {s:?} (accepted: {})",
                Self::ACCEPTED_SPELLINGS
            )),
        }
    }

    pub fn parse(s: &str) -> Self {
        match Self::validate(s) {
            Ok(mode) => mode,
            Err(error) => {
                tracing::warn!("{error}; using the most restrictive mode instead");
                SandboxMode::ReadOnly
            }
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxMode::Off => "off",
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSideEffect {
    Read,
    Write,
    Shell,
    Execute,
}

pub fn tool_side_effect(tool_name: &str) -> Option<ToolSideEffect> {
    let name = tool_name.to_ascii_lowercase();
    if name.starts_with("mcp__") {
        return None;
    }
    if name.starts_with("github") {
        return Some(ToolSideEffect::Write);
    }
    match name.as_str() {
        "read" | "grep" | "glob" | "fetchurl" | "fetch_url" | "websearch" | "web_search"
        | "lsp" | "invokesubagent" | "invoke_subagent" | "managesubagents"
        | "manage_subagents" | "definesubagent" | "define_subagent" | "askuserquestion"
        | "ask_user_question" | "getgoal" | "get_goal" | "todolist" | "todo_list"
        | "cronlist" | "cron_list" | "updategoal" | "update_goal" | "setgoalbudget"
        | "set_goal_budget" | "tasklist" | "task_list" | "taskoutput" | "task_output"
        | "taskwait" | "task_wait" | "exitplanmode" | "exit_plan_mode" | "enterspecmode"
        | "enter_spec_mode" | "exitspecmode" | "exit_spec_mode" | "creategoal"
        | "create_goal" | "skill" | "sessionquery" | "session_query" | "notifyuser"
        | "notify_user" | "agent" | "agentswarm" | "agent_swarm" | "waitfor" | "wait_for"
        | "towerinit" | "tower_init" | "towerplan" | "tower_plan" | "towerspawn"
        | "tower_spawn" | "towersend" | "tower_send" | "towerinbox" | "tower_inbox"
        | "towerfinding" | "tower_finding" | "towerreview" | "tower_review"
        | "towermission" | "tower_mission" | "towerstatus" | "tower_status"
        | "select_tools" => Some(ToolSideEffect::Read),
        "write" | "edit" | "knowledge" | "memory" | "taskstop" | "task_stop" | "croncreate"
        | "cron_create" | "crondelete" | "cron_delete" | "towermerge" | "tower_merge"
        | "towerteardown" | "tower_teardown" => Some(ToolSideEffect::Write),
        "bash" | "powershell" | "workflow" => Some(ToolSideEffect::Shell),
        "runcode" | "run_code" => Some(ToolSideEffect::Execute),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxExecutionPolicy {
    pub mode: SandboxMode,
    pub workspace_root: String,

    #[serde(default)]
    pub extra_roots: Vec<String>,
}

impl SandboxExecutionPolicy {
    pub fn new(mode: SandboxMode, workspace_root: impl Into<String>) -> Self {
        Self {
            mode,
            workspace_root: workspace_root.into(),
            extra_roots: Vec::new(),
        }
    }

    pub fn off(workspace_root: impl Into<String>) -> Self {
        Self::new(SandboxMode::Off, workspace_root)
    }

    pub fn read_only(workspace_root: impl Into<String>) -> Self {
        Self::new(SandboxMode::ReadOnly, workspace_root)
    }

    pub fn workspace_write(workspace_root: impl Into<String>) -> Self {
        Self::new(SandboxMode::WorkspaceWrite, workspace_root)
    }

    pub fn with_extra_roots<I, S>(mut self, roots: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extra_roots = roots
            .into_iter()
            .map(Into::into)
            .filter(|root| !normalize_path_for_comparison(root).is_empty())
            .collect();
        self
    }

    pub fn authorized_roots(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.workspace_root.as_str())
            .chain(self.extra_roots.iter().map(String::as_str))
    }

    pub fn is_path_authorized(&self, target_path: &str) -> bool {
        self.authorized_roots()
            .any(|root| is_path_within_workspace(target_path, root))
    }

    fn roots_summary(&self) -> String {
        if self.extra_roots.is_empty() {
            return format!("\"{}\"", self.workspace_root);
        }
        self.authorized_roots()
            .map(|root| format!("\"{root}\""))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn sandbox_write_guard(&self, target_path: &str) -> Option<String> {
        if self.mode == SandboxMode::Off {
            return None;
        }

        if self.mode == SandboxMode::ReadOnly {
            return Some(
                LocalizedText::with_params(
                    "engine.tools.sandbox.writeBlockedReadOnly",
                    i18n_params!["mode" => self.mode.as_str()],
                )
                .render(),
            );
        }

        if !self.is_path_authorized(target_path) {
            return Some(if self.extra_roots.is_empty() {
                LocalizedText::with_params(
                    "engine.tools.sandbox.writeBlockedOutsideWorkspace",
                    i18n_params![
                        "mode" => self.mode.as_str(),
                        "workspace" => self.workspace_root.as_str(),
                        "path" => target_path,
                    ],
                )
                .render()
            } else {
                format!(
                    "Sandbox mode \"{}\" blocks writes outside the authorized roots ({}). Target path \"{}\" is outside them.",
                    self.mode.as_str(),
                    self.roots_summary(),
                    target_path
                )
            });
        }

        None
    }

    pub fn sandbox_tool_veto(
        &self,
        tool_name: &str,
        side_effect: Option<ToolSideEffect>,
        accesses: &ToolAccesses,
    ) -> Option<String> {
        if self.mode == SandboxMode::Off {
            return None;
        }

        let Some(side_effect) = side_effect else {
            return Some(
                LocalizedText::with_params(
                    "engine.tools.sandbox.toolBlockedUndeclared",
                    i18n_params![
                        "mode" => self.mode.as_str(),
                        "tool" => tool_name,
                    ],
                )
                .render(),
            );
        };

        match side_effect {
            ToolSideEffect::Read => None,
            ToolSideEffect::Execute => Some(
                LocalizedText::with_params(
                    "engine.tools.sandbox.codeExecutionBlocked",
                    i18n_params!["mode" => self.mode.as_str()],
                )
                .render(),
            ),
            ToolSideEffect::Shell => Some(match self.mode {
                SandboxMode::ReadOnly => LocalizedText::with_params(
                    "engine.tools.sandbox.shellBlockedReadOnly",
                    i18n_params!["mode" => self.mode.as_str(), "tool" => tool_name],
                )
                .render(),
                _ => LocalizedText::with_params(
                    "engine.tools.sandbox.shellBlockedNoBackend",
                    i18n_params!["mode" => self.mode.as_str(), "tool" => tool_name],
                )
                .render(),
            }),
            ToolSideEffect::Write => self.sandbox_write_veto(tool_name, accesses),
        }
    }

    fn sandbox_write_veto(&self, tool_name: &str, accesses: &ToolAccesses) -> Option<String> {
        let write_paths: Vec<&str> = accesses
            .iter()
            .filter_map(|access| match access {
                ToolResourceAccess::File(file)
                    if matches!(
                        file.operation,
                        FileOperation::Write | FileOperation::ReadWrite
                    ) =>
                {
                    Some(file.path.as_str())
                }
                _ => None,
            })
            .collect();

        if self.mode == SandboxMode::ReadOnly {
            return Some(if write_paths.is_empty() {
                LocalizedText::with_params(
                    "engine.tools.sandbox.toolBlockedReadOnly",
                    i18n_params![
                        "mode" => self.mode.as_str(),
                        "tool" => tool_name,
                    ],
                )
                .render()
            } else {
                LocalizedText::with_params(
                    "engine.tools.sandbox.writeBlockedReadOnly",
                    i18n_params!["mode" => self.mode.as_str()],
                )
                .render()
            });
        }

        if write_paths.is_empty() {
            return Some(
                LocalizedText::with_params(
                    "engine.tools.sandbox.writeBlockedUnverifiable",
                    i18n_params![
                        "mode" => self.mode.as_str(),
                        "tool" => tool_name,
                    ],
                )
                .render(),
            );
        }

        write_paths
            .iter()
            .find_map(|path| self.sandbox_write_guard(path))
    }
}

pub fn normalize_path_for_comparison(path: &str) -> String {
    let replaced = path.replace('\\', "/");
    let has_leading_slash = replaced.starts_with('/');

    let (prefix, rest) = if replaced.len() >= 2
        && replaced.as_bytes()[0].is_ascii_alphabetic()
        && replaced.as_bytes()[1] == b':'
    {
        let drive = (replaced.as_bytes()[0] as char).to_ascii_lowercase();
        (format!("{drive}:"), &replaced[2..])
    } else {
        (String::new(), replaced.as_str())
    };

    let mut segments = Vec::new();
    for part in rest.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            segments.pop();
        } else {
            segments.push(part);
        }
    }

    let joined = segments.join("/");
    if !prefix.is_empty() {
        if joined.is_empty() {
            prefix
        } else {
            format!("{prefix}/{joined}")
        }
    } else if has_leading_slash {
        format!("/{joined}")
    } else {
        joined
    }
}

pub fn is_path_within_workspace(target_path: &str, workspace_root: &str) -> bool {
    let norm_target = normalize_path_for_comparison(target_path);
    let norm_root = normalize_path_for_comparison(workspace_root);
    if norm_root.is_empty() {
        return false;
    }
    if norm_target == norm_root {
        return true;
    }
    let prefix = format!("{}/", norm_root);
    norm_target.starts_with(&prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_access(path: &str) -> ToolAccesses {
        vec![ToolResourceAccess::File(crate::turn_loop::types::ToolFileAccess {
            operation: FileOperation::Write,
            path: path.to_string(),
            recursive: false,
        })]
    }

    #[test]
    fn test_sandbox_mode_parsing() {
        for (input, expected) in [
            ("off", SandboxMode::Off),
            ("OFF", SandboxMode::Off),
            ("  off  ", SandboxMode::Off),
            ("read-only", SandboxMode::ReadOnly),
            ("Read-Only", SandboxMode::ReadOnly),
            ("readonly", SandboxMode::ReadOnly),
            ("workspace-write", SandboxMode::WorkspaceWrite),
            ("WorkSpace-Write ", SandboxMode::WorkspaceWrite),
            ("workspacewrite", SandboxMode::WorkspaceWrite),
        ] {
            assert_eq!(SandboxMode::validate(input).unwrap(), expected, "{input}");
        }
        assert_eq!(SandboxMode::parse("bogus"), SandboxMode::ReadOnly);
    }

    #[test]
    fn test_write_guard_read_only_blocks_writes() {
        let policy = SandboxExecutionPolicy::read_only("/workspace");
        let denial = policy.sandbox_write_guard("/workspace/a.txt").unwrap();
        assert!(denial.contains("blocks writes to the filesystem"));
    }

    #[test]
    fn test_write_guard_workspace_write_allows_inside() {
        let policy = SandboxExecutionPolicy::workspace_write("/workspace");
        assert!(
            policy
                .sandbox_write_guard("/workspace/sub/a.txt")
                .is_none()
        );
        let denial = policy.sandbox_write_guard("/other/a.txt").unwrap();
        assert!(denial.contains("outside the workspace root"));
    }

    #[test]
    fn test_extra_roots_are_authorized() {
        let policy =
            SandboxExecutionPolicy::workspace_write("/workspace").with_extra_roots(["/scratch"]);
        assert!(policy.sandbox_write_guard("/scratch/a.txt").is_none());
        let denial = policy.sandbox_write_guard("/elsewhere/a.txt").unwrap();
        assert!(denial.contains("outside the authorized roots"));
        assert!(denial.contains("\"/workspace\", \"/scratch\""));
    }

    #[test]
    fn test_windows_path_normalization() {
        assert_eq!(
            normalize_path_for_comparison(r"C:\Workspace\.\sub\..\a.txt"),
            "c:/Workspace/a.txt"
        );
        assert!(is_path_within_workspace(
            r"C:\Workspace\sub\a.txt",
            "C:/Workspace"
        ));
        assert!(!is_path_within_workspace(
            r"C:\Workspace2\a.txt",
            "C:/Workspace"
        ));
    }

    #[test]
    fn test_off_mode_never_vetoes() {
        let policy = SandboxExecutionPolicy::off("/workspace");
        assert!(
            policy
                .sandbox_tool_veto("Bash", tool_side_effect("Bash"), &ToolAccesses::new())
                .is_none()
        );
        assert!(
            policy
                .sandbox_tool_veto("mcp__x__y", None, &ToolAccesses::new())
                .is_none()
        );
    }

    #[test]
    fn test_read_tools_are_allowed_in_every_mode() {
        for policy in [
            SandboxExecutionPolicy::read_only("/workspace"),
            SandboxExecutionPolicy::workspace_write("/workspace"),
        ] {
            assert_eq!(tool_side_effect("Read"), Some(ToolSideEffect::Read));
            assert!(
                policy
                    .sandbox_tool_veto("Read", tool_side_effect("Read"), &ToolAccesses::new())
                    .is_none()
            );
        }
    }

    #[test]
    fn test_undeclared_tools_fail_closed() {
        let policy = SandboxExecutionPolicy::read_only("/workspace");
        let denial = policy.sandbox_tool_veto("mcp__x__y", None, &ToolAccesses::new()).unwrap();
        assert!(denial.contains("does not declare whether it has side effects"));
    }

    #[test]
    fn test_shell_tools_are_refused_without_a_backend() {
        let policy = SandboxExecutionPolicy::read_only("/workspace");
        let denial = policy
            .sandbox_tool_veto("Bash", tool_side_effect("Bash"), &ToolAccesses::new())
            .unwrap();
        assert!(denial.contains("cannot be proven write-free"));

        let policy = SandboxExecutionPolicy::workspace_write("/workspace");
        let denial = policy
            .sandbox_tool_veto("PowerShell", tool_side_effect("PowerShell"), &ToolAccesses::new())
            .unwrap();
        assert!(denial.contains("none is available on this host"));
    }

    #[test]
    fn test_code_execution_is_refused() {
        let policy = SandboxExecutionPolicy::workspace_write("/workspace");
        let denial = policy
            .sandbox_tool_veto("run_code", tool_side_effect("run_code"), &ToolAccesses::new())
            .unwrap();
        assert!(denial.contains("blocks code execution"));
    }

    #[test]
    fn test_write_tools_check_their_declared_paths() {
        let policy = SandboxExecutionPolicy::workspace_write("/workspace");
        assert!(
            policy
                .sandbox_tool_veto("Write", tool_side_effect("Write"), &write_access("/workspace/a"))
                .is_none()
        );
        let denial = policy
            .sandbox_tool_veto("Write", tool_side_effect("Write"), &write_access("/other/a"))
            .unwrap();
        assert!(denial.contains("outside the workspace root"));

        let policy = SandboxExecutionPolicy::read_only("/workspace");
        let denial = policy
            .sandbox_tool_veto("Write", tool_side_effect("Write"), &write_access("/workspace/a"))
            .unwrap();
        assert!(denial.contains("blocks writes to the filesystem"));

        let denial = policy
            .sandbox_tool_veto("Write", tool_side_effect("Write"), &ToolAccesses::new())
            .unwrap();
        assert!(denial.contains("allows read-only tools only"));
    }
}
