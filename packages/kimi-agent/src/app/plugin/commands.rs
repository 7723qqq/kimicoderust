use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::_base::text::frontmatter::parse_frontmatter;
use crate::wire::record::WireAsyncError;

use super::types::PluginCommandDef;

#[derive(Debug, Clone)]
pub struct ParseCommandTextInput {
    pub text: String,
    pub command_path: String,
    pub plugin_id: String,
    pub fallback_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoadPluginCommandInput {
    pub command_path: String,
    pub plugin_id: String,
    pub fallback_name: Option<String>,
}

pub fn parse_command_text(input: ParseCommandTextInput) -> Result<PluginCommandDef, WireAsyncError> {
    let parsed = parse_frontmatter(&input.text)?;
    let empty = serde_json::Map::new();
    let frontmatter = parsed.data.as_object().unwrap_or(&empty);
    let base_name = input.fallback_name.unwrap_or_else(|| {
        let file_name = Path::new(&input.command_path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if file_name.to_ascii_lowercase().ends_with(".md") {
            file_name[..file_name.len() - 3].to_string()
        } else {
            file_name
        }
    });
    let name = non_empty_string(frontmatter.get("name")).unwrap_or(base_name);
    let body = parsed.body.trim().to_string();
    let description = non_empty_string(frontmatter.get("description"))
        .unwrap_or_else(|| description_from_body(&body));
    Ok(PluginCommandDef {
        plugin_id: input.plugin_id,
        name,
        description,
        body,
        path: resolve_path(&input.command_path)?,
    })
}

pub async fn load_plugin_command(input: LoadPluginCommandInput) -> Option<PluginCommandDef> {
    let text = tokio::fs::read_to_string(&input.command_path).await.ok()?;
    parse_command_text(ParseCommandTextInput {
        text,
        command_path: input.command_path,
        plugin_id: input.plugin_id,
        fallback_name: input.fallback_name,
    })
    .ok()
}

pub fn expand_command_arguments(body: &str, args: &str) -> String {
    let replaced = body.replace("$ARGUMENTS", args);
    if !body.contains("$ARGUMENTS") && !args.is_empty() {
        return format!("{replaced}\n\nARGUMENTS: {args}");
    }
    replaced
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn description_from_body(body: &str) -> String {
    let Some(first_line) = body.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return "No description provided.".to_string();
    };
    if first_line.chars().count() > 240 {
        return format!("{}…", first_line.chars().take(239).collect::<String>());
    }
    first_line.to_string()
}

fn resolve_path(path: &str) -> Result<String, WireAsyncError> {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(absolute.to_string_lossy().into_owned())
}
