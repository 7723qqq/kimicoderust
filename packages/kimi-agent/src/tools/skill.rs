use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{arg_error, err_result};
use crate::callbacks::HostCallbacks;
use crate::rpc::types::{ContentBlock, StateReadRequest, ToolDelivery};
use crate::turn_loop::types::ExecutableToolResult;

const SKILL_NOT_FOUND_MESSAGE: &str = "not found in the current skill listing.";

pub const MAX_SKILL_QUERY_DEPTH: u32 = 3;

pub fn nested_skill_too_deep_error(depth: u32, skill_name: Option<&str>) -> String {
    let label = skill_name
        .map(|name| format!(" \"{name}\""))
        .unwrap_or_default();
    format!(
        "Nested skill invocation{label} exceeded the maximum depth of {depth} — refusing to recurse further."
    )
}

const STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE: &str = "The connected client does not support the state bridge. Do NOT call this tool again — the host cannot load skills.";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillWire {
    name: String,
    instructions: String,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    disable_model_invocation: Option<bool>,
    #[serde(default)]
    skill_type: Option<String>,
    #[serde(default)]
    arguments: Option<SkillArgumentsWire>,
    #[serde(default)]
    argument_names: Option<Vec<String>>,
    #[serde(default)]
    plugin: Option<SkillPluginWire>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SkillArgumentsWire {
    String(String),
    List(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillPluginWire {
    id: String,
    #[serde(default)]
    instructions: Option<String>,
}

fn tokenize_args(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_content = false;

    for c in raw.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                current.push(c);
                has_content = true;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            has_content = true;
            continue;
        }
        if c.is_whitespace() {
            if has_content {
                out.push(current);
                current = String::new();
                has_content = false;
            }
            continue;
        }
        current.push(c);
        has_content = true;
    }

    if has_content {
        out.push(current);
    }
    out
}

pub fn escape_xml_tags(input: &str) -> String {
    input.replace('<', "&lt;").replace('>', "&gt;")
}

fn escape_xml_attr(input: &str) -> String {
    input.replace('&', "&amp;").replace('"', "&quot;")
}

fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn replace_named_placeholder(content: &str, name: &str, replacement: &str) -> String {
    let target = format!("${name}");
    let mut result = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(pos) = rest.find(&target) {
        result.push_str(&rest[..pos]);
        let after_pos = pos + target.len();
        let next_char = rest[after_pos..].chars().next();

        let is_word_or_bracket =
            next_char.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '[');

        if is_word_or_bracket {
            result.push_str(&target);
        } else {
            result.push_str(replacement);
        }
        rest = &rest[after_pos..];
    }
    result.push_str(rest);
    result
}

fn replace_positional_placeholders(content: &str, tokens: &[String]) -> String {
    let mut result = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(pos) = rest.find('$') {
        result.push_str(&rest[..pos]);
        let after_dollar = &rest[pos + 1..];
        let digit_count = after_dollar
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .count();

        if digit_count > 0 {
            let digits = &after_dollar[..digit_count];
            let after_digits = &after_dollar[digit_count..];
            let next_char = after_digits.chars().next();
            let is_word = next_char.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');

            if !is_word && let Ok(index) = digits.parse::<usize>() {
                let val = tokens.get(index).map(|s| s.as_str()).unwrap_or("");
                result.push_str(&escape_xml_tags(val));
                rest = after_digits;
                continue;
            }
        }

        result.push('$');
        rest = after_dollar;
    }
    result.push_str(rest);
    result
}

static ARGUMENTS_INDEXED_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\$ARGUMENTS\[(\d+)\]").unwrap());

pub(crate) fn expand_skill_parameters(
    body: &str,
    raw_args: &str,
    skill_dir: &str,
    session_id: &str,
    argument_names: &[String],
) -> String {
    let tokens = tokenize_args(raw_args);
    let mut content = body.to_string();

    for (index, name) in argument_names.iter().enumerate() {
        let val = escape_xml_tags(tokens.get(index).map(|s| s.as_str()).unwrap_or(""));
        content = replace_named_placeholder(&content, name, &val);
    }

    content = ARGUMENTS_INDEXED_RE
        .replace_all(&content, |caps: &regex::Captures| {
            let index: usize = caps[1].parse().unwrap_or(usize::MAX);
            escape_xml_tags(tokens.get(index).map(|s| s.as_str()).unwrap_or(""))
        })
        .into_owned();

    content = replace_positional_placeholders(&content, &tokens);

    content = content.replace("$ARGUMENTS", &escape_xml_tags(raw_args));

    let has_argument_placeholder = content != body;

    content = content
        .replace("${KIMI_SKILL_DIR}", skill_dir)
        .replace("${KIMI_SESSION_ID}", session_id);

    if !has_argument_placeholder && !raw_args.is_empty() {
        return format!("{}\n\nARGUMENTS: {}", content, escape_xml_tags(raw_args));
    }

    content
}

fn resolve_argument_names(wire: &SkillWire) -> Vec<String> {
    if let Some(ref names) = wire.argument_names {
        return names.clone();
    }
    match &wire.arguments {
        Some(SkillArgumentsWire::String(s)) => s
            .split_whitespace()
            .filter(|n| !n.trim().is_empty() && !n.chars().all(|c| c.is_ascii_digit()))
            .map(|n| n.to_string())
            .collect(),
        Some(SkillArgumentsWire::List(list)) => list
            .iter()
            .filter(|n| !n.trim().is_empty() && !n.chars().all(|c| c.is_ascii_digit()))
            .cloned()
            .collect(),
        None => Vec::new(),
    }
}

pub(crate) fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    match trimmed.strip_prefix("---") {
        Some(rest) => match rest.find("\n---") {
            Some(idx) => rest[idx + "\n---".len()..].trim_start_matches(['\r', '\n']),
            None => trimmed,
        },
        None => trimmed,
    }
}

pub fn is_user_activatable_skill_type(skill_type: Option<&str>) -> bool {
    crate::skills::is_user_activatable_skill_type(skill_type)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillPromptError {
    NotFound,

    TypeUnsupported(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSlashSkillPrompt {
    pub text: String,
    pub name: String,
    pub path: Option<String>,
    pub source: Option<String>,
    pub skill_type: Option<String>,
}

pub fn render_user_slash_skill_prompt(
    scan: &SkillScan<'_>,
    name: &str,
    raw_args: &str,
    session_id: &str,
) -> Result<UserSlashSkillPrompt, SkillPromptError> {
    let resolved = scan_skill(name, scan).ok_or(SkillPromptError::NotFound)?;
    if !is_user_activatable_skill_type(resolved.skill_type.as_deref()) {
        return Err(SkillPromptError::TypeUnsupported(resolved.name.clone()));
    }

    let skill_dir = resolved.dir.clone().unwrap_or_default();
    let expanded = expand_skill_parameters(
        &resolved.instructions,
        raw_args,
        &skill_dir,
        session_id,
        &resolved.argument_names,
    );
    let content = prefix_plugin_instructions(&resolved.plugin, expanded);
    let attrs = render_skill_attributes(&resolved, raw_args, "user-slash");
    let block = format!("<skill-loaded{attrs}>\n{content}\n</skill-loaded>");

    Ok(UserSlashSkillPrompt {
        text: format!(
            "User activated the skill \"{}\". Follow the loaded skill instructions.\n\n{block}",
            escape_xml(&resolved.name)
        ),
        name: resolved.name,
        path: resolved.path,
        source: resolved.source,
        skill_type: resolved.skill_type,
    })
}

struct ResolvedSkill {
    name: String,
    instructions: String,
    dir: Option<String>,
    path: Option<String>,
    source: Option<String>,
    skill_type: Option<String>,
    disable_model_invocation: bool,
    argument_names: Vec<String>,
    plugin: Option<SkillPluginWire>,
}

impl ResolvedSkill {
    fn from_wire(wire: SkillWire) -> Self {
        let argument_names = resolve_argument_names(&wire);
        Self {
            name: wire.name,
            instructions: wire.instructions,
            dir: wire.dir,
            path: wire.path,
            source: wire.source,
            skill_type: wire.skill_type,
            disable_model_invocation: wire.disable_model_invocation == Some(true),
            argument_names,
            plugin: wire.plugin,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SkillScan<'a> {
    pub root: Option<&'a Path>,
    pub extra_dirs: &'a [PathBuf],

    pub plugin_dirs: &'a [crate::skills::PluginSkillDir],
    pub merge_all_available_skills: bool,
}

fn scan_skill(name: &str, scan: &SkillScan<'_>) -> Option<ResolvedSkill> {
    let found = crate::skills::scan_all_skills_with_extra_merge_and_plugins(
        scan.root,
        scan.extra_dirs,
        scan.plugin_dirs,
        scan.merge_all_available_skills,
    )
    .into_iter()
    .find(|s| s.name == name)?;

    let plugin = Path::new(&found.path)
        .parent()
        .and_then(Path::parent)
        .and_then(|root| scan.plugin_dirs.iter().find(|p| p.dir == root))
        .map(|p| SkillPluginWire {
            id: p.plugin_id.clone(),
            instructions: p.instructions.clone(),
        });

    if found.source == "builtin" {
        let skill_name = found.name.clone();
        let def = crate::skills::builtin_skill_defs()
            .into_iter()
            .find(|def| def.descriptor.name == skill_name)?;

        let pseudo_path = def.descriptor.path.clone();
        return Some(ResolvedSkill {
            name: skill_name,
            instructions: strip_frontmatter(def.body).trim_end().to_string(),
            dir: Some(pseudo_path.clone()),
            path: Some(pseudo_path),
            source: Some(def.descriptor.source.clone()),
            skill_type: None,
            disable_model_invocation: def.descriptor.disable_model_invocation,
            argument_names: Vec::new(),
            plugin: None,
        });
    }

    let raw = std::fs::read_to_string(&found.path).ok()?;
    let meta = crate::skills::parse_skill_frontmatter(&raw, &found.name);
    let dir = Path::new(&found.path)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"));
    Some(ResolvedSkill {
        name: found.name,
        instructions: strip_frontmatter(&raw).trim_end().to_string(),
        dir,
        path: Some(found.path),
        source: Some(found.source),
        skill_type: meta.skill_type,
        disable_model_invocation: found.disable_model_invocation,
        argument_names: meta.argument_names,
        plugin,
    })
}

pub async fn execute_skill(
    callbacks: &dyn HostCallbacks,
    session_id: Option<&str>,
    args: &Value,
    scan: SkillScan<'_>,
) -> ExecutableToolResult {
    execute_skill_with_depth(callbacks, session_id, args, scan, 0).await
}

pub async fn execute_skill_with_depth(
    callbacks: &dyn HostCallbacks,
    session_id: Option<&str>,
    args: &Value,
    scan: SkillScan<'_>,
    query_depth: u32,
) -> ExecutableToolResult {
    let current_depth = query_depth;
    if current_depth >= MAX_SKILL_QUERY_DEPTH {
        let skill_name = args.get("skill").and_then(Value::as_str);
        return err_result(nested_skill_too_deep_error(
            MAX_SKILL_QUERY_DEPTH,
            skill_name,
        ));
    }

    let name = args
        .get("skill")
        .or_else(|| args.get("name"))
        .and_then(|s| s.as_str());
    let Some(name) = name else {
        return arg_error("Skill", "skill", "engine.tools.argMustBeString");
    };
    if name.is_empty() {
        return arg_error("Skill", "skill", "engine.tools.argMustNotBeEmpty");
    }
    let raw_args = args.get("args").and_then(|a| a.as_str()).unwrap_or("");
    let session_id = args
        .get("session_id")
        .and_then(|s| s.as_str())
        .or(session_id);

    let resolved = if let Some(skill) = scan_skill(name, &scan) {
        skill
    } else {
        let request = StateReadRequest {
            domain: "skill".into(),
            key: name.into(),
            turn_id: args
                .get("turn_id")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            tool_call_id: args
                .get("tool_call_id")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
        };
        match callbacks.state_read(request).await {
            Ok(response) => match serde_json::from_value::<SkillWire>(response.value) {
                Ok(wire) => ResolvedSkill::from_wire(wire),
                Err(_) => {
                    return err_result(
                        "Invalid skill state from host: expected { name, instructions }.".into(),
                    );
                }
            },
            Err(error) => return map_state_error(name, error),
        }
    };

    if resolved.disable_model_invocation {
        return err_result(format!(
            "Skill \"{}\" can only be triggered by the user (model invocation is disabled).",
            resolved.name
        ));
    }
    if !crate::skills::is_inline_skill_type(resolved.skill_type.as_deref()) {
        return err_result(format!(
            "Skill \"{}\" is not an inline skill and cannot be invoked by the model in v1.",
            resolved.name
        ));
    }

    let skill_dir = resolved.dir.clone().unwrap_or_default();
    let expanded_instructions = expand_skill_parameters(
        &resolved.instructions,
        raw_args,
        &skill_dir,
        session_id.unwrap_or(""),
        &resolved.argument_names,
    );
    let skill_content = prefix_plugin_instructions(&resolved.plugin, expanded_instructions);

    let activation_id = ulid::Ulid::new().to_string();

    let trigger = if current_depth > 0 {
        "nested-skill"
    } else {
        "model-tool"
    };

    let mut origin = json!({
        "kind": "skill_activation",
        "activationId": activation_id,
        "skillName": resolved.name,
        "trigger": trigger,
    });
    if !raw_args.is_empty() {
        origin["skillArgs"] = json!(raw_args);
    }
    if let Some(ref path) = resolved.path {
        origin["skillPath"] = json!(path);
    }
    if let Some(ref source) = resolved.source {
        origin["skillSource"] = json!(source);
    }

    let mut activated = json!({
        "type": "skill.activated",
        "activationId": activation_id,
        "skillName": resolved.name,
        "trigger": trigger,
    });
    if !raw_args.is_empty() {
        activated["skillArgs"] = json!(raw_args);
    }
    if let Some(ref path) = resolved.path {
        activated["skillPath"] = json!(path);
    }
    if let Some(ref source) = resolved.source {
        activated["skillSource"] = json!(source);
    }
    callbacks.emit_event(activated);
    callbacks.telemetry(json!({
        "event": "skill_invoked",
        "skill_name": resolved.name,
        "trigger": trigger,
    }));

    let attrs = render_skill_attributes(&resolved, raw_args, trigger);
    let block = format!("<skill-loaded{attrs}>\n{skill_content}\n</skill-loaded>");
    let delivery_text =
        format!("Skill tool loaded instructions for this request. Follow them.\n\n{block}");

    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        content: format!(
            "Skill \"{}\" loaded inline. Follow its instructions.",
            resolved.name
        ),
        delivery: Some(ToolDelivery {
            blocks: vec![ContentBlock::Text {
                text: delivery_text,
                meta: None,
            }],
            origin: Some(origin),
        }),
        stop_turn: false,
        is_error: false,
        note: None,
        display: None,
    }
}

fn prefix_plugin_instructions(plugin: &Option<SkillPluginWire>, content: String) -> String {
    if let Some(plugin) = plugin
        && let Some(instructions) = &plugin.instructions
    {
        let trimmed = instructions.trim();
        if !trimmed.is_empty() {
            let escaped_id = escape_xml_attr(&plugin.id);
            return format!(
                "<plugin-instructions plugin=\"{escaped_id}\">\n{trimmed}\n</plugin-instructions>\n\n{content}"
            );
        }
    }
    content
}

fn render_skill_attributes(skill: &ResolvedSkill, raw_args: &str, trigger: &str) -> String {
    let attrs: [(&str, Option<&str>); 5] = [
        ("name", Some(skill.name.as_str())),
        ("trigger", Some(trigger)),
        ("source", skill.source.as_deref()),
        ("dir", skill.dir.as_deref()),
        ("args", Some(raw_args)),
    ];
    attrs
        .iter()
        .filter(|(_, value)| value.is_some())
        .map(|(name, value)| format!(" {name}=\"{}\"", escape_xml(value.unwrap())))
        .collect()
}

fn map_state_error(name: &str, error: String) -> ExecutableToolResult {
    if error.contains("-32002") {
        err_result(format!("Skill \"{name}\" {SKILL_NOT_FOUND_MESSAGE}"))
    } else if error.contains("does not support state bridge") {
        err_result(STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE.into())
    } else {
        err_result(error)
    }
}

pub fn skill_tool_def() -> crate::turn_loop::types::ToolInfo {
    crate::turn_loop::types::ToolInfo {
        name: "Skill".into(),
        description: "Invoke a registered skill from the current skill listing. BLOCKING REQUIREMENT: when a skill from the listing matches the user's request, you MUST call this tool (not free-form text). Do not re-invoke a skill to repeat work already done: if a `<skill-loaded>` block for it with the same `args` is already present in the conversation, follow those instructions directly instead of calling the tool again. Do call the tool again when you need the skill with different arguments — the loaded block was expanded with the earlier `args` and will not reflect new inputs.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The exact name of the skill to invoke, spelled as it appears in the current skill listing (e.g. \"commit\", \"pdf\")."
                },
                "args": {
                    "type": "string",
                    "description": "Optional argument string for the skill, written like a command line (e.g. `-m \"fix bug\"`, `123`, a file path). It is split on whitespace (quotes group a token) and expanded into the skill's placeholders ($NAME, $1, $ARGUMENTS); if the skill body has no placeholders, the whole string is still appended as a trailing `ARGUMENTS:` line. Omit it only when there is nothing to pass."
                }
            },
            "required": ["skill"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::types::{
        BoxFuture, PermissionDecision, StateReadRequest, StateReadResponse, StateWriteRequest,
        StateWriteResponse,
    };
    use std::sync::Arc;

    type Scripted = (
        ScriptedCallbacks,
        Arc<std::sync::Mutex<Option<StateReadRequest>>>,
        Arc<std::sync::Mutex<Vec<Value>>>,
    );

    struct ScriptedCallbacks {
        read_response: Result<StateReadResponse, String>,
        write_response: Result<StateWriteResponse, String>,
        read_received: Arc<std::sync::Mutex<Option<StateReadRequest>>>,
        events: Arc<std::sync::Mutex<Vec<Value>>>,
        telemetry_events: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl HostCallbacks for ScriptedCallbacks {
        fn llm_chat(
            &self,
            _: crate::rpc::types::LlmChatRequest,
        ) -> BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn execute_tool(
            &self,
            _: crate::rpc::types::ToolExecuteRequest,
        ) -> BoxFuture<'static, Result<crate::rpc::types::ToolExecuteResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn check_permission(
            &self,
            _: crate::rpc::types::PermissionCheckRequest,
        ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
            Box::pin(async { Ok(PermissionDecision::allow()) })
        }

        fn state_read(
            &self,
            request: StateReadRequest,
        ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
            *self.read_received.lock().unwrap() = Some(request);
            let response = self.read_response.clone();
            Box::pin(async move { response })
        }

        fn state_write(
            &self,
            _: StateWriteRequest,
        ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
            let response = self.write_response.clone();
            Box::pin(async move { response })
        }

        fn emit_event(&self, event: Value) {
            self.events.lock().unwrap().push(event);
        }

        fn telemetry(&self, event: Value) {
            self.telemetry_events.lock().unwrap().push(event);
        }
    }

    fn scripted(read_response: Result<StateReadResponse, String>) -> Scripted {
        let read_received = Arc::new(std::sync::Mutex::new(None));
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let telemetry_events = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            ScriptedCallbacks {
                read_response,
                write_response: Ok(StateWriteResponse {
                    ok: true,
                    value: Value::Null,
                }),
                read_received: read_received.clone(),
                events: events.clone(),
                telemetry_events,
            },
            read_received,
            events,
        )
    }

    fn read_ok(value: Value) -> Result<StateReadResponse, String> {
        Ok(StateReadResponse { value })
    }

    fn scan(root: Option<&Path>) -> SkillScan<'_> {
        SkillScan {
            root,
            extra_dirs: &[],
            plugin_dirs: &[],
            merge_all_available_skills: true,
        }
    }

    fn sample_skill() -> Value {
        serde_json::json!({
            "name": "commit",
            "instructions": "1. Stage the files.\n2. Write the message."
        })
    }

    fn write_project_skill(root: &Path, name: &str, frontmatter: &str, body: &str) {
        let dir = root.join(".kimi-code").join("skills");
        write_skill_in(&dir, name, frontmatter, body);
    }

    fn write_skill_in(container: &Path, name: &str, frontmatter: &str, body: &str) {
        let dir = container.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n{frontmatter}---\n\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn user_activatable_gate_matches_v2() {
        for ok in [None, Some("prompt"), Some("inline"), Some("flow")] {
            assert!(is_user_activatable_skill_type(ok), "{ok:?} must activate");
        }
        for refused in [Some("reference"), Some("anything-else")] {
            assert!(
                !is_user_activatable_skill_type(refused),
                "{refused:?} must not activate"
            );
        }
    }

    #[test]
    fn user_slash_prompt_renders_the_engine_block_with_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_project_skill(root, "commit", "", "1. Stage the files.");

        let prompt = render_user_slash_skill_prompt(&scan(Some(root)), "commit", "", "ses_1")
            .expect("project skill renders");

        assert!(
            prompt.text.starts_with(
                "User activated the skill \"commit\". Follow the loaded skill instructions.\n\n<skill-loaded "
            ),
            "{}",
            prompt.text
        );
        assert!(
            prompt
                .text
                .contains(" trigger=\"user-slash\" source=\"project\" dir=\""),
            "{}",
            prompt.text
        );
        assert!(
            prompt
                .text
                .ends_with(">\n1. Stage the files.\n</skill-loaded>"),
            "{}",
            prompt.text
        );
        assert_eq!(prompt.name, "commit");
        assert_eq!(prompt.source.as_deref(), Some("project"));
        assert!(prompt.path.is_some());
    }

    #[test]
    fn user_slash_prompt_refuses_a_non_user_activatable_type() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_project_skill(root, "docs-ref", "type: reference\n", "Reference only.");

        assert_eq!(
            render_user_slash_skill_prompt(&scan(Some(root)), "docs-ref", "", "ses_1"),
            Err(SkillPromptError::TypeUnsupported("docs-ref".into()))
        );
    }

    #[test]
    fn user_slash_prompt_reports_an_unknown_name() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            render_user_slash_skill_prompt(&scan(Some(dir.path())), "nope", "", "ses_1"),
            Err(SkillPromptError::NotFound)
        );
    }

    #[test]
    fn user_slash_prompt_expands_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_project_skill(
            root,
            "greet",
            "",
            "Hello $ARGUMENTS, from ${KIMI_SKILL_DIR}",
        );

        let prompt =
            render_user_slash_skill_prompt(&scan(Some(root)), "greet", "world", "ses_1").unwrap();
        assert!(prompt.text.contains("Hello world,"), "{}", prompt.text);
        assert!(!prompt.text.contains("$ARGUMENTS"), "{}", prompt.text);
    }

    #[test]
    fn a_plugin_contributed_skill_is_prefixed_with_the_plugin_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("plugin").join("skills");
        write_skill_in(&plugin_dir, "brand", "", "Use the house voice.");
        let plugin_dirs = [crate::skills::PluginSkillDir {
            dir: plugin_dir,
            plugin_id: "brandpack".into(),
            instructions: Some("  The brand pack's rules.  ".into()),
            session_start: None,
        }];
        let scanned = SkillScan {
            root: None,
            extra_dirs: &[],
            plugin_dirs: &plugin_dirs,
            merge_all_available_skills: true,
        };

        let prompt =
            render_user_slash_skill_prompt(&scanned, "brandpack:brand", "", "ses_1").unwrap();

        assert!(
            prompt.text.contains(
                "<skill-loaded name=\"brandpack:brand\" trigger=\"user-slash\" source=\"extra\" dir=\""
            ),
            "{}",
            prompt.text
        );
        assert!(
            prompt.text.contains(
                "<plugin-instructions plugin=\"brandpack\">\nThe brand pack's rules.\n</plugin-instructions>\n\nUse the house voice."
            ),
            "{}",
            prompt.text
        );
    }

    #[tokio::test]
    async fn the_model_tool_path_prefixes_plugin_instructions_too() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("plugin").join("skills");
        write_skill_in(&plugin_dir, "brand", "", "Use the house voice.");
        let plugin_dirs = [crate::skills::PluginSkillDir {
            dir: plugin_dir,
            plugin_id: "brandpack".into(),
            instructions: Some("The brand pack's rules.".into()),
            session_start: None,
        }];

        let (callbacks, read_received, _) = scripted(Err("no bridge".into()));

        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "brandpack:brand" }),
            SkillScan {
                root: None,
                extra_dirs: &[],
                plugin_dirs: &plugin_dirs,
                merge_all_available_skills: true,
            },
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(read_received.lock().unwrap().is_none(), "the scan answered");
        let ContentBlock::Text { text, .. } = &result.delivery.as_ref().unwrap().blocks[0] else {
            panic!("expected a text block");
        };
        assert!(
            text.contains("<plugin-instructions plugin=\"brandpack\">\nThe brand pack's rules.\n</plugin-instructions>"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn test_renders_skill_output_and_delivery() {
        let (callbacks, read_received, events) = scripted(read_ok(sample_skill()));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "commit" }),
            scan(None),
        )
        .await;
        assert!(!result.is_error);

        assert_eq!(
            result.content,
            "Skill \"commit\" loaded inline. Follow its instructions."
        );

        let delivery = result.delivery.expect("steer delivery");
        let ContentBlock::Text { text, .. } = &delivery.blocks[0] else {
            panic!("expected a text block");
        };
        assert_eq!(
            text,
            "Skill tool loaded instructions for this request. Follow them.\n\n<skill-loaded name=\"commit\" trigger=\"model-tool\" args=\"\">\n1. Stage the files.\n2. Write the message.\n</skill-loaded>"
        );

        let origin = delivery.origin.expect("origin");
        assert_eq!(origin["kind"], "skill_activation");
        assert_eq!(origin["skillName"], "commit");
        assert_eq!(origin["trigger"], "model-tool");

        let request = read_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.domain, "skill");
        assert_eq!(request.key, "commit");
        assert_eq!(request.turn_id, "");
        assert_eq!(request.tool_call_id, "");

        let recorded = events.lock().unwrap();
        let activated = recorded
            .iter()
            .find(|e| e["type"] == "skill.activated")
            .expect("skill.activated event");
        assert_eq!(activated["skillName"], "commit");
        assert_eq!(activated["trigger"], "model-tool");
        assert!(activated.get("skillArgs").is_none());
    }

    #[tokio::test]
    async fn test_not_found_maps_to_v2_message() {
        let (callbacks, _, _) = scripted(Err(
            "State read error: [-32002] unknown skill: commit".into()
        ));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "commit" }),
            scan(None),
        )
        .await;
        assert!(result.is_error);
        assert_eq!(
            result.content,
            "Skill \"commit\" not found in the current skill listing."
        );
    }

    #[tokio::test]
    async fn test_unsupported_host_returns_failure_message() {
        let (callbacks, _, _) = scripted(Err("host does not support state bridge".into()));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "commit" }),
            scan(None),
        )
        .await;
        assert!(result.is_error);
        assert_eq!(result.content, STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE);
    }

    #[tokio::test]
    async fn test_other_host_error_passes_through() {
        let (callbacks, _, _) = scripted(Err(
            "State read error: [-32001] unknown domain: skill".into()
        ));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "commit" }),
            scan(None),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("-32001"));
        assert!(result.content.contains("unknown domain"));
    }

    #[tokio::test]
    async fn test_invalid_args_return_error_without_calling_host() {
        let (callbacks, read_received, _) = scripted(read_ok(sample_skill()));
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "skill": "" }),
            serde_json::json!({ "skill": 42 }),
        ] {
            let result = execute_skill(&callbacks, None, &bad, scan(None)).await;
            assert!(result.is_error, "args: {bad}");
            assert!(result.content.contains("Invalid Skill arguments"));
        }
        assert!(read_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_invalid_wire_shape_returns_error() {
        let (callbacks, _, _) = scripted(read_ok(serde_json::json!({ "name": "commit" })));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "commit" }),
            scan(None),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("Invalid skill state from host"));
    }

    #[tokio::test]
    async fn test_turn_and_tool_call_ids_are_forwarded() {
        let (callbacks, read_received, _) = scripted(read_ok(sample_skill()));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({
                "skill": "commit",
                "turn_id": "turn-42",
                "tool_call_id": "call_abc"
            }),
            scan(None),
        )
        .await;
        assert!(!result.is_error);
        let request = read_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.turn_id, "turn-42");
        assert_eq!(request.tool_call_id, "call_abc");
    }

    #[test]
    fn test_tool_def_matches_v2_schema() {
        let def = skill_tool_def();
        assert_eq!(def.name, "Skill");
        assert_eq!(def.input_schema["type"], "object");
        assert_eq!(def.input_schema["additionalProperties"], false);
        assert_eq!(def.input_schema["required"][0], "skill");
        assert!(def.input_schema["properties"]["skill"].is_object());
        assert!(def.input_schema["properties"]["args"].is_object());
        assert!(def.description.contains("BLOCKING REQUIREMENT"));
        assert!(def.description.contains("<skill-loaded>"));
    }

    #[tokio::test]
    async fn test_renders_skill_with_args_expansion() {
        let (callbacks, _, events) = scripted(read_ok(sample_skill()));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({
                "skill": "commit",
                "args": "-m \"feat: new feature\""
            }),
            scan(None),
        )
        .await;
        assert!(!result.is_error);
        let ContentBlock::Text { text, .. } = &result.delivery.unwrap().blocks[0] else {
            panic!("expected a text block");
        };
        assert!(
            text.contains("args=\"-m &quot;feat: new feature&quot;\""),
            "{text}"
        );
        assert!(
            text.contains("ARGUMENTS: -m \"feat: new feature\""),
            "{text}"
        );

        let activated = events
            .lock()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "skill.activated")
            .unwrap()
            .clone();
        assert_eq!(activated["skillArgs"], "-m \"feat: new feature\"");
    }

    #[tokio::test]
    async fn test_name_field_fallback() {
        let (callbacks, read_received, _) = scripted(read_ok(sample_skill()));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "name": "commit" }),
            scan(None),
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(read_received.lock().unwrap().clone().unwrap().key, "commit");
    }

    #[test]
    fn test_expand_all_placeholders() {
        let body = "raw=$ARGUMENTS zero=$0 one=$1 second=$ARGUMENTS[1] flag=$flag message=$message dir=${KIMI_SKILL_DIR} session=${KIMI_SESSION_ID}";
        let arg_names = vec!["flag".into(), "message".into()];
        let expanded = expand_skill_parameters(
            body,
            "-m \"fix login\"",
            "/tmp/skills/commit",
            "ses_1",
            &arg_names,
        );
        assert_eq!(
            expanded,
            "raw=-m \"fix login\" zero=-m one=fix login second=fix login flag=-m message=fix login dir=/tmp/skills/commit session=ses_1"
        );
    }

    #[test]
    fn test_unknown_placeholder_left_alone() {
        let body = "unknown=$missing actual=$0 missing=$1";
        let expanded = expand_skill_parameters(body, "hello", "/x", "s", &[]);
        assert_eq!(expanded, "unknown=$missing actual=hello missing=");
    }

    #[test]
    fn test_backslash_dollar_literal() {
        let body = r"raw=\$ARGUMENTS zero=\$0 indexed=\$ARGUMENTS[1] target=\$target";
        let arg_names = vec!["target".into()];
        let expanded = expand_skill_parameters(body, "src/app.ts careful", "/x", "", &arg_names);
        assert_eq!(
            expanded,
            r"raw=\src/app.ts careful zero=\src/app.ts indexed=\careful target=\src/app.ts"
        );
    }

    #[test]
    fn test_append_arguments_when_no_placeholder() {
        let body = "Review this file.";
        let expanded = expand_skill_parameters(body, "src/app.ts", "", "", &[]);
        assert_eq!(expanded, "Review this file.\n\nARGUMENTS: src/app.ts");
    }

    #[test]
    fn test_context_placeholders_still_appends_arguments() {
        let body = "Use ${KIMI_SKILL_DIR}/references/checklist.md.";
        let expanded = expand_skill_parameters(body, "src/app.ts", "/skills/review", "ses_1", &[]);
        assert_eq!(
            expanded,
            "Use /skills/review/references/checklist.md.\n\nARGUMENTS: src/app.ts"
        );
    }

    #[test]
    fn test_longer_variable_names_not_matched() {
        let body = "Leave $targeted alone.";
        let arg_names = vec!["target".into()];
        let expanded = expand_skill_parameters(body, "src/app.ts", "", "", &arg_names);
        assert_eq!(expanded, "Leave $targeted alone.\n\nARGUMENTS: src/app.ts");
    }

    #[test]
    fn test_space_separated_argument_names() {
        let wire = SkillWire {
            name: "review".into(),
            instructions: "Target: $target\nMode: $mode".into(),
            dir: None,
            path: None,
            source: None,
            disable_model_invocation: None,
            skill_type: None,
            arguments: Some(SkillArgumentsWire::String("target mode".into())),
            argument_names: None,
            plugin: None,
        };
        let resolved = ResolvedSkill::from_wire(wire);
        let expanded = expand_skill_parameters(
            &resolved.instructions,
            "src/app.ts careful",
            "",
            "",
            &resolved.argument_names,
        );
        assert_eq!(expanded, "Target: src/app.ts\nMode: careful");
    }

    #[test]
    fn test_numeric_argument_names_ignored() {
        let wire = SkillWire {
            name: "review".into(),
            instructions: "Zero: $0\nOne: $1".into(),
            dir: None,
            path: None,
            source: None,
            disable_model_invocation: None,
            skill_type: None,
            arguments: Some(SkillArgumentsWire::List(vec!["1".into()])),
            argument_names: None,
            plugin: None,
        };
        let resolved = ResolvedSkill::from_wire(wire);
        assert!(resolved.argument_names.is_empty());
        let expanded = expand_skill_parameters(
            &resolved.instructions,
            "first second",
            "",
            "",
            &resolved.argument_names,
        );
        assert_eq!(expanded, "Zero: first\nOne: second");
    }

    #[test]
    fn test_escapes_xml_tags_in_expanded_args() {
        let body = "target=$target raw=$ARGUMENTS";
        let arg_names = vec!["target".into()];
        let expanded = expand_skill_parameters(body, "<src/app.ts> & notes", "", "", &arg_names);
        assert_eq!(
            expanded,
            "target=&lt;src/app.ts&gt; raw=&lt;src/app.ts&gt; & notes"
        );
    }

    #[tokio::test]
    async fn test_plugin_instructions_prefixed() {
        let skill = serde_json::json!({
            "name": "brainstorm",
            "instructions": "Brainstorm body.",
            "plugin": {
                "id": "superpowers",
                "instructions": "Use AskUserQuestion for clarifying questions."
            }
        });
        let (callbacks, _, _) = scripted(read_ok(skill));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "brainstorm" }),
            scan(None),
        )
        .await;
        assert!(!result.is_error);
        let ContentBlock::Text { text, .. } = &result.delivery.unwrap().blocks[0] else {
            panic!("expected a text block");
        };
        assert!(text.contains("<plugin-instructions plugin=\"superpowers\">\nUse AskUserQuestion for clarifying questions.\n</plugin-instructions>\n\nBrainstorm body."));
    }

    #[tokio::test]
    async fn test_disable_model_invocation_rejected() {
        let skill = serde_json::json!({
            "name": "secret-skill",
            "instructions": "Do secret things",
            "disableModelInvocation": true
        });
        let (callbacks, _, _) = scripted(read_ok(skill));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "secret-skill" }),
            scan(None),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("model invocation is disabled"));
    }

    #[tokio::test]
    async fn test_non_inline_skill_type_rejected() {
        let skill = serde_json::json!({
            "name": "flow-skill",
            "instructions": "run workflow",
            "skillType": "flow"
        });
        let (callbacks, _, _) = scripted(read_ok(skill));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "flow-skill" }),
            scan(None),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("not an inline skill"));
    }

    #[tokio::test]
    async fn test_skill_interpolates_session_id_from_context() {
        let skill = serde_json::json!({
            "name": "session-test",
            "instructions": "session is ${KIMI_SESSION_ID}",
        });
        let (callbacks, _, _) = scripted(read_ok(skill));
        let result = execute_skill(
            &callbacks,
            Some("session-xyz-123"),
            &serde_json::json!({ "skill": "session-test" }),
            scan(None),
        )
        .await;
        assert!(!result.is_error);
        let ContentBlock::Text { text, .. } = &result.delivery.unwrap().blocks[0] else {
            panic!("expected a text block");
        };
        assert!(text.contains("session is session-xyz-123"));
    }

    #[tokio::test]
    async fn test_workspace_skill_loads_without_the_host_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".kimi-code").join("skills").join("demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: A workspace skill.\n---\n\nDo the demo thing.\n",
        )
        .unwrap();

        let (callbacks, read_received, events) =
            scripted(Err("host does not support state bridge".into()));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "demo" }),
            scan(Some(dir.path())),
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            result.content,
            "Skill \"demo\" loaded inline. Follow its instructions."
        );
        let ContentBlock::Text { text, .. } = &result.delivery.as_ref().unwrap().blocks[0] else {
            panic!("expected a text block");
        };
        assert!(text.contains("<skill-loaded name=\"demo\""));
        assert!(text.contains("Do the demo thing."));

        assert!(!text.contains("---\ndescription:"));

        assert!(read_received.lock().unwrap().is_none());
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e["type"] == "skill.activated")
        );

        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "no-such-skill" }),
            scan(Some(dir.path())),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("does not support the state bridge"));
    }

    #[tokio::test]
    async fn test_scanned_skill_honors_arguments_and_type_gates() {
        let dir = tempfile::tempdir().unwrap();

        let named_dir = dir.path().join(".agents").join("skills").join("review");
        std::fs::create_dir_all(&named_dir).unwrap();
        std::fs::write(
            named_dir.join("SKILL.md"),
            "---\nname: review\ndescription: Review a target.\narguments: target mode\n---\n\nReview $target in $mode.",
        )
        .unwrap();

        let (callbacks, _, _) = scripted(Err("host does not support state bridge".into()));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({
                "skill": "review",
                "args": "src/app.ts strict"
            }),
            scan(Some(dir.path())),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let ContentBlock::Text { text, .. } = &result.delivery.as_ref().unwrap().blocks[0] else {
            panic!("expected a text block");
        };
        assert!(text.contains("Review src/app.ts in strict."), "{text}");
        assert!(text.contains("source=\"project\""), "{text}");
        assert!(text.contains("dir=\""), "{text}");

        let flow_dir = dir.path().join(".agents").join("skills").join("flow-skill");
        std::fs::create_dir_all(&flow_dir).unwrap();
        std::fs::write(
            flow_dir.join("SKILL.md"),
            "---\nname: flow-skill\ndescription: A flow.\ntype: flow\n---\n\nRun the flow.",
        )
        .unwrap();
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "flow-skill" }),
            scan(Some(dir.path())),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("not an inline skill"));
    }

    #[tokio::test]
    async fn test_skill_scan_honors_the_merge_switch() {
        let dir = tempfile::tempdir().unwrap();

        let generic = dir.path().join(".agents").join("skills").join("generic");
        std::fs::create_dir_all(&generic).unwrap();
        std::fs::write(
            generic.join("SKILL.md"),
            "---\nname: generic\ndescription: From .agents\n---\n\nGeneric body.\n",
        )
        .unwrap();
        let brand = dir.path().join(".kimi-code").join("skills").join("brand");
        std::fs::create_dir_all(&brand).unwrap();
        std::fs::write(
            brand.join("SKILL.md"),
            "---\nname: brand\ndescription: From .kimi-code\n---\n\nBrand body.\n",
        )
        .unwrap();

        let (callbacks, read_received, _) =
            scripted(Err("host does not support state bridge".into()));

        let merged = SkillScan {
            root: Some(dir.path()),
            extra_dirs: &[],
            plugin_dirs: &[],
            merge_all_available_skills: true,
        };
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "brand" }),
            merged,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let ContentBlock::Text { text, .. } = &result.delivery.as_ref().unwrap().blocks[0] else {
            panic!("expected a text block");
        };
        assert!(text.contains("Brand body."), "{text}");
        assert!(read_received.lock().unwrap().is_none(), "the scan answered");

        let selected = SkillScan {
            root: Some(dir.path()),
            extra_dirs: &[],
            plugin_dirs: &[],
            merge_all_available_skills: false,
        };
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "brand" }),
            selected,
        )
        .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("does not support the state bridge"),
            "{}",
            result.content
        );
        assert!(
            read_received.lock().unwrap().is_some(),
            "a merge-off miss must fall back to the host"
        );

        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "generic" }),
            selected,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
    }

    #[tokio::test]
    async fn test_sub_skill_is_not_model_invocable() {
        let (callbacks, _, _) = scripted(Err("host does not support state bridge".into()));

        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "sub-skill.review" }),
            scan(None),
        )
        .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("model invocation is disabled"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn test_builtin_skills_load_embedded_bodies() {
        let (callbacks, read_received, _) =
            scripted(Err("host does not support state bridge".into()));

        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "update-config" }),
            scan(None),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let delivery = result.delivery.expect("delivery");
        let ContentBlock::Text { text, .. } = &delivery.blocks[0] else {
            panic!("expected a text block");
        };
        assert!(
            text.contains("<skill-loaded name=\"update-config\""),
            "{text}"
        );
        assert!(text.contains("source=\"builtin\""), "{text}");
        assert!(text.contains("dir=\"builtin://update-config\""), "{text}");

        assert!(read_received.lock().unwrap().is_none());

        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "custom-theme" }),
            scan(None),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("model invocation is disabled"));
    }

    #[tokio::test]
    async fn test_skill_name_is_escaped_in_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".kimi-code").join("skills").join("a-b");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: 'q\"x'\ndescription: d\n---\n\nBody.",
        )
        .unwrap();

        let (callbacks, _, _) = scripted(Err("x".into()));
        let result = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "q\"x" }),
            scan(Some(dir.path())),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let ContentBlock::Text { text, .. } = &result.delivery.as_ref().unwrap().blocks[0] else {
            panic!("expected a text block");
        };
        assert!(text.contains("name=\"q&quot;x\""), "{text}");
    }

    #[test]
    fn test_nested_skill_too_deep_error_matches_v2() {
        assert_eq!(MAX_SKILL_QUERY_DEPTH, 3);
        assert_eq!(
            nested_skill_too_deep_error(MAX_SKILL_QUERY_DEPTH, Some("commit")),
            "Nested skill invocation \"commit\" exceeded the maximum depth of 3 — refusing to recurse further."
        );

        assert_eq!(
            nested_skill_too_deep_error(MAX_SKILL_QUERY_DEPTH, None),
            "Nested skill invocation exceeded the maximum depth of 3 — refusing to recurse further."
        );
    }

    #[tokio::test]
    async fn test_skill_query_depth_limit() {
        let (callbacks, read_received, events) = scripted(read_ok(sample_skill()));

        for depth in 0..MAX_SKILL_QUERY_DEPTH {
            let result = execute_skill_with_depth(
                &callbacks,
                None,
                &serde_json::json!({ "skill": "commit" }),
                scan(None),
                depth,
            )
            .await;
            assert!(
                !result.is_error,
                "depth {depth} must load: {}",
                result.content
            );
            assert_eq!(
                result.content,
                "Skill \"commit\" loaded inline. Follow its instructions."
            );
        }

        let result = execute_skill_with_depth(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "commit" }),
            scan(None),
            MAX_SKILL_QUERY_DEPTH,
        )
        .await;
        assert!(result.is_error);
        assert_eq!(
            result.content,
            nested_skill_too_deep_error(MAX_SKILL_QUERY_DEPTH, Some("commit"))
        );
        assert_eq!(
            result.content,
            "Nested skill invocation \"commit\" exceeded the maximum depth of 3 — refusing to recurse further."
        );

        let activated = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "skill.activated")
            .count();
        assert_eq!(activated, MAX_SKILL_QUERY_DEPTH as usize);
        assert!(read_received.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn test_depth_refusal_precedes_resolution() {
        let (callbacks, read_received, events) = scripted(Err(
            "State read error: [-32002] unknown skill: commit".into(),
        ));
        let result = execute_skill_with_depth(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "no-such-skill" }),
            scan(None),
            MAX_SKILL_QUERY_DEPTH,
        )
        .await;
        assert!(result.is_error);
        assert_eq!(
            result.content,
            "Nested skill invocation \"no-such-skill\" exceeded the maximum depth of 3 — refusing to recurse further."
        );
        assert!(read_received.lock().unwrap().is_none());
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .all(|e| e["type"] != "skill.activated")
        );
    }

    #[tokio::test]
    async fn test_nested_invocation_records_the_nested_skill_trigger() {
        let (callbacks, _, events) = scripted(read_ok(sample_skill()));
        let result = execute_skill_with_depth(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "commit" }),
            scan(None),
            1,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let ContentBlock::Text { text, .. } = &result.delivery.as_ref().unwrap().blocks[0] else {
            panic!("expected a text block");
        };
        assert!(text.contains("trigger=\"nested-skill\""), "{text}");
        let events = events.lock().unwrap();
        let activated = events
            .iter()
            .find(|e| e["type"] == "skill.activated")
            .expect("skill.activated event");
        assert_eq!(activated["trigger"], "nested-skill");
    }

    #[tokio::test]
    async fn test_execute_skill_is_the_depth_zero_entry_point() {
        let (callbacks, _, _) = scripted(read_ok(sample_skill()));
        let direct = execute_skill(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "commit" }),
            scan(None),
        )
        .await;
        let at_zero = execute_skill_with_depth(
            &callbacks,
            None,
            &serde_json::json!({ "skill": "commit" }),
            scan(None),
            0,
        )
        .await;
        assert!(!direct.is_error && !at_zero.is_error);
        assert_eq!(direct.content, at_zero.content);
    }
}
