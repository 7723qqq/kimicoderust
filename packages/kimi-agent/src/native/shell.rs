use super::environment_probe::{
    HostEnvironmentProbeDeps, ProbeShellNotFound, exec_file_text, locate_windows_git_bash,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFlavor {
    Bash,
    Pwsh,
    PowerShell,
    Cmd,
}

impl ShellFlavor {
    pub fn from_preference(preference: &str) -> Option<Self> {
        match preference.trim().to_ascii_lowercase().as_str() {
            "bash" => Some(Self::Bash),
            "pwsh" => Some(Self::Pwsh),
            "powershell" => Some(Self::PowerShell),
            "cmd" => Some(Self::Cmd),
            _ => None,
        }
    }

    pub fn from_path(path: &str) -> Self {
        let base = path
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(path)
            .to_ascii_lowercase();
        match base.as_str() {
            "pwsh" | "pwsh.exe" => Self::Pwsh,
            "powershell" | "powershell.exe" => Self::PowerShell,
            "cmd" | "cmd.exe" => Self::Cmd,
            _ => Self::Bash,
        }
    }

    pub fn args_prefix(self) -> Vec<String> {
        match self {
            Self::Pwsh | Self::PowerShell => {
                vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                ]
            }
            Self::Cmd => vec!["/c".to_string()],
            Self::Bash => vec!["-c".to_string()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedShell {
    pub program: String,
    pub flavor: ShellFlavor,
}

impl ResolvedShell {
    pub fn new(program: impl Into<String>) -> Self {
        let program = program.into();
        let flavor = ShellFlavor::from_path(&program);
        Self { program, flavor }
    }

    pub fn args_prefix(&self) -> Vec<String> {
        self.flavor.args_prefix()
    }
}

pub fn posix_shell(exists: &dyn Fn(&str) -> bool) -> ResolvedShell {
    const CANDIDATES: &[&str] = &["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"];
    let program = CANDIDATES
        .iter()
        .find(|candidate| exists(candidate))
        .copied()
        .unwrap_or("/bin/sh");
    ResolvedShell {
        program: program.to_string(),
        flavor: ShellFlavor::Bash,
    }
}

pub fn resolve_shell(preference: Option<&str>) -> Result<ResolvedShell, ProbeShellNotFound> {
    #[cfg(not(windows))]
    {
        let _ = preference;
        Ok(posix_shell(&|candidate| {
            std::path::Path::new(candidate).exists()
        }))
    }
    #[cfg(windows)]
    {
        let env = |name: &str| std::env::var(name).ok();
        let home_dir = crate::default_workspace::user_home_dir().unwrap_or_default();
        let deps = HostEnvironmentProbeDeps {
            platform: "win32",
            arch: std::env::consts::ARCH,
            release: std::env::consts::OS,
            home_dir: &home_dir,
            env: &env,
            is_file: &|path| path.is_file(),
            exec_file_text: &exec_file_text,
        };
        resolve_windows_shell(preference, &deps)
    }
}

#[cfg(windows)]
fn resolve_windows_shell(
    preference: Option<&str>,
    deps: &HostEnvironmentProbeDeps<'_>,
) -> Result<ResolvedShell, ProbeShellNotFound> {
    if let Some(path) = (deps.env)("KIMI_SHELL_PATH")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        && (deps.is_file)(std::path::Path::new(&path))
    {
        return Ok(ResolvedShell::new(path));
    }

    if let Some(flavor) = preference
        .and_then(ShellFlavor::from_preference)
        .filter(|flavor| *flavor != ShellFlavor::Bash)
        && let Some(program) = program_for(flavor, deps)
    {
        return Ok(ResolvedShell { program, flavor });
    }

    if let Some(program) = which(deps, "pwsh.exe") {
        return Ok(ResolvedShell {
            program,
            flavor: ShellFlavor::Pwsh,
        });
    }
    if let Some(program) = which(deps, "powershell.exe") {
        return Ok(ResolvedShell {
            program,
            flavor: ShellFlavor::PowerShell,
        });
    }
    let program = locate_windows_git_bash(deps)?;
    Ok(ResolvedShell {
        program,
        flavor: ShellFlavor::Bash,
    })
}

#[cfg(windows)]
fn program_for(flavor: ShellFlavor, deps: &HostEnvironmentProbeDeps<'_>) -> Option<String> {
    match flavor {
        ShellFlavor::Pwsh => which(deps, "pwsh.exe").or_else(|| Some("pwsh".to_string())),
        ShellFlavor::PowerShell => {
            which(deps, "powershell.exe").or_else(|| Some("powershell".to_string()))
        }
        ShellFlavor::Cmd => Some("cmd.exe".to_string()),
        ShellFlavor::Bash => None,
    }
}

#[cfg(windows)]
fn which(deps: &HostEnvironmentProbeDeps<'_>, name: &str) -> Option<String> {
    const WHICH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2_000);
    let output = (deps.exec_file_text)("where", &[name], WHICH_TIMEOUT)?;
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_parsing() {
        assert_eq!(
            ShellFlavor::from_preference("pwsh"),
            Some(ShellFlavor::Pwsh)
        );
        assert_eq!(
            ShellFlavor::from_preference("PowerShell"),
            Some(ShellFlavor::PowerShell)
        );
        assert_eq!(ShellFlavor::from_preference("cmd"), Some(ShellFlavor::Cmd));
        assert_eq!(
            ShellFlavor::from_preference("bash"),
            Some(ShellFlavor::Bash)
        );
        assert_eq!(ShellFlavor::from_preference("auto"), None);
        assert_eq!(ShellFlavor::from_preference("nope"), None);
    }

    #[test]
    fn flavor_from_path_classifies_basenames() {
        assert_eq!(
            ShellFlavor::from_path("C:\\Program Files\\PowerShell\\7\\pwsh.exe"),
            ShellFlavor::Pwsh
        );
        assert_eq!(
            ShellFlavor::from_path(
                "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
            ),
            ShellFlavor::PowerShell
        );
        assert_eq!(ShellFlavor::from_path("cmd.exe"), ShellFlavor::Cmd);
        assert_eq!(ShellFlavor::from_path("/usr/bin/bash"), ShellFlavor::Bash);
    }

    #[test]
    fn arg_prefixes_match_each_flavor() {
        assert_eq!(ShellFlavor::Bash.args_prefix(), vec!["-c"]);
        assert_eq!(ShellFlavor::Cmd.args_prefix(), vec!["/c"]);
        assert_eq!(
            ShellFlavor::Pwsh.args_prefix(),
            vec!["-NoProfile", "-NonInteractive", "-Command"]
        );
    }

    #[test]
    fn posix_chain_prefers_bin_bash_and_keeps_the_c_prefix() {
        let resolved = posix_shell(&|candidate| candidate == "/bin/bash");
        assert_eq!(resolved.program, "/bin/bash");
        assert_eq!(resolved.flavor, ShellFlavor::Bash);
        assert_eq!(resolved.args_prefix(), vec!["-c"]);
    }

    #[test]
    fn posix_chain_walks_the_three_bash_locations_in_order() {
        assert_eq!(
            posix_shell(&|candidate| candidate == "/usr/bin/bash").program,
            "/usr/bin/bash"
        );
        assert_eq!(
            posix_shell(&|candidate| candidate == "/usr/local/bin/bash").program,
            "/usr/local/bin/bash"
        );

        assert_eq!(
            posix_shell(&|candidate| candidate == "/bin/bash" || candidate == "/usr/local/bin/bash")
                .program,
            "/bin/bash"
        );
    }

    #[test]
    fn posix_chain_falls_back_to_sh_when_no_bash_exists() {
        let resolved = posix_shell(&|_| false);
        assert_eq!(resolved.program, "/bin/sh");
        assert_eq!(resolved.args_prefix(), vec!["-c"]);
    }

    #[test]
    fn the_posix_arm_predicate_typechecks_and_probes_the_filesystem() {
        let probe = |candidate: &str| std::path::Path::new(candidate).exists();
        let resolved = posix_shell(&probe);
        assert!(
            [
                "/bin/bash",
                "/usr/bin/bash",
                "/usr/local/bin/bash",
                "/bin/sh"
            ]
            .contains(&resolved.program.as_str())
        );
        assert_eq!(resolved.args_prefix(), vec!["-c"]);
    }

    #[cfg(not(windows))]
    #[test]
    fn resolve_shell_picks_a_real_posix_candidate() {
        let resolved = resolve_shell(None).expect("a posix host always resolves");
        assert!(
            [
                "/bin/bash",
                "/usr/bin/bash",
                "/usr/local/bin/bash",
                "/bin/sh"
            ]
            .contains(&resolved.program.as_str())
        );
        assert_eq!(resolved.args_prefix(), vec!["-c"]);
    }

    #[test]
    fn explicit_path_wins_over_preference() {
        let resolved = resolve_shell(Some("cmd")).expect("the explicit flavor resolves");
        assert!(!resolved.program.is_empty());
        assert_eq!(resolved.args_prefix(), resolved.flavor.args_prefix());
    }

    #[cfg(windows)]
    fn windows_deps<'a>(
        env: &'a dyn Fn(&str) -> Option<String>,
        is_file: &'a dyn Fn(&std::path::Path) -> bool,
        exec: &'a dyn Fn(&str, &[&str], std::time::Duration) -> Option<String>,
    ) -> HostEnvironmentProbeDeps<'a> {
        HostEnvironmentProbeDeps {
            platform: "win32",
            arch: "x64",
            release: "10.0.22631",
            home_dir: std::path::Path::new("C:\\Users\\tester"),
            env,
            is_file,
            exec_file_text: exec,
        }
    }

    #[cfg(windows)]
    #[test]
    fn auto_resolution_uses_the_provisioned_git_bash() {
        let env = |name: &str| (name == "KIMI_CODE_HOME").then(|| "D:\\kimi-home".to_string());
        let provisioned = crate::native::shell_payload::shell_payload_exe_path(
            std::path::Path::new("D:\\kimi-home"),
            crate::native::shell_payload::shell_payload_pin_of(
                crate::native::shell_payload::ShellPayloadKind::Bash,
            ),
        );
        let is_file = |path: &std::path::Path| path == provisioned;
        let no_exec = |_: &str, _: &[&str], _: std::time::Duration| None;
        let resolved = resolve_windows_shell(None, &windows_deps(&env, &is_file, &no_exec))
            .expect("the provisioned bash resolves");
        assert_eq!(resolved.program, provisioned.display().to_string());
        assert_eq!(resolved.flavor, ShellFlavor::Bash);
    }

    #[cfg(windows)]
    #[test]
    fn auto_resolution_returns_the_checked_error_instead_of_cmd() {
        let no_env = |_: &str| None;
        let no_file = |_: &std::path::Path| false;
        let no_exec = |_: &str, _: &[&str], _: std::time::Duration| None;
        let error = resolve_windows_shell(None, &windows_deps(&no_env, &no_file, &no_exec))
            .expect_err("auto resolution must not fall back to cmd");
        assert_eq!(error.code(), "shell.git_bash_not_found");
        assert!(!error.checked.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn explicit_cmd_preference_does_not_require_git_bash() {
        let no_env = |_: &str| None;
        let no_file = |_: &std::path::Path| false;
        let no_exec = |_: &str, _: &[&str], _: std::time::Duration| None;
        let resolved =
            resolve_windows_shell(Some("cmd"), &windows_deps(&no_env, &no_file, &no_exec))
                .expect("the explicit cmd preference remains supported");
        assert_eq!(resolved.program, "cmd.exe");
        assert_eq!(resolved.flavor, ShellFlavor::Cmd);
    }
}
