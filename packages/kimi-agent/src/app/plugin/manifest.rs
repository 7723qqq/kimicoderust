use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::features::externalHooks::configSection::HookDefConfig;
use crate::mcp_core::config_schema::{McpServerConfig, McpTransport, validate_mcp_server_config};

use super::types::{
    PLUGIN_NAME_REGEX, PluginAuthor, PluginCommandEntry, PluginDiagnostic,
    PluginDiagnosticSeverity, PluginInterface, PluginInterfaceOverride, PluginManifest,
    PluginManifestKind, PluginSessionStart,
};

const KIMI_PLUGIN_ROOT_PATH: &str = "kimi.plugin.json";
const KIMI_PLUGIN_DIR_PATH: &str = ".kimi-plugin/plugin.json";
pub const PLUGIN_SYSTEM_PROMPT_MAX_BYTES: usize = 32 * 1024;

const UNSUPPORTED_RUNTIME_FIELDS: &[&str] = &[
    "tools",
    "apps",
    "inject",
    "configFile",
    "config_file",
    "bootstrap",
];

const KNOWN_TOP_LEVEL_FIELDS: &[&str] = &[
    "name",
    "displayName",
    "version",
    "description",
    "shortDescription",
    "longDescription",
    "keywords",
    "author",
    "developerName",
    "email",
    "websiteURL",
    "homepage",
    "license",
    "skills",
    "agents",
    "commands",
    "hooks",
    "mcpServers",
    "interface",
    "sessionStart",
    "skillInstructions",
    "systemPrompt",
    "systemPromptPath",
];

#[derive(Debug, Clone, Default)]
pub struct ParsedManifestResult {
    pub manifest: Option<PluginManifest>,
    pub interface_locales: Option<HashMap<String, PluginInterface>>,
    pub manifest_kind: Option<PluginManifestKind>,
    pub manifest_path: Option<String>,
    pub shadowed_manifest_path: Option<String>,
    pub diagnostics: Vec<PluginDiagnostic>,
}

pub async fn parse_manifest(plugin_root: &str) -> ParsedManifestResult {
    let root = PathBuf::from(plugin_root);
    let root_json_path = root.join(KIMI_PLUGIN_ROOT_PATH);
    let dir_json_path = root.join(KIMI_PLUGIN_DIR_PATH);
    let root_json_exists = is_file(&root_json_path).await;
    let dir_json_exists = is_file(&dir_json_path).await;
    if !root_json_exists && !dir_json_exists {
        return ParsedManifestResult {
            diagnostics: vec![diagnostic(
                PluginDiagnosticSeverity::Error,
                format!("No manifest at {KIMI_PLUGIN_ROOT_PATH} or {KIMI_PLUGIN_DIR_PATH}"),
            )],
            ..Default::default()
        };
    }
    let manifest_path = if root_json_exists {
        root_json_path
    } else {
        dir_json_path.clone()
    };
    let manifest_kind = if root_json_exists {
        PluginManifestKind::KimiPluginRoot
    } else {
        PluginManifestKind::KimiPluginDir
    };
    let manifest_path_string = path_string(&manifest_path);
    let shadowed_manifest_path = (root_json_exists && dir_json_exists)
        .then(|| path_string(&dir_json_path));
    let metadata = || ParsedManifestResult {
        manifest_kind: Some(manifest_kind),
        manifest_path: Some(manifest_path_string.clone()),
        shadowed_manifest_path: shadowed_manifest_path.clone(),
        ..Default::default()
    };
    let raw = match tokio::fs::read_to_string(&manifest_path).await {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(raw) => raw,
            Err(error) => {
                let mut result = metadata();
                result.diagnostics.push(diagnostic(
                    PluginDiagnosticSeverity::Error,
                    format!(
                        "Failed to parse {}: {error}",
                        relative_path(&root, &manifest_path)
                    ),
                ));
                return result;
            }
        },
        Err(error) => {
            let mut result = metadata();
            result.diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Error,
                format!(
                    "Failed to parse {}: {error}",
                    relative_path(&root, &manifest_path)
                ),
            ));
            return result;
        }
    };
    let Some(raw) = raw.as_object() else {
        let mut result = metadata();
        result.diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Error,
            "manifest must be a JSON object",
        ));
        return result;
    };
    let mut diagnostics = Vec::new();
    let name = raw
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Error,
            "\"name\" is required",
        ));
        return ParsedManifestResult {
            diagnostics,
            ..metadata()
        };
    }
    if !PLUGIN_NAME_REGEX.is_match(&name) {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Error,
            format!(
                "\"name\" must match /{}/ (got \"{name}\")",
                PLUGIN_NAME_REGEX.as_str()
            ),
        ));
        return ParsedManifestResult {
            diagnostics,
            ..metadata()
        };
    }
    let mut skills = resolve_dir_list_field(plugin_root, "skills", raw.get("skills"), &mut diagnostics).await;
    let mut root_skill_fallback = None;
    if !raw.contains_key("skills") && is_file(&root.join("SKILL.md")).await {
        skills = vec![plugin_root.to_string()];
        root_skill_fallback = Some(true);
    }
    let mut agents = resolve_dir_list_field(plugin_root, "agents", raw.get("agents"), &mut diagnostics).await;
    if !raw.contains_key("agents") && is_dir(&root.join("agents")).await {
        agents = vec![path_string(&root.join("agents"))];
    }
    let skill_instructions = raw
        .get("skillInstructions")
        .and_then(Value::as_str)
        .map(str::to_string);
    let system_prompt = read_system_prompt(plugin_root, raw, &mut diagnostics).await;
    record_unsupported_runtime_fields(raw, &mut diagnostics);
    let manifest = PluginManifest {
        name,
        version: string_field(raw, "version"),
        description: string_field(raw, "description"),
        keywords: string_array_field(raw, "keywords"),
        homepage: string_field(raw, "homepage"),
        license: string_field(raw, "license"),
        author: read_author(raw.get("author")),
        skills: Some(skills),
        root_skill_fallback,
        agents: Some(agents),
        session_start: read_session_start(raw.get("sessionStart"), &mut diagnostics),
        mcp_servers: read_mcp_servers(plugin_root, raw.get("mcpServers"), &mut diagnostics).await,
        hooks: read_hooks(raw.get("hooks"), &mut diagnostics),
        commands: read_commands(plugin_root, raw.get("commands"), &mut diagnostics).await,
        interface: read_interface(raw.get("interface")),
        skill_instructions,
        system_prompt,
    };
    let interface_locales = read_interface_locales(plugin_root, &manifest, &mut diagnostics).await;
    ParsedManifestResult {
        manifest: Some(manifest),
        interface_locales,
        diagnostics,
        ..metadata()
    }
}

fn record_unsupported_runtime_fields(raw: &Map<String, Value>, diagnostics: &mut Vec<PluginDiagnostic>) {
    for field in UNSUPPORTED_RUNTIME_FIELDS {
        if raw.contains_key(*field) {
            diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Info,
                format!("\"{field}\" is present but not supported by Kimi plugins"),
            ));
        }
    }
    record_unknown_fields(raw, diagnostics);
}

fn record_unknown_fields(raw: &Map<String, Value>, diagnostics: &mut Vec<PluginDiagnostic>) {
    for key in raw.keys() {
        if KNOWN_TOP_LEVEL_FIELDS.contains(&key.as_str())
            || UNSUPPORTED_RUNTIME_FIELDS.contains(&key.as_str())
        {
            continue;
        }
        let near = nearest_field(key, KNOWN_TOP_LEVEL_FIELDS);
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Warn,
            match near {
                Some(near) => format!(
                    "\"{key}\" is not a recognized plugin field and was ignored; did you mean \"{near}\"?"
                ),
                None => format!("\"{key}\" is not a recognized plugin field and was ignored"),
            },
        ));
    }
}

fn nearest_field(key: &str, candidates: &[&str]) -> Option<String> {
    let normalized = normalize_field_name(key);
    let mut best = None;
    let mut best_distance = usize::MAX;
    for candidate in candidates {
        let distance = edit_distance(&normalized, &normalize_field_name(candidate));
        if distance < best_distance {
            best_distance = distance;
            best = Some(*candidate);
        }
    }
    let threshold = (normalized.chars().count() / 3).max(1);
    (best_distance <= threshold).then(|| best.unwrap().to_string())
}

fn normalize_field_name(name: &str) -> String {
    name.chars()
        .filter(|character| !matches!(character, '_' | '-'))
        .collect::<String>()
        .to_lowercase()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a = a.chars().collect::<Vec<_>>();
    let b = b.chars().collect::<Vec<_>>();
    let mut previous = (0..=b.len()).collect::<Vec<_>>();
    for (i, left) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, right) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(left != right);
            let insertion = current[j] + 1;
            let deletion = previous[j + 1] + 1;
            current.push(substitution.min(insertion).min(deletion));
        }
        previous = current;
    }
    previous[b.len()]
}

async fn resolve_dir_list_field(
    plugin_root: &str,
    field: &str,
    raw: Option<&Value>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let entries = if let Some(entry) = raw.as_str() {
        vec![entry.to_string()]
    } else if let Some(raw_entries) = raw.as_array().filter(|entries| entries.iter().all(Value::is_string)) {
        raw_entries
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    } else {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Error,
            format!("\"{field}\" must be a string or string[]"),
        ));
        return Vec::new();
    };
    let mut resolved = Vec::new();
    for entry in entries {
        if !entry.starts_with("./") {
            diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Error,
                format!("\"{field}\" path must start with \"./\" (got \"{entry}\")"),
            ));
            continue;
        }
        let absolute = absolute_path(&Path::new(plugin_root).join(&entry));
        let real = realpath_or(&absolute).await;
        let root_real = realpath_or(Path::new(plugin_root)).await;
        if !is_within(&real, &root_real) {
            diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Error,
                format!("\"{field}\" path resolves outside the plugin ({entry})"),
            ));
            continue;
        }
        if !is_dir(&real).await {
            diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Warn,
                format!("\"{field}\" path is not a directory ({entry})"),
            ));
            continue;
        }
        resolved.push(path_string(&real));
    }
    resolved
}

async fn resolve_plugin_path_field(
    plugin_root: &str,
    field: &str,
    value: &str,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<String> {
    if !value.starts_with("./") {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Warn,
            format!("\"{field}\" path must start with \"./\" (got \"{value}\")"),
        ));
        return None;
    }
    let absolute = absolute_path(&Path::new(plugin_root).join(value));
    let real = realpath_or(&absolute).await;
    let root_real = realpath_or(Path::new(plugin_root)).await;
    if !is_within(&real, &root_real) {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Warn,
            format!("\"{field}\" path resolves outside the plugin ({value})"),
        ));
        return None;
    }
    Some(path_string(&real))
}

fn read_session_start(raw: Option<&Value>, diagnostics: &mut Vec<PluginDiagnostic>) -> Option<PluginSessionStart> {
    let raw = raw?;
    let Some(raw) = raw.as_object() else {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Warn,
            "\"sessionStart\" must be an object",
        ));
        return None;
    };
    let skill = raw
        .get("skill")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if skill.is_empty() {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Warn,
            "\"sessionStart.skill\" is required when sessionStart is present",
        ));
        return None;
    }
    Some(PluginSessionStart { skill })
}

async fn read_system_prompt(
    plugin_root: &str,
    raw: &Map<String, Value>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<String> {
    let mut parts = Vec::new();
    if raw.get("systemPrompt").is_some_and(|value| !value.is_string()) {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Warn,
            "\"systemPrompt\" must be a string",
        ));
    }
    if let Some(inline) = string_field(raw, "systemPrompt") {
        let inline_bytes = inline.len();
        if inline_bytes > PLUGIN_SYSTEM_PROMPT_MAX_BYTES {
            diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Warn,
                format!(
                    "\"systemPrompt\" is {inline_bytes} bytes, exceeding the {} KB limit; the field is ignored",
                    PLUGIN_SYSTEM_PROMPT_MAX_BYTES / 1024
                ),
            ));
        } else {
            parts.push(inline);
        }
    }
    if let Some(path_value) = raw.get("systemPromptPath") {
        let Some(path_value) = path_value.as_str() else {
            diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Warn,
                "\"systemPromptPath\" must be a string",
            ));
            return finish_system_prompt(parts);
        };
        if path_value.trim().is_empty() {
            diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Warn,
                "\"systemPromptPath\" must not be blank",
            ));
        } else {
            let original_path = path_value.to_string();
            let resolved = resolve_plugin_path_field(
                plugin_root,
                "systemPromptPath",
                path_value.trim(),
                diagnostics,
            )
            .await;
            if let Some(resolved) = resolved {
                let metadata = tokio::fs::metadata(&resolved).await.ok();
                match metadata {
                    None => diagnostics.push(diagnostic(
                        PluginDiagnosticSeverity::Warn,
                        format!("\"systemPromptPath\" is not a file ({original_path})"),
                    )),
                    Some(metadata) if !metadata.is_file() => diagnostics.push(diagnostic(
                        PluginDiagnosticSeverity::Warn,
                        format!("\"systemPromptPath\" is not a file ({original_path})"),
                    )),
                    Some(metadata) if metadata.len() > PLUGIN_SYSTEM_PROMPT_MAX_BYTES as u64 => {
                        diagnostics.push(diagnostic(
                            PluginDiagnosticSeverity::Warn,
                            format!(
                                "\"systemPromptPath\" is {} bytes, exceeding the {} KB limit; the file is ignored ({original_path})",
                                metadata.len(),
                                PLUGIN_SYSTEM_PROMPT_MAX_BYTES / 1024
                            ),
                        ));
                    }
                    Some(_) => match tokio::fs::read_to_string(&resolved).await {
                        Ok(content) => {
                            let content = content.strip_prefix('\u{feff}').unwrap_or(&content).trim();
                            if !content.is_empty() {
                                parts.push(content.to_string());
                            }
                        }
                        Err(error) => diagnostics.push(diagnostic(
                            PluginDiagnosticSeverity::Warn,
                            format!(
                                "Failed to read \"systemPromptPath\" ({original_path}): {error}"
                            ),
                        )),
                    },
                }
            }
        }
    }
    finish_system_prompt(parts)
}

fn finish_system_prompt(parts: Vec<String>) -> Option<String> {
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

async fn read_mcp_servers(
    plugin_root: &str,
    raw: Option<&Value>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<HashMap<String, McpServerConfig>> {
    let raw = raw?;
    let Some(raw) = raw.as_object() else {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Warn,
            "\"mcpServers\" must be an object",
        ));
        return None;
    };
    let mut output = HashMap::new();
    for (name, value) in raw {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Warn,
                "\"mcpServers\" entries must have a non-empty name",
            ));
            continue;
        }
        let config = match validate_mcp_server_config(value.clone()) {
            Ok(config) => config,
            Err(error) => {
                diagnostics.push(diagnostic(
                    PluginDiagnosticSeverity::Warn,
                    format!("Invalid MCP server \"{trimmed_name}\": {error}"),
                ));
                continue;
            }
        };
        if let Some(normalized) = normalize_plugin_mcp_server(
            plugin_root,
            trimmed_name,
            config,
            diagnostics,
        )
        .await
        {
            output.insert(trimmed_name.to_string(), normalized);
        }
    }
    (!output.is_empty()).then_some(output)
}

fn read_hooks(raw: Option<&Value>, diagnostics: &mut Vec<PluginDiagnostic>) -> Option<Vec<HookDefConfig>> {
    let raw = raw?;
    let Some(entries) = raw.as_array() else {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Warn,
            "\"hooks\" must be an array",
        ));
        return None;
    };
    let mut output = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        match serde_json::from_value::<HookDefConfig>(entry.clone()) {
            Ok(hook) => output.push(hook),
            Err(error) => diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Warn,
                format!("Invalid hook at index {index}: {error}"),
            )),
        }
    }
    (!output.is_empty()).then_some(output)
}

async fn read_commands(
    plugin_root: &str,
    raw: Option<&Value>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<Vec<PluginCommandEntry>> {
    let raw = raw?;
    let entries = if let Some(entry) = raw.as_str() {
        vec![entry.to_string()]
    } else if let Some(raw_entries) = raw.as_array().filter(|entries| entries.iter().all(Value::is_string)) {
        raw_entries
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    } else {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Warn,
            "\"commands\" must be a string or string[]",
        ));
        return None;
    };
    let mut files = Vec::new();
    for entry in entries {
        let Some(resolved) = resolve_plugin_path_field(
            plugin_root,
            "commands",
            &entry,
            diagnostics,
        )
        .await
        else {
            continue;
        };
        let resolved_path = PathBuf::from(&resolved);
        if is_dir(&resolved_path).await {
            files.extend(list_markdown_files_recursive(&resolved_path).await);
        } else if is_file(&resolved_path).await && resolved.ends_with(".md") {
            let parent = resolved_path.parent().unwrap_or(&resolved_path);
            files.push(PluginCommandEntry {
                name: command_name_from_file(&resolved_path, parent),
                path: resolved,
            });
        } else {
            diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Warn,
                format!("\"commands\" entry must be a directory or .md file ({entry})"),
            ));
        }
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));
    (!files.is_empty()).then_some(files)
}

async fn list_markdown_files_recursive(root: &Path) -> Vec<PluginCommandEntry> {
    let mut output = Vec::new();
    walk_markdown(root, root, &mut output).await;
    output
}

fn walk_markdown<'a>(
    root: &'a Path,
    dir: &'a Path,
    output: &'a mut Vec<PluginCommandEntry>,
) -> futures_util::future::BoxFuture<'a, ()> {
    Box::pin(async move {
        let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let full = entry.path();
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_dir() {
                walk_markdown(root, &full, output).await;
            } else if file_type.is_file()
                && entry.file_name().to_string_lossy().ends_with(".md")
            {
                output.push(PluginCommandEntry {
                    name: command_name_from_file(&full, root),
                    path: path_string(&full),
                });
            }
        }
    })
}

fn command_name_from_file(file: &Path, root: &Path) -> String {
    let relative = file.strip_prefix(root).unwrap_or(file);
    let rendered = relative.to_string_lossy().replace('\\', "/");
    if rendered.to_ascii_lowercase().ends_with(".md") {
        rendered[..rendered.len() - 3].to_string()
    } else {
        rendered
    }
}

async fn normalize_plugin_mcp_server(
    plugin_root: &str,
    name: &str,
    mut config: McpServerConfig,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<McpServerConfig> {
    if matches!(config.transport, McpTransport::Http | McpTransport::Sse) {
        return Some(config);
    }
    let mut command = config.command.clone().unwrap_or_default();
    if command.starts_with("./") {
        command = resolve_plugin_path_field(
            plugin_root,
            &format!("mcpServers.{name}.command"),
            &command,
            diagnostics,
        )
        .await?;
    } else if command.contains('/') || Path::new(&command).is_absolute() {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Warn,
            format!("\"mcpServers.{name}.command\" must be a PATH command or start with \"./\""),
        ));
        return None;
    }
    let mut cwd = config.cwd.clone();
    if let Some(value) = cwd {
        cwd = Some(
            resolve_plugin_path_field(
                plugin_root,
                &format!("mcpServers.{name}.cwd"),
                &value,
                diagnostics,
            )
            .await?,
        );
    }
    config.command = Some(command);
    config.cwd = cwd;
    Some(config)
}

fn read_author(raw: Option<&Value>) -> Option<PluginAuthor> {
    let raw = raw?;
    if let Some(name) = raw.as_str() {
        return Some(PluginAuthor {
            name: Some(name.to_string()),
            email: None,
        });
    }
    let raw = raw.as_object()?;
    let name = string_field(raw, "name");
    let email = string_field(raw, "email");
    if name.is_none() && email.is_none() {
        return None;
    }
    Some(PluginAuthor { name, email })
}

fn read_interface(raw: Option<&Value>) -> Option<PluginInterface> {
    let raw = raw?.as_object()?;
    let output = PluginInterface {
        display_name: string_field(raw, "displayName"),
        short_description: string_field(raw, "shortDescription"),
        long_description: string_field(raw, "longDescription"),
        developer_name: string_field(raw, "developerName"),
        website_url: string_field(raw, "websiteURL"),
        default_locale: string_field(raw, "defaultLocale"),
        locales: string_field(raw, "locales"),
        mcp_overrides: read_interface_overrides(raw.get("mcpOverrides")),
        skill_overrides: read_interface_overrides(raw.get("skillOverrides")),
    };
    (!output.is_empty()).then_some(output)
}

fn read_interface_overrides(raw: Option<&Value>) -> Option<HashMap<String, PluginInterfaceOverride>> {
    let raw = raw?.as_object()?;
    let mut output = HashMap::new();
    for (name, value) in raw {
        let Some(value) = value.as_object() else {
            continue;
        };
        let override_ = PluginInterfaceOverride {
            display_name: string_field(value, "displayName"),
            description: string_field(value, "description"),
            icon_url: string_field(value, "iconUrl"),
        };
        if !override_.is_empty() {
            output.insert(name.clone(), override_);
        }
    }
    (!output.is_empty()).then_some(output)
}

async fn read_interface_locales(
    plugin_root: &str,
    manifest: &PluginManifest,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<HashMap<String, PluginInterface>> {
    let value = manifest.interface.as_ref()?.locales.clone()?;
    let locale_root = resolve_plugin_path_field(
        plugin_root,
        "interface.locales",
        &value,
        diagnostics,
    )
    .await?;
    let locale_root = PathBuf::from(locale_root);
    if !is_dir(&locale_root).await {
        diagnostics.push(diagnostic(
            PluginDiagnosticSeverity::Warn,
            "\"interface.locales\" path is not a directory",
        ));
        return None;
    }
    let mut entries = match tokio::fs::read_dir(&locale_root).await {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Warn,
                format!("Failed to read \"interface.locales\": {error}"),
            ));
            return None;
        }
    };
    let mut output = HashMap::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let file = entry.file_name().to_string_lossy().into_owned();
        if !file.ends_with(".json") {
            continue;
        }
        let locale = file[..file.len() - 5].to_string();
        let real = match tokio::fs::canonicalize(entry.path()).await {
            Ok(real) => real,
            Err(_) => {
                diagnostics.push(diagnostic(
                    PluginDiagnosticSeverity::Warn,
                    format!("\"interface.locales\" file is invalid ({file})"),
                ));
                continue;
            }
        };
        if !is_within(&real, &locale_root) || !is_file(&real).await {
            diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Warn,
                format!("\"interface.locales\" file is invalid ({file})"),
            ));
            continue;
        }
        match tokio::fs::read_to_string(&real).await {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(raw) => match read_interface(Some(&raw)) {
                    Some(localized) => {
                        output.insert(locale, localized);
                    }
                    None => diagnostics.push(diagnostic(
                        PluginDiagnosticSeverity::Warn,
                        format!(
                            "\"interface.locales\" file has no supported display fields ({file})"
                        ),
                    )),
                },
                Err(error) => diagnostics.push(diagnostic(
                    PluginDiagnosticSeverity::Warn,
                    format!("Failed to parse interface locale \"{locale}\": {error}"),
                )),
            },
            Err(error) => diagnostics.push(diagnostic(
                PluginDiagnosticSeverity::Warn,
                format!("Failed to parse interface locale \"{locale}\": {error}"),
            )),
        }
    }
    (!output.is_empty()).then_some(output)
}

fn string_field(raw: &Map<String, Value>, key: &str) -> Option<String> {
    let trimmed = raw.get(key)?.as_str()?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn string_array_field(raw: &Map<String, Value>, key: &str) -> Option<Vec<String>> {
    let values = raw.get(key)?.as_array()?;
    if !values.iter().all(Value::is_string) {
        return None;
    }
    Some(
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

fn is_within(child: &Path, parent: &Path) -> bool {
    let Ok(relative) = child.strip_prefix(parent) else {
        return false;
    };
    !relative.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    })
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}

async fn is_dir(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
}

async fn realpath_or(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf())
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn diagnostic(severity: PluginDiagnosticSeverity, message: impl Into<String>) -> PluginDiagnostic {
    PluginDiagnostic {
        severity,
        message: message.into(),
    }
}
