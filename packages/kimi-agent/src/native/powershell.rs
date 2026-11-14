use std::path::{Path, PathBuf};

use super::environment_probe::ProbeShellNotFound;
use super::shell_payload::{ShellPayloadKind, find_shell_payload_pin, shell_payload_exe_path};

pub const POWER_SHELL_UTF8_PREAMBLE: &str = "[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new()\n\
$OutputEncoding=[System.Text.UTF8Encoding]::new()\n\
$ProgressPreference='SilentlyContinue'";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerShellKind {
    Pwsh,
    WindowsPowerShell,
}

impl PowerShellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pwsh => "pwsh",
            Self::WindowsPowerShell => "windows-powershell",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerShellExecutable {
    pub path: PathBuf,
    pub kind: PowerShellKind,
}

pub fn encode_script(script: &str) -> String {
    use base64::Engine as _;
    let full = format!("{POWER_SHELL_UTF8_PREAMBLE}\n{script}");
    let bytes: Vec<u8> = full.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub const POWER_SHELL_ENV: &[(&str, &str)] = &[
    ("NO_COLOR", "1"),
    ("POWERSHELL_TELEMETRY_OPTOUT", "1"),
    ("POWERSHELL_UPDATECHECK_OPTOUT", "1"),
];

pub struct DescriptionVariants;

impl DescriptionVariants {
    pub fn without_background(description: &str) -> String {
        replace_paragraph(
            description,
            "If `run_in_background=true`,",
            "Background execution is disabled for this agent. Do not set `run_in_background=true`.",
        )
    }

    pub fn without_auto_background(description: &str) -> String {
        description.replace(
            "a foreground call that hits its timeout is moved to the background instead of being killed.",
            "a foreground call that hits its timeout is killed.",
        )
    }
}

fn replace_paragraph(description: &str, prefix: &str, replacement: &str) -> String {
    let Some(start) = description.find(prefix) else {
        return description.to_string();
    };
    let end = description[start..]
        .find(
            "

",
        )
        .map(|offset| start + offset)
        .unwrap_or(description.len());
    let mut out = String::with_capacity(description.len());
    out.push_str(&description[..start]);
    out.push_str(replacement);
    out.push_str(&description[end..]);
    out
}

pub fn build_args(script: &str) -> Vec<String> {
    vec![
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-EncodedCommand".to_string(),
        encode_script(script),
    ]
}

pub fn normalize_output(text: &str) -> String {
    text.replace("\r\n", "\n")
}

pub struct ResolveDeps<'a> {
    pub pwsh_override: Option<&'a str>,
    pub path_env: Option<&'a str>,
    pub system_root: Option<&'a str>,
    pub kimi_home: &'a Path,
    pub is_file: &'a dyn Fn(&Path) -> bool,
}

pub fn resolve(deps: ResolveDeps<'_>) -> Result<PowerShellExecutable, ProbeShellNotFound> {
    let mut checked: Vec<String> = Vec::new();

    let accept = |path: PathBuf, kind: PowerShellKind, checked: &mut Vec<String>| {
        checked.push(path.display().to_string());
        (deps.is_file)(&path).then_some(PowerShellExecutable { path, kind })
    };

    if let Some(override_path) = deps.pwsh_override.map(str::trim).filter(|s| !s.is_empty())
        && let Some(found) = accept(
            PathBuf::from(override_path),
            PowerShellKind::Pwsh,
            &mut checked,
        )
    {
        return Ok(found);
    }

    if let Some(pin) = find_shell_payload_pin(ShellPayloadKind::Pwsh) {
        let payload_exe = shell_payload_exe_path(deps.kimi_home, pin);
        if let Some(found) = accept(payload_exe, PowerShellKind::Pwsh, &mut checked) {
            return Ok(found);
        }
    }

    for candidate in pwsh_candidates(deps.path_env) {
        if let Some(found) = accept(candidate, PowerShellKind::Pwsh, &mut checked) {
            return Ok(found);
        }
    }

    for candidate in windows_power_shell_candidates(deps.system_root) {
        if let Some(found) = accept(candidate, PowerShellKind::WindowsPowerShell, &mut checked) {
            return Ok(found);
        }
    }

    Err(ProbeShellNotFound {
        message: "PowerShell was not found on this Windows host. Run `kimi shells provision --pwsh` to install the bundled pwsh payload, or install PowerShell 7 from https://github.com/PowerShell/PowerShell, or set KIMI_PWSH_PATH to a pwsh.exe.".to_string(),
        checked,
    })
}

fn pwsh_candidates(path_env: Option<&str>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path_value) = path_env {
        for dir in path_value.split(';') {
            let trimmed = dir.trim();
            if !trimmed.is_empty() {
                candidates.push(Path::new(trimmed).join("pwsh.exe"));
            }
        }
    }
    candidates.push(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"));
    candidates.push(PathBuf::from(
        r"C:\Program Files\PowerShell\7-preview\pwsh.exe",
    ));
    dedupe_case_insensitive(candidates)
}

fn windows_power_shell_candidates(system_root: Option<&str>) -> Vec<PathBuf> {
    let root = system_root
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(r"C:\Windows");
    vec![
        Path::new(root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"),
    ]
}

fn dedupe_case_insensitive(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        let key = path.display().to_string().to_lowercase();
        if seen.insert(key) {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn decode(script: &str) -> String {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encode_script(script))
            .expect("valid base64");
        let (pairs, _) = raw.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        String::from_utf16(&units).expect("valid utf-16")
    }

    #[test]
    fn the_encoded_script_is_the_preamble_then_the_callers_script() {
        let decoded = decode("Get-ChildItem");
        assert_eq!(
            decoded,
            format!("{POWER_SHELL_UTF8_PREAMBLE}\nGet-ChildItem")
        );
        assert!(decoded.ends_with("Get-ChildItem"));
        assert!(decoded.contains("UTF8Encoding"));
        assert!(decoded.contains("ProgressPreference='SilentlyContinue'"));
    }

    #[test]
    fn non_ascii_scripts_round_trip() {
        assert!(decode("Write-Output '你好'").ends_with("Write-Output '你好'"));
    }

    #[test]
    fn the_argv_disables_profile_and_interactive_prompts() {
        let args = build_args("Get-Date");
        assert_eq!(
            args[..3],
            [
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-EncodedCommand".to_string()
            ]
        );
        assert_eq!(args[3], encode_script("Get-Date"));
    }

    #[test]
    fn crlf_is_normalized_to_lf() {
        assert_eq!(normalize_output("a\r\nb\r\n"), "a\nb\n");
        assert_eq!(normalize_output("a\nb"), "a\nb");
    }

    #[test]
    fn the_explicit_override_wins() {
        let found = resolve(ResolveDeps {
            pwsh_override: Some(r"C:\custom\pwsh.exe"),
            path_env: Some(r"C:\tools"),
            system_root: None,
            kimi_home: Path::new(r"C:\Users\tester\.kimi-code"),
            is_file: &|path| path == Path::new(r"C:\custom\pwsh.exe"),
        })
        .expect("the override resolves");
        assert_eq!(found.kind, PowerShellKind::Pwsh);
        assert_eq!(found.path, PathBuf::from(r"C:\custom\pwsh.exe"));
    }

    #[test]
    fn the_provisioned_payload_is_probed_before_path() {
        let kimi_home = Path::new(r"D:\kimi-home");
        let provisioned = shell_payload_exe_path(
            kimi_home,
            find_shell_payload_pin(ShellPayloadKind::Pwsh).unwrap(),
        );
        let found = resolve(ResolveDeps {
            pwsh_override: None,
            path_env: Some(r"C:\tools"),
            system_root: None,
            kimi_home,
            is_file: &|path| path == provisioned,
        })
        .expect("the provisioned pwsh resolves");
        assert_eq!(found.path, provisioned);
        assert_eq!(found.kind, PowerShellKind::Pwsh);
    }

    #[test]
    fn a_pwsh_on_path_is_next_and_the_install_root_is_the_last_resort() {
        let found = resolve(ResolveDeps {
            pwsh_override: None,
            path_env: Some(r"C:\tools;C:\other"),
            system_root: None,
            kimi_home: Path::new(r"C:\Users\tester\.kimi-code"),
            is_file: &|path| path == Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe"),
        })
        .expect("the install root resolves");
        assert_eq!(
            found.path,
            PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe")
        );
    }

    #[test]
    fn windows_powershell_is_the_final_fallback_and_reports_its_kind() {
        let found = resolve(ResolveDeps {
            pwsh_override: None,
            path_env: Some(""),
            system_root: Some(r"D:\Windows"),
            kimi_home: Path::new(r"C:\Users\tester\.kimi-code"),
            is_file: &|path| {
                path == Path::new(r"D:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
            },
        })
        .expect("the OS interpreter resolves");
        assert_eq!(found.kind, PowerShellKind::WindowsPowerShell);
    }

    #[test]
    fn a_missing_interpreter_lists_every_candidate_it_probed() {
        let error = resolve(ResolveDeps {
            pwsh_override: None,
            path_env: Some(r"C:\tools"),
            system_root: Some(r"C:\Windows"),
            kimi_home: Path::new(r"C:\Users\tester\.kimi-code"),
            is_file: &|_| false,
        })
        .expect_err("nothing resolves");
        assert!(error.checked.iter().any(|path| path.ends_with("pwsh.exe")));
        assert!(
            error
                .checked
                .iter()
                .any(|path| path.ends_with("powershell.exe"))
        );
        let message = &error.message;
        assert!(message.contains("KIMI_PWSH_PATH"));
        assert!(message.contains("PowerShell 7"));
    }

    #[test]
    fn duplicate_case_variants_are_probed_once() {
        let candidates = pwsh_candidates(Some(r"C:\Tools;C:\TOOLS"));
        let pwsh_count = candidates
            .iter()
            .filter(|path| path.ends_with("pwsh.exe"))
            .count();
        assert_eq!(pwsh_count, 3, "PATH hit plus two install roots");
    }
}

#[cfg(test)]
mod description_variants {
    use super::*;

    const TEMPLATE: &str = "Execute a Windows PowerShell script natively.

Foreground calls default to 60s and allow up to 300s; a foreground call that hits its timeout is moved to the background instead of being killed.

If `run_in_background=true`, the script will be started as a background task and this tool will return a task ID. If a human user wants to inspect background tasks themselves, point them to the background-task panel.

**Safety:** do not use this tool to escape the working directory.";

    #[test]
    fn the_default_variant_keeps_the_template() {
        assert!(TEMPLATE.contains("moved to the background"));
        assert!(TEMPLATE.contains("If `run_in_background=true`"));
    }

    #[test]
    fn without_background_replaces_the_whole_paragraph() {
        let rendered = DescriptionVariants::without_background(TEMPLATE);
        assert!(rendered.contains(
            "Background execution is disabled for this agent. Do not set `run_in_background=true`."
        ));
        assert!(!rendered.contains("If `run_in_background=true`"));
        assert!(!rendered.contains("background-task panel"));

        assert!(rendered.starts_with("Execute a Windows PowerShell script natively."));
        assert!(
            rendered.contains("**Safety:** do not use this tool to escape the working directory.")
        );
    }

    #[test]
    fn without_auto_background_swaps_the_sentence_and_nothing_else() {
        let rendered = DescriptionVariants::without_auto_background(TEMPLATE);
        assert!(rendered.contains("a foreground call that hits its timeout is killed."));
        assert!(!rendered.contains("moved to the background instead of being killed"));
        assert!(rendered.contains("If `run_in_background=true`"));
    }

    #[test]
    fn a_template_without_the_paragraph_is_returned_unchanged() {
        let plain = "A description with no background paragraph.";
        assert_eq!(DescriptionVariants::without_background(plain), plain);
    }

    #[test]
    fn every_spawn_carries_the_telemetry_and_colour_opts_outs() {
        let names: Vec<&str> = POWER_SHELL_ENV.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            [
                "NO_COLOR",
                "POWERSHELL_TELEMETRY_OPTOUT",
                "POWERSHELL_UPDATECHECK_OPTOUT"
            ]
        );
        assert!(POWER_SHELL_ENV.iter().all(|(_, value)| *value == "1"));
    }
}
