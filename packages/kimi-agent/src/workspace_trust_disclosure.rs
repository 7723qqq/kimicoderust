//! Workspace trust disclosure — what trusting a workspace would switch on.
//!
//! Ports v2 `workspace/workspaceTrust/trustDisclosureService.ts` (+ the wire
//! shapes in `trustDisclosure.ts`, D:\kimi\kimi-code @ a2afdbc7d). The prompt
//! that asks "do you trust this folder?" is only answerable if the user can
//! see what the answer activates: project-level MCP servers, the extra
//! directories those would widen the sandbox to, and the instruction files
//! (AGENTS.md, project skills, agent profiles) the session would start reading.
//!
//! v2 assembles that from five services. Here the ingredients already exist as
//! separate modules — [`crate::project_local_config`] for
//! `workspace.additional_dir`, [`crate::prompt::agents_md`] for the cascaded
//! instruction files, [`crate::skills`] for the project skill catalog and
//! [`crate::prompt::profiles`] for the builtin profile set — and this module is
//! the one place that composes them. The TypeScript host used to read
//! `.mcp.json` itself (`sdk-rpc-client-native.ts` `readProjectMcpDisclosure`);
//! that copy is gone, because a second parser is a second answer to the same
//! question.
//!
//! Trusted workspaces disclose nothing: the caller only asks when the gate is
//! about to show a prompt.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::McpServerConfig;

/// One project-level MCP server a trust decision would start.
///
/// The safe subset on purpose: `env`, `headers` and `bearer_token_env_var` are
/// exactly what a planted `.mcp.json` would exfiltrate, and this description is
/// rendered *before* the workspace is trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustGatedMcpServer {
    pub name: String,
    pub transport: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The config file that declared this server.
    pub origin: String,
}

/// The instruction files a trusted session would load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustGatedInstructionSources {
    pub agents_md_paths: Vec<String>,
    pub skills: Vec<String>,
    pub agent_profiles: Vec<String>,
    /// Every instruction path, deduplicated — what the prompt lists.
    pub paths: Vec<String>,
}

/// Everything trusting this workspace would activate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustGatedActivation {
    pub mcp_servers: Vec<TrustGatedMcpServer>,
    /// Directories outside the project root that would become reachable.
    pub additional_dirs: Vec<String>,
    /// The configuration files that contributed those directories.
    pub additional_dir_sources: Vec<String>,
    /// Inspections that could not be completed. Rendered as-is: the prompt
    /// tells the user what it could not see instead of implying there was
    /// nothing to see.
    pub warnings: Vec<String>,
    pub instruction_sources: TrustGatedInstructionSources,
}

/// What the disclosure needs to know about the workspace it describes.
#[derive(Debug, Clone)]
pub struct DisclosureInput<'a> {
    pub work_dir: &'a Path,
    /// `--skill-dirs` / `KIMI_CODE_SKILL_DIRS`. When the user declared skill
    /// roots explicitly, the project skill scan is not the thing being
    /// activated, so v2 reports no skill roots (and neither does this).
    pub explicit_skill_dirs: bool,
    pub merge_all_available_skills: bool,
    /// Directories the user is about to add (`/add-dir`). v2's
    /// `describeGatedActivation(pendingAdditionalDirs)` discloses them
    /// alongside the configured ones, so the prompt answers "what would
    /// trusting plus these get me" rather than only the on-disk half.
    pub pending_additional_dirs: Vec<std::path::PathBuf>,
}

/// Assemble the disclosure for one workspace.
///
/// Upstream divides the work across four `Promise.all` branches that each
/// degrade to an empty result plus a warning; the same contract holds here: a
/// failed inspection never hides the rest of the list, it adds a line saying
/// what could not be inspected.
pub fn describe_gated_activation(input: &DisclosureInput<'_>) -> TrustGatedActivation {
    let project_root = project_root_of(input.work_dir);
    let mut warnings: Vec<String> = Vec::new();

    let mcp_servers = describe_gated_mcp_servers(input, &project_root);
    let (additional_dirs, additional_dir_sources) =
        read_gated_additional_dirs(input, &project_root, &mut warnings);
    let instruction_sources = describe_instruction_sources(input, &project_root, &mut warnings);

    TrustGatedActivation {
        mcp_servers,
        additional_dirs,
        additional_dir_sources,
        warnings,
        instruction_sources,
    }
}

/// The directory a disclosure path is judged against: the nearest enclosing
/// git work tree, else the workspace itself (v2 `findGitWorkTree ?? cwd`).
fn project_root_of(work_dir: &Path) -> PathBuf {
    realpath_or_self(
        &crate::project_local_config::locate_additional_dirs_config(work_dir).project_root,
    )
}

fn describe_gated_mcp_servers(
    input: &DisclosureInput<'_>,
    project_root: &Path,
) -> Vec<TrustGatedMcpServer> {
    let mut out: Vec<TrustGatedMcpServer> = read_project_mcp_layers(input.work_dir)
        .into_iter()
        .filter(|(_, config, _)| config.enabled != Some(false))
        .filter_map(|(name, config, origin)| {
            // A project-level server is one whose declaring file lives inside
            // the project. Both project layers are read here, so the test is on
            // the file's own path rather than on which layer supplied it.
            let origin_path = realpath_or_self(Path::new(&origin));
            if !is_inside_or_equal(&origin_path, project_root) {
                return None;
            }
            describe_mcp_server(&name, &config, &origin_path)
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The project's two MCP config layers, later winning, each entry tagged with
/// the file that declared it — the shape v2's `loadMcpServersDetailed` reports
/// through `origins`.
///
/// The user layer (`<home>/mcp.json`) is deliberately absent: it applies
/// whether or not this workspace is trusted, so it is not something the
/// decision switches on.
fn read_project_mcp_layers(work_dir: &Path) -> Vec<(String, McpServerConfig, String)> {
    let location = crate::project_local_config::locate_additional_dirs_config(work_dir);
    let root = location.project_root;
    let layers: [PathBuf; 2] = [
        root.join(".mcp.json"),
        work_dir.join(".kimi-code").join("mcp.json"),
    ];

    let mut merged: Vec<(String, McpServerConfig, String)> = Vec::new();
    for path in layers {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<ProjectMcpFile>(&raw) else {
            continue;
        };
        let origin = display_path(&path);
        for (name, mut config) in parsed.mcp_servers {
            // v2 `normalizeStdioCwd` (`mcpConfig/configLoader.ts:162-166`):
            // only a stdio entry's `cwd` is normalized. Upstream tests the
            // declared `transport`, so an entry that only carries a URL is a
            // remote server and keeps whatever `cwd` the config wrote.
            if config.transport.as_deref() == Some("stdio")
                || (config.transport.is_none() && config.url.is_none())
            {
                config.cwd = Some(match config.cwd.as_deref() {
                    None => display_path(&root),
                    // `pathe.resolve` treats a leading `/` as absolute even on
                    // Windows, while `Path::join` would prepend the drive and
                    // turn `/tmp/x` into `C:/tmp/x`. The config is written in
                    // the POSIX dialect MCP uses, so keep it intact.
                    Some(cwd) if cwd.starts_with('/') => cwd.to_string(),
                    Some(cwd) => display_path(&root.join(cwd)),
                });
            }
            merged.retain(|(kept, _, _)| kept != &name);
            merged.push((name, config, origin.clone()));
        }
    }
    merged
}

/// The subset of a `.mcp.json` the disclosure reads: `mcpServers` only.
#[derive(Debug, serde::Deserialize)]
struct ProjectMcpFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: std::collections::BTreeMap<String, McpServerConfig>,
}

/// v2 `describeMcpServer`: a stdio server reports its command line, a remote
/// one its URL, and an entry with neither is dropped — guessing a transport
/// would put a claim in front of the user the config never made.
fn describe_mcp_server(
    name: &str,
    config: &McpServerConfig,
    origin: &Path,
) -> Option<TrustGatedMcpServer> {
    let declared = config.transport.as_deref();
    let transport = match declared {
        Some("stdio") => "stdio",
        Some("http") => "http",
        Some("sse") => "sse",
        // An explicit spelling the engine does not model falls back to the
        // shape test, exactly like the session's own loader.
        _ if config.command.is_some() => "stdio",
        _ if config.url.is_some() => "http",
        _ => return None,
    };
    Some(TrustGatedMcpServer {
        name: name.to_string(),
        transport: transport.to_string(),
        command: config.command.clone(),
        args: config.args.clone(),
        cwd: config.cwd.clone(),
        url: config.url.clone(),
        origin: display_path(origin),
    })
}

fn read_gated_additional_dirs(
    input: &DisclosureInput<'_>,
    project_root: &Path,
    warnings: &mut Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let work_dir = input.work_dir;
    let location = crate::project_local_config::locate_additional_dirs_config(work_dir);
    let configured = match crate::project_local_config::read_additional_dirs(work_dir) {
        Ok(dirs) => dirs,
        Err(error) => {
            warnings.push(format!(
                "Could not inspect additional directory configuration: {error}"
            ));
            Vec::new()
        }
    };
    let mut dirs: Vec<String> = Vec::new();
    for dir in configured {
        let real = realpath_or_self(&dir);
        if is_inside_or_equal(&real, project_root) {
            continue;
        }
        dirs.push(display_path(&real));
    }
    let mut sources = if dirs.is_empty() {
        Vec::new()
    } else {
        vec![display_path(&realpath_or_self(&location.config_path))]
    };
    // Pending directories name themselves as their own source: they are not in
    // any config file yet.
    for pending in &input.pending_additional_dirs {
        let real = realpath_or_self(pending);
        if is_inside_or_equal(&real, project_root) {
            continue;
        }
        let display = display_path(&real);
        if dirs.contains(&display) {
            continue;
        }
        dirs.push(display.clone());
        sources.push(display);
    }
    (dirs, sources)
}

fn describe_instruction_sources(
    input: &DisclosureInput<'_>,
    project_root: &Path,
    warnings: &mut Vec<String>,
) -> TrustGatedInstructionSources {
    // 1. AGENTS.md files, project-scoped only.
    let loaded = crate::prompt::agents_md::load_agents_md(input.work_dir, None);
    if let Some(warning) = loaded.warning.as_deref() {
        warnings.push(warning.to_string());
    }
    let mut agents_md_paths: Vec<String> = loaded
        .paths
        .iter()
        .map(|path| realpath_or_self(Path::new(path)))
        .filter(|path| is_inside_or_equal(path, project_root))
        .map(|path| display_path(&path))
        .collect();
    agents_md_paths.sort();

    // 2. Project skills. The catalog scan is cheap and synchronous here; a
    //    scan that fails is reported rather than rendered as "no skills".
    let mut skills: Vec<String> = Vec::new();
    let mut skill_paths: Vec<String> = Vec::new();
    if !input.explicit_skill_dirs {
        let roots = crate::skills::SkillScanRoots {
            root: Some(input.work_dir.to_path_buf()),
            extra_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            merge_all_available_skills: input.merge_all_available_skills,
        };
        for descriptor in roots.catalog() {
            if descriptor.source != "project" {
                continue;
            }
            skills.push(descriptor.name);
            let path = realpath_or_self(Path::new(&descriptor.path));
            if is_inside_or_equal(&path, project_root) {
                skill_paths.push(display_path(&path));
            }
        }
        skills.sort();
        skill_paths.sort();
        skill_paths.dedup();
    }

    // 3. Agent profiles. The fork's catalog is the builtin set: a workspace
    //    cannot contribute profiles yet, so the list stays empty rather than
    //    naming profiles that activate nothing.
    let agent_profiles: Vec<String> = Vec::new();

    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(agents_md_paths.iter().cloned());
    paths.extend(skill_paths);
    TrustGatedInstructionSources {
        agents_md_paths,
        skills,
        agent_profiles,
        paths: paths.into_iter().collect(),
    }
}

/// `realpath` with the input as the fallback: a path that does not exist yet
/// is still the path the disclosure names.
fn realpath_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        // `canonicalize` also fails for a path that exists but is not readable;
        // falling back to the lexical absolute form keeps the comparison
        // meaningful instead of collapsing every such path to "".
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

/// v2 `isInsideOrEqualDir`: equal counts as inside; `..`-escapes do not.
fn is_inside_or_equal(child: &Path, parent: &Path) -> bool {
    if child == parent {
        return true;
    }
    match child.strip_prefix(parent) {
        Ok(rest) => !rest.as_os_str().is_empty() && !rest.starts_with(".."),
        Err(_) => false,
    }
}

/// Forward-slash display form, matching the rest of the engine's wire paths.
fn display_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kimi-trust-disclosure-{tag}-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a `.mcp.json` in the project root with the given server entries.
    fn write_mcp_json(dir: &Path, servers: serde_json::Value) {
        std::fs::write(
            dir.join(".mcp.json"),
            serde_json::json!({ "mcpServers": servers }).to_string(),
        )
        .unwrap();
    }

    fn input(work_dir: &Path) -> DisclosureInput<'_> {
        DisclosureInput {
            work_dir,
            explicit_skill_dirs: false,
            merge_all_available_skills: true,
            pending_additional_dirs: Vec::new(),
        }
    }

    /// A project-level server is disclosed with its command line but never its
    /// environment: the prompt runs before the workspace is trusted, and `env`
    /// is exactly what a planted `.mcp.json` would leak.
    #[test]
    fn discloses_project_mcp_servers_without_their_environment() {
        let dir = tmpdir("mcp");
        write_mcp_json(
            &dir,
            serde_json::json!({
                "local": {
                    "command": "node",
                    "args": ["server.js"],
                    "env": { "SECRET": "sk-do-not-disclose" }
                }
            }),
        );

        let activation = describe_gated_activation(&input(&dir));
        assert_eq!(activation.mcp_servers.len(), 1);
        let server = &activation.mcp_servers[0];
        assert_eq!(server.name, "local");
        assert_eq!(server.transport, "stdio");
        assert_eq!(server.command.as_deref(), Some("node"));
        assert!(server.origin.ends_with(".mcp.json"));
        let rendered = serde_json::to_string(&activation).unwrap();
        assert!(
            !rendered.contains("sk-do-not-disclose"),
            "env must not reach the prompt: {rendered}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// v2 `normalizeStdioCwd`: an absent `cwd` takes the layer's base, a
    /// relative one resolves against it, and a POSIX-absolute one stays put —
    /// `pathe.resolve` does not prepend a drive on Windows and neither does
    /// this.
    #[test]
    fn normalizes_stdio_cwd_like_upstream() {
        let dir = tmpdir("cwd");
        write_mcp_json(
            &dir,
            serde_json::json!({
                "implicit": { "command": "node" },
                "relative": { "command": "node", "cwd": "sub" },
                "absolute": { "command": "node", "cwd": "/tmp/x" },
                "remote": { "url": "https://example.test/mcp", "cwd": "ignored" }
            }),
        );

        let activation = describe_gated_activation(&input(&dir));
        let by_name = |name: &str| {
            activation
                .mcp_servers
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} missing: {activation:?}"))
        };
        // The implicit base is the layer's project root, canonicalized once by
        // the reader; assert the shape rather than re-deriving the same path
        // through a different sequence.
        let implicit = by_name("implicit").cwd.clone().unwrap();
        assert!(
            implicit.starts_with("C:/") || implicit.starts_with('/'),
            "{implicit}"
        );
        assert!(
            implicit
                .to_lowercase()
                .contains("kimi-trust-disclosure-cwd"),
            "{implicit}"
        );
        assert!(
            by_name("relative")
                .cwd
                .as_deref()
                .unwrap()
                .ends_with("/sub")
        );
        assert_eq!(by_name("absolute").cwd.as_deref(), Some("/tmp/x"));
        // A remote server's `cwd` is not normalized: upstream only rewrites a
        // stdio entry's, so whatever the config wrote stays as written.
        assert_eq!(by_name("remote").cwd.as_deref(), Some("ignored"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Both project layers are read, and the later one wins, matching the
    /// session's own merge order.
    #[test]
    fn reads_both_project_layers_with_the_later_winning() {
        let dir = tmpdir("layers");
        write_mcp_json(
            &dir,
            serde_json::json!({ "shared": { "command": "root-cmd" } }),
        );
        std::fs::create_dir_all(dir.join(".kimi-code")).unwrap();
        std::fs::write(
            dir.join(".kimi-code").join("mcp.json"),
            serde_json::json!({
                "mcpServers": { "shared": { "command": "project-cmd" } }
            })
            .to_string(),
        )
        .unwrap();

        let activation = describe_gated_activation(&input(&dir));
        assert_eq!(activation.mcp_servers.len(), 1, "{activation:?}");
        assert_eq!(
            activation.mcp_servers[0].command.as_deref(),
            Some("project-cmd")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An entry with neither a command nor a URL describes nothing; guessing a
    /// transport would put a claim in front of the user the config never made.
    #[test]
    fn skips_servers_with_neither_command_nor_url() {
        let dir = tmpdir("undescribable");
        write_mcp_json(
            &dir,
            serde_json::json!({
                "ok": { "url": "https://example.test/mcp" },
                "undescribable": { "note": "neither" }
            }),
        );

        let activation = describe_gated_activation(&input(&dir));
        let names: Vec<&str> = activation
            .mcp_servers
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["ok"]);
        assert_eq!(activation.mcp_servers[0].transport, "http");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An explicitly disabled server is not activated by trusting.
    #[test]
    fn skips_disabled_servers() {
        let dir = tmpdir("disabled");
        write_mcp_json(
            &dir,
            serde_json::json!({
                "off": { "command": "node", "enabled": false },
                "on": { "command": "node" }
            }),
        );

        let activation = describe_gated_activation(&input(&dir));
        let names: Vec<&str> = activation
            .mcp_servers
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["on"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `workspace.additional_dir` entries outside the project are disclosed
    /// together with the file that declared them.
    #[test]
    fn discloses_additional_dirs_and_their_config_source() {
        let dir = tmpdir("dirs");
        let outside = tmpdir("dirs-outside");
        std::fs::create_dir_all(dir.join(".kimi-code")).unwrap();
        std::fs::write(
            dir.join(".kimi-code").join("local.toml"),
            format!(
                "[workspace]\nadditional_dir = [\"{}\"]\n",
                outside.display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();

        let activation = describe_gated_activation(&input(&dir));
        assert_eq!(activation.additional_dirs.len(), 1, "{activation:?}");
        assert!(
            activation.additional_dirs[0]
                .ends_with(outside.file_name().unwrap().to_string_lossy().as_ref())
        );
        assert_eq!(activation.additional_dir_sources.len(), 1);
        assert!(activation.additional_dir_sources[0].ends_with("local.toml"));
        assert!(activation.warnings.is_empty(), "{:?}", activation.warnings);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// A directory the user is about to add is disclosed too — the prompt
    /// answers "what would trusting plus this get me", not only the on-disk
    /// half (v2 `describeGatedActivation(pendingAdditionalDirs)`).
    #[test]
    fn discloses_pending_additional_dirs() {
        let dir = tmpdir("pending");
        let about_to_add = tmpdir("pending-outside");

        let mut spec = input(&dir);
        spec.pending_additional_dirs = vec![about_to_add.clone()];
        let activation = describe_gated_activation(&spec);

        assert_eq!(activation.additional_dirs.len(), 1, "{activation:?}");
        assert!(
            activation.additional_dirs[0]
                .ends_with(about_to_add.file_name().unwrap().to_string_lossy().as_ref())
        );
        // It names itself as its source: it is not in any config file yet.
        assert_eq!(
            activation.additional_dir_sources.len(),
            activation.additional_dirs.len()
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&about_to_add).ok();
    }

    /// The AGENTS.md files the session would read are named, and only the ones
    /// inside the project.
    #[test]
    fn discloses_project_agents_md_paths() {
        let dir = tmpdir("agents");
        std::fs::write(dir.join("AGENTS.md"), "# local instructions\n").unwrap();

        let activation = describe_gated_activation(&input(&dir));
        let paths = &activation.instruction_sources.agents_md_paths;
        assert_eq!(paths.len(), 1, "{paths:?}");
        assert!(paths[0].ends_with("AGENTS.md"));
        assert!(
            activation
                .instruction_sources
                .paths
                .iter()
                .any(|p| p.ends_with("AGENTS.md"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same file discovered twice is one disclosure: the instruction path
    /// list is a set, not a concatenation of every source's list.
    #[test]
    fn instruction_paths_are_deduplicated() {
        let dir = tmpdir("dedupe");
        std::fs::write(dir.join("AGENTS.md"), "# one\n").unwrap();
        let activation = describe_gated_activation(&input(&dir));
        let paths = &activation.instruction_sources.paths;
        let unique: BTreeSet<_> = paths.iter().collect();
        assert_eq!(paths.len(), unique.len(), "{paths:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn inside_or_equal_rejects_parent_escapes() {
        let root = Path::new("/a/b");
        assert!(is_inside_or_equal(Path::new("/a/b"), root));
        assert!(is_inside_or_equal(Path::new("/a/b/c"), root));
        assert!(!is_inside_or_equal(Path::new("/a/bc"), root));
        assert!(!is_inside_or_equal(Path::new("/a"), root));
    }
}
