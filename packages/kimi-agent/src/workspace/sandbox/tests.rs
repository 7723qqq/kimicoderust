//! v2 `test/workspace/sandbox/sandbox.test.ts`, ported case for case.
//!
//! Behaviour is asserted against the same messages v2 renders, so the i18n
//! keys stay honest: `'[sandbox]'`, `'fail closed'`, the mode spelling and the
//! tool/path names all have to survive rendering.

use serde_json::{Value, json};

use super::{
    SandboxExecutionPolicy, SandboxMode, SANDBOX_SECTION, is_sandbox_backend_available,
    resolve_sandbox_policy, sandbox_tool_veto, sandbox_write_guard,
};
use crate::app::config::config::{
    ConfigChangeListener, ConfigDiagnosticsListener, ConfigInspectValue, ConfigSectionChangeListener,
    ConfigTarget, IConfigService, ResolvedConfig,
};
use crate::_base::di::lifecycle::DisposableRef;
use crate::tool::tool_contract::{ToolAccesses, ToolSideEffect};
use crate::turn_loop::types::{FileOperation, ToolAccesses as ToolAccessesList, ToolFileAccess, ToolResourceAccess};
use crate::wire::record::WireAsyncError;

/// v2 `configWith(mode)` stub: only `get(SANDBOX_SECTION)` is exercised.
struct ConfigStub {
    section: Option<Value>,
}

#[async_trait::async_trait]
impl IConfigService for ConfigStub {
    fn get(&self, domain: &str) -> Option<Value> {
        if domain == SANDBOX_SECTION {
            self.section.clone()
        } else {
            None
        }
    }

    async fn ready(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }

    fn on_did_change_configuration(&self, _listener: ConfigChangeListener) -> DisposableRef {
        unimplemented!()
    }

    fn on_did_section_change(&self, _listener: ConfigSectionChangeListener) -> DisposableRef {
        unimplemented!()
    }

    fn on_did_change_diagnostics(&self, _listener: ConfigDiagnosticsListener) -> DisposableRef {
        unimplemented!()
    }

    async fn assert_valid(&self, _domain: &str) -> Result<(), WireAsyncError> {
        Ok(())
    }

    fn inspect(&self, _domain: &str) -> ConfigInspectValue {
        unimplemented!()
    }

    fn get_all(&self) -> ResolvedConfig {
        unimplemented!()
    }

    async fn set(
        &self,
        _domain: &str,
        _patch: Option<Value>,
        _target: Option<ConfigTarget>,
    ) -> Result<(), WireAsyncError> {
        unimplemented!()
    }

    async fn replace(
        &self,
        _domain: &str,
        _value: Option<Value>,
        _target: Option<ConfigTarget>,
    ) -> Result<(), WireAsyncError> {
        unimplemented!()
    }

    async fn replace_sections(
        &self,
        _sections: std::collections::BTreeMap<String, Option<Value>>,
        _target: Option<ConfigTarget>,
        _options: Option<crate::app::config::config::ConfigReplaceSectionsOptions>,
    ) -> Result<(), WireAsyncError> {
        unimplemented!()
    }

    async fn reload(&self) -> Result<(), WireAsyncError> {
        unimplemented!()
    }

    fn diagnostics(&self) -> Vec<crate::app::config::config::ConfigDiagnostic> {
        unimplemented!()
    }
}

fn config_with(mode: Option<&str>) -> ConfigStub {
    ConfigStub {
        section: mode.map(|mode| json!({ "mode": mode })),
    }
}

fn policy(mode: SandboxMode) -> SandboxExecutionPolicy {
    SandboxExecutionPolicy {
        mode,
        workspace_root: "/work".to_string(),
    }
}

#[test]
fn policy_resolution_defaults_to_off_when_the_section_is_absent() {
    let policy = resolve_sandbox_policy(&config_with(None), "/work");
    assert_eq!(policy.mode, SandboxMode::Off);
    assert_eq!(policy.workspace_root, "/work");
}

#[test]
fn policy_resolution_carries_the_configured_mode_and_root() {
    assert_eq!(
        resolve_sandbox_policy(&config_with(Some("read-only")), "/work").mode,
        SandboxMode::ReadOnly
    );
    assert_eq!(
        resolve_sandbox_policy(&config_with(Some("workspace-write")), "/work").mode,
        SandboxMode::WorkspaceWrite
    );
}

#[test]
fn backend_fails_closed() {
    // v2: `isSandboxBackendAvailable()` is `false` — confined modes are refused at
    // the boundary rather than silently running unsandboxed.
    assert!(!is_sandbox_backend_available());
}

#[test]
fn write_guard_allows_writes_when_sandbox_is_off() {
    assert_eq!(sandbox_write_guard(&policy(SandboxMode::Off), "/work/file.txt"), None);
}

#[test]
fn write_guard_blocks_every_write_in_read_only_mode() {
    let error = sandbox_write_guard(&policy(SandboxMode::ReadOnly), "/work/file.txt").expect("must block");
    assert!(error.contains("read-only"), "{error}");
    assert!(error.contains("[sandbox]"), "{error}");
}

#[test]
fn write_guard_allows_inside_workspace_in_workspace_write_mode() {
    let policy = policy(SandboxMode::WorkspaceWrite);
    assert_eq!(sandbox_write_guard(&policy, "/work/file.txt"), None);
    assert_eq!(sandbox_write_guard(&policy, "/work/sub/dir/file.txt"), None);
}

#[test]
fn write_guard_blocks_outside_workspace_in_workspace_write_mode() {
    let error =
        sandbox_write_guard(&policy(SandboxMode::WorkspaceWrite), "/etc/passwd").expect("must block");
    assert!(error.contains("/work"), "{error}");
    assert!(error.contains("/etc/passwd"), "{error}");
}

#[test]
fn write_guard_does_not_confuse_a_sibling_directory() {
    assert!(sandbox_write_guard(&policy(SandboxMode::WorkspaceWrite), "/work2/file.txt").is_some());
}

#[test]
fn write_guard_treats_drive_letter_case_difference_as_inside() {
    let policy = SandboxExecutionPolicy {
        mode: SandboxMode::WorkspaceWrite,
        workspace_root: "C:/work".to_string(),
    };
    assert_eq!(sandbox_write_guard(&policy, "c:/work/file.txt"), None);
}

#[test]
fn veto_allows_everything_when_sandbox_is_off() {
    let policy = policy(SandboxMode::Off);
    assert_eq!(
        sandbox_tool_veto(
            &policy,
            "Write",
            Some(ToolSideEffect::Write),
            Some(&ToolAccesses::write_file("/work/a"))
        ),
        None
    );
    assert_eq!(
        sandbox_tool_veto(&policy, "Bash", Some(ToolSideEffect::Shell), None),
        None
    );
    assert_eq!(
        sandbox_tool_veto(&policy, "run_code", Some(ToolSideEffect::Execute), None),
        None
    );
    assert_eq!(sandbox_tool_veto(&policy, "mcp__x", None, None), None);
}

#[test]
fn veto_always_allows_read_class_tools_in_confined_modes() {
    for mode in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
        assert_eq!(
            sandbox_tool_veto(
                &policy(mode),
                "Read",
                Some(ToolSideEffect::Read),
                Some(&ToolAccesses::read_file("/etc/passwd"))
            ),
            None
        );
    }
}

#[test]
fn veto_blocks_write_class_tools_with_declared_targets_in_read_only() {
    let error = sandbox_tool_veto(
        &policy(SandboxMode::ReadOnly),
        "Write",
        Some(ToolSideEffect::Write),
        Some(&ToolAccesses::write_file("/work/a")),
    )
    .expect("must block");
    assert!(error.contains("read-only"), "{error}");
    assert!(error.contains("[sandbox]"), "{error}");
}

#[test]
fn veto_blocks_write_class_tools_without_declared_targets_in_read_only() {
    let error = sandbox_tool_veto(
        &policy(SandboxMode::ReadOnly),
        "TaskStop",
        Some(ToolSideEffect::Write),
        Some(&ToolAccesses::none()),
    )
    .expect("must block");
    assert!(error.contains("TaskStop"), "{error}");
    assert!(error.contains("read-only"), "{error}");
}

#[test]
fn veto_blocks_write_class_tools_that_declare_no_accesses_in_read_only() {
    let error = sandbox_tool_veto(&policy(SandboxMode::ReadOnly), "Memory", Some(ToolSideEffect::Write), None)
        .expect("must block");
    assert!(error.contains("Memory"), "{error}");
}

#[test]
fn veto_allows_in_workspace_writes_in_workspace_write_mode() {
    let policy = policy(SandboxMode::WorkspaceWrite);
    assert_eq!(
        sandbox_tool_veto(
            &policy,
            "Write",
            Some(ToolSideEffect::Write),
            Some(&ToolAccesses::write_file("/work/sub/a.ts"))
        ),
        None
    );
    assert_eq!(
        sandbox_tool_veto(
            &policy,
            "Edit",
            Some(ToolSideEffect::Write),
            Some(&ToolAccesses::read_write_file("/work/b.ts"))
        ),
        None
    );
}

#[test]
fn veto_blocks_out_of_workspace_writes_in_workspace_write_mode() {
    let error = sandbox_tool_veto(
        &policy(SandboxMode::WorkspaceWrite),
        "Write",
        Some(ToolSideEffect::Write),
        Some(&ToolAccesses::write_file("/etc/x")),
    )
    .expect("must block");
    assert!(error.contains("/work"), "{error}");
    assert!(error.contains("/etc/x"), "{error}");
}

#[test]
fn veto_blocks_write_class_tools_without_verifiable_targets_in_workspace_write() {
    let error = sandbox_tool_veto(
        &policy(SandboxMode::WorkspaceWrite),
        "CronCreate",
        Some(ToolSideEffect::Write),
        Some(&ToolAccesses::none()),
    )
    .expect("must block");
    assert!(error.contains("CronCreate"), "{error}");
    assert!(error.contains("fail closed"), "{error}");
}

#[test]
fn veto_blocks_shell_tools_in_read_only_mode() {
    let error = sandbox_tool_veto(&policy(SandboxMode::ReadOnly), "Bash", Some(ToolSideEffect::Shell), None)
        .expect("must block");
    assert!(error.contains("Bash"), "{error}");
    assert!(error.contains("[sandbox]"), "{error}");
}

#[test]
fn veto_blocks_shell_tools_in_workspace_write_mode_when_no_backend() {
    let error = sandbox_tool_veto(
        &policy(SandboxMode::WorkspaceWrite),
        "PowerShell",
        Some(ToolSideEffect::Shell),
        None,
    )
    .expect("must block");
    assert!(error.contains("PowerShell"), "{error}");
    assert!(error.contains("fail closed"), "{error}");
}

#[test]
fn veto_blocks_code_execution_in_both_confined_modes() {
    for mode in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
        let error = sandbox_tool_veto(&policy(mode), "run_code", Some(ToolSideEffect::Execute), None)
            .expect("must block");
        assert!(error.contains("run_code"), "{error}");
    }
}

#[test]
fn veto_blocks_tools_without_a_declared_side_effect_class() {
    for mode in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
        let error = sandbox_tool_veto(&policy(mode), "mcp__server__tool", None, None)
            .expect("must block");
        assert!(error.contains("mcp__server__tool"), "{error}");
        assert!(error.contains("fail closed"), "{error}");
    }
}

#[test]
fn validate_rejects_unknown_spellings_like_the_z_enum() {
    assert_eq!(SandboxMode::validate("off").unwrap(), SandboxMode::Off);
    assert_eq!(SandboxMode::validate("read-only").unwrap(), SandboxMode::ReadOnly);
    assert_eq!(
        SandboxMode::validate("workspace-write").unwrap(),
        SandboxMode::WorkspaceWrite
    );
    assert!(SandboxMode::validate("readonlyy").is_err());
}

#[test]
fn write_paths_only_include_file_writes() {
    let accesses: ToolAccessesList = vec![
        ToolResourceAccess::File(ToolFileAccess {
            operation: FileOperation::Write,
            path: "/work/a".to_string(),
            recursive: false,
        }),
        ToolResourceAccess::File(ToolFileAccess {
            operation: FileOperation::Read,
            path: "/work/b".to_string(),
            recursive: false,
        }),
        ToolResourceAccess::All,
    ];
    // The read entry and `All` must not be treated as write targets: only
    // `/work/a` is checked, and it is inside the workspace.
    assert_eq!(
        sandbox_tool_veto(
            &policy(SandboxMode::WorkspaceWrite),
            "Edit",
            Some(ToolSideEffect::Write),
            Some(&accesses)
        ),
        None
    );
}
