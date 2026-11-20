use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::config::config::IConfigService;

use crate::tool::tool_contract::ToolSideEffect;
use crate::turn_loop::types::{FileOperation, ToolAccesses, ToolResourceAccess};

pub const SANDBOX_SECTION: &str = "sandbox";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxMode {
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "read-only")]
    ReadOnly,
    #[serde(rename = "workspace-write")]
    WorkspaceWrite,
}

impl SandboxMode {
    /// v2 `z.enum(['off', 'read-only', 'workspace-write'])`: an unknown spelling is
    /// rejected at the config boundary.
    pub fn validate(value: &str) -> Result<Self, String> {
        match value {
            "off" => Ok(Self::Off),
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            other => Err(format!(
                "invalid sandbox mode \"{other}\": expected \"off\", \"read-only\" or \"workspace-write\""
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxExecutionPolicy {
    pub mode: SandboxMode,
    pub workspace_root: String,
}

pub static SANDBOX_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "mode": {"enum": ["off", "read-only", "workspace-write"], "default": "off"}
        }
    })
});

pub fn resolve_sandbox_policy(
    config: &dyn IConfigService,
    workspace_root: &str,
) -> SandboxExecutionPolicy {
    let mode = config
        .get(SANDBOX_SECTION)
        .as_ref()
        .and_then(|section| section.get("mode"))
        .and_then(Value::as_str)
        .map(|mode| match mode {
            "read-only" => SandboxMode::ReadOnly,
            "workspace-write" => SandboxMode::WorkspaceWrite,
            _ => SandboxMode::Off,
        })
        .unwrap_or(SandboxMode::Off);
    SandboxExecutionPolicy {
        mode,
        workspace_root: workspace_root.to_string(),
    }
}

pub fn is_sandbox_backend_available() -> bool {
    false
}

pub fn sandbox_write_guard(
    policy: &SandboxExecutionPolicy,
    target_path: &str,
) -> Option<String> {
    if policy.mode == SandboxMode::Off {
        return None;
    }
    if policy.mode == SandboxMode::ReadOnly {
        return Some(t("toolsV2.sandbox.writeBlockedReadOnly", params(&[("mode", policy.mode.as_str().into())])));
    }
    if !is_path_within_workspace(target_path, &policy.workspace_root) {
        return Some(t(
            "toolsV2.sandbox.writeBlockedOutsideWorkspace",
            params(&[
                ("mode", policy.mode.as_str().into()),
                ("path", target_path.into()),
                ("workspace", policy.workspace_root.as_str().into()),
            ]),
        ));
    }
    None
}

pub fn sandbox_tool_veto(
    policy: &SandboxExecutionPolicy,
    tool_name: &str,
    side_effect: Option<ToolSideEffect>,
    accesses: Option<&ToolAccesses>,
) -> Option<String> {
    if policy.mode == SandboxMode::Off {
        return None;
    }
    let Some(side_effect) = side_effect else {
        return Some(t(
            "toolsV2.sandbox.toolBlockedUndeclared",
            params(&[("tool", tool_name.into()), ("mode", policy.mode.as_str().into())]),
        ));
    };
    match side_effect {
        ToolSideEffect::Read => None,
        ToolSideEffect::Execute => Some(t(
            "toolsV2.sandbox.codeExecutionBlocked",
            params(&[("mode", policy.mode.as_str().into())]),
        )),
        ToolSideEffect::Shell => Some(t(
            if policy.mode == SandboxMode::ReadOnly {
                "toolsV2.sandbox.shellBlockedReadOnly"
            } else {
                "toolsV2.sandbox.shellBlockedNoBackend"
            },
            params(&[("tool", tool_name.into()), ("mode", policy.mode.as_str().into())]),
        )),
        ToolSideEffect::Write => sandbox_write_veto(policy, tool_name, accesses),
    }
}

fn sandbox_write_veto(
    policy: &SandboxExecutionPolicy,
    tool_name: &str,
    accesses: Option<&ToolAccesses>,
) -> Option<String> {
    let write_paths = accesses
        .into_iter()
        .flatten()
        .filter_map(|access| match access {
            ToolResourceAccess::File(file)
                if matches!(file.operation, FileOperation::Write | FileOperation::ReadWrite) =>
            {
                Some(file.path.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if policy.mode == SandboxMode::ReadOnly {
        return Some(if write_paths.is_empty() {
            t(
                "toolsV2.sandbox.toolBlockedReadOnly",
                params(&[("tool", tool_name.into()), ("mode", policy.mode.as_str().into())]),
            )
        } else {
            t(
                "toolsV2.sandbox.writeBlockedReadOnly",
                params(&[("mode", policy.mode.as_str().into())]),
            )
        });
    }
    if write_paths.is_empty() {
        return Some(t(
            "toolsV2.sandbox.writeBlockedUnverifiable",
            params(&[("tool", tool_name.into()), ("mode", policy.mode.as_str().into())]),
        ));
    }
    for path in write_paths {
        if let Some(error) = sandbox_write_guard(policy, &path) {
            return Some(error);
        }
    }
    None
}

fn is_path_within_workspace(target_path: &str, workspace_root: &str) -> bool {
    let target = normalize_path_for_comparison(target_path);
    let root = normalize_path_for_comparison(workspace_root);
    !root.is_empty() && (target == root || target.starts_with(&format!("{root}/")))
}

fn normalize_path_for_comparison(path: &str) -> String {
    let normalized = path.replace('\\', "/").trim_end_matches('/').to_string();
    if normalized.as_bytes().get(1) == Some(&b':')
        && normalized.as_bytes().get(2) == Some(&b'/')
        && normalized.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
    {
        normalized.to_lowercase()
    } else {
        normalized
    }
}

fn params(entries: &[(&str, Value)]) -> Map<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect()
}

fn t(key: &'static str, params: Map<String, Value>) -> String {
    let params = params
        .into_iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key, value.to_string())))
        .collect();
    crate::i18n::LocalizedText::with_params(key, params).render()
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
