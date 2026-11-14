use indexmap::IndexMap;
use std::error::Error;
use std::fmt;

use regex::Regex;

use crate::_base::utils::xml_escape::{escape_xml_attr, escape_xml_tags};

use super::parser::skill_argument_names;
use super::types::{
    SkillCatalog, SkillDefinition, SkillPluginContext, SkillSource, SkippedSkill,
    is_inline_skill_type, normalize_skill_name, skill_source_rank,
};

pub use crate::skills::*;

const LISTING_DESC_MAX: usize = 250;

#[derive(Debug, Clone)]
pub struct SkillNotFoundError {
    pub skill_name: String,
}

impl fmt::Display for SkillNotFoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Skill \"{}\" is not registered", self.skill_name)
    }
}

impl Error for SkillNotFoundError {}

#[derive(Debug, Default)]
pub struct InMemorySkillCatalog {
    by_name: IndexMap<String, SkillDefinition>,
    variants_by_name: IndexMap<String, Vec<SkillDefinition>>,
    by_plugin_and_name: IndexMap<String, SkillDefinition>,
    roots: Vec<String>,
    skipped: Vec<SkippedSkill>,
}

impl InMemorySkillCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_builtin_skill(&mut self, mut skill: SkillDefinition) {
        skill.source = SkillSource::Builtin;
        self.register(skill, false);
    }

    pub fn register(&mut self, skill: SkillDefinition, replace: bool) {
        if let Some(plugin) = skill.plugin.clone() {
            self.register_plugin_skill(skill, &plugin, replace);
            return;
        }
        let key = normalize_skill_name(&skill.name);
        let should_replace = replace
            || self
                .by_name
                .get(&key)
                .is_none_or(|existing| skill_source_rank(skill.source) > skill_source_rank(existing.source));
        if should_replace {
            self.by_name.insert(key.clone(), skill.clone());
            self.variants_by_name.insert(key, vec![skill]);
        }
    }

    pub fn register_variants(&mut self, skills: &[SkillDefinition], replace: bool) {
        let mut grouped: IndexMap<String, Vec<SkillDefinition>> = IndexMap::new();
        for input in skills {
            let (key, skill) = catalog_variant(input);
            let variants = grouped.entry(key).or_default();
            if !variants.iter().any(|variant| variant.path == skill.path) {
                variants.push(skill);
            }
        }
        for (key, variants) in grouped {
            if replace || !self.variants_by_name.contains_key(&key) {
                self.variants_by_name.insert(key, variants);
            }
        }
    }

    pub fn record_skipped(&mut self, skills: &[SkippedSkill]) {
        self.skipped.extend_from_slice(skills);
    }

    pub fn add_roots(&mut self, roots: &[String]) {
        for root in roots {
            if !self.roots.contains(root) {
                self.roots.push(root.clone());
            }
        }
    }

    pub fn get_kimi_skills_description(&self) -> String {
        let rendered = render_grouped_skills(&self.list_skills(), format_full_skill);
        if rendered.is_empty() { "No skills".to_string() } else { rendered }
    }

    fn register_plugin_skill(
        &mut self,
        skill: SkillDefinition,
        plugin: &SkillPluginContext,
        replace: bool,
    ) {
        let prefix = format!("{}:", plugin.id);
        let bare_name = skill.name.strip_prefix(&prefix).unwrap_or(&skill.name).to_string();
        let plugin_key = plugin_skill_key(&plugin.id, &bare_name);
        if replace || !self.by_plugin_and_name.contains_key(&plugin_key) {
            let mut bare = skill.clone();
            bare.name = bare_name.clone();
            self.by_plugin_and_name.insert(plugin_key, bare);
        }
        let qualified_name = format!("{prefix}{bare_name}");
        let qualified_key = normalize_skill_name(&qualified_name);
        if replace || !self.by_name.contains_key(&qualified_key) {
            let mut qualified = skill;
            qualified.name = qualified_name;
            self.by_name.insert(qualified_key.clone(), qualified.clone());
            self.variants_by_name.insert(qualified_key, vec![qualified]);
        }
    }
}

impl SkillCatalog for InMemorySkillCatalog {
    fn get_skill(&self, name: &str, path: Option<&str>) -> Option<SkillDefinition> {
        let key = normalize_skill_name(name);
        match path {
            None => self.by_name.get(&key).cloned(),
            Some(path) => self
                .variants_by_name
                .get(&key)?
                .iter()
                .find(|skill| skill.path == path)
                .cloned(),
        }
    }

    fn get_plugin_skill(&self, plugin_id: &str, name: &str) -> Option<SkillDefinition> {
        self.by_plugin_and_name.get(&plugin_skill_key(plugin_id, name)).cloned()
    }

    fn render_skill_prompt(
        &self,
        skill: &SkillDefinition,
        raw_args: &str,
        session_id: Option<&str>,
    ) -> String {
        let content = expand_skill_parameters(skill, raw_args, session_id);
        let Some(plugin) = &skill.plugin else {
            return content;
        };
        let Some(instructions) = &plugin.instructions else {
            return content;
        };
        if trim_js(instructions).is_empty() {
            return content;
        }
        format!(
            "<plugin-instructions plugin=\"{}\">\n{}\n</plugin-instructions>\n\n{}",
            escape_xml_attr(&plugin.id),
            instructions,
            content
        )
    }

    fn list_default_skills(&self) -> Vec<SkillDefinition> {
        let mut skills: Vec<_> = self.by_name.values().cloned().collect();
        skills.sort_by(|a, b| locale_compare(&a.name, &b.name));
        skills
    }

    fn list_skills(&self) -> Vec<SkillDefinition> {
        let mut skills: Vec<_> = self.variants_by_name.values().flatten().cloned().collect();
        skills.sort_by(|a, b| locale_compare(&a.name, &b.name));
        skills
    }

    fn list_invocable_skills(&self) -> Vec<SkillDefinition> {
        self.list_skills()
            .into_iter()
            .filter(|skill| {
                skill.disable_model_invocation() != Some(true)
                    && is_inline_skill_type(skill.skill_type())
            })
            .collect()
    }

    fn get_skill_roots(&self) -> Vec<String> {
        self.roots.clone()
    }

    fn get_skipped_by_policy(&self) -> Vec<SkippedSkill> {
        self.skipped.clone()
    }

    fn get_model_skill_listing(&self) -> String {
        let mut lines = vec!["DISREGARD any earlier skill listings. Current available skills:".to_string()];
        let invocable: Vec<_> = self
            .list_invocable_skills()
            .into_iter()
            .filter(|skill| skill.is_sub_skill() != Some(true))
            .collect();
        let listing = render_grouped_skills(&invocable, format_model_skill);
        if !listing.is_empty() {
            lines.push(listing);
            if !self.roots.is_empty() {
                lines.push("### Skill roots".to_string());
                lines.extend(self.roots.iter().map(|root| format!("- `{root}`")));
            }
        }
        if lines.len() == 1 { String::new() } else { lines.join("\n") }
    }
}

fn catalog_variant(skill: &SkillDefinition) -> (String, SkillDefinition) {
    let Some(plugin) = &skill.plugin else {
        return (normalize_skill_name(&skill.name), skill.clone());
    };
    let prefix = format!("{}:", plugin.id);
    let bare_name = skill.name.strip_prefix(&prefix).unwrap_or(&skill.name);
    let qualified_name = format!("{prefix}{bare_name}");
    let mut qualified = skill.clone();
    qualified.name = qualified_name.clone();
    (normalize_skill_name(&qualified_name), qualified)
}

fn expand_skill_parameters(skill: &SkillDefinition, raw_args: &str, session_id: Option<&str>) -> String {
    let tokens = tokenize_args(raw_args);
    let body = &skill.content;
    let mut content = body.clone();
    for (index, name) in skill_argument_names(&skill.metadata).into_iter().enumerate() {
        let pattern = format!(r"\${}(?![\[\w])", escape_reg_exp(&name));
        content = js_replace_all_regex(
            &content,
            &pattern,
            &escape_xml_tags(tokens.get(index).map_or("", String::as_str)),
        );
    }
    content = Regex::new(r"\$ARGUMENTS\[([0-9]+)\]")
        .unwrap()
        .replace_all(&content, |captures: &regex::Captures<'_>| {
            let index = captures[1].parse::<usize>().unwrap_or(usize::MAX);
            escape_xml_tags(tokens.get(index).map_or("", String::as_str))
        })
        .into_owned();
    content = Regex::new(r"\$([0-9]+)(?![A-Za-z0-9_])")
        .unwrap()
        .replace_all(&content, |captures: &regex::Captures<'_>| {
            let index = captures[1].parse::<usize>().unwrap_or(usize::MAX);
            escape_xml_tags(tokens.get(index).map_or("", String::as_str))
        })
        .into_owned();
    content = js_replace_all_literal(&content, "$ARGUMENTS", &escape_xml_tags(raw_args));
    let has_argument_placeholder = content != *body;
    content = js_replace_all_literal(&content, "${KIMI_SKILL_DIR}", &skill.dir);
    content = js_replace_all_literal(&content, "${KIMI_SESSION_ID}", session_id.unwrap_or(""));
    if !has_argument_placeholder && !raw_args.is_empty() {
        format!("{content}\n\nARGUMENTS: {}", escape_xml_tags(raw_args))
    } else {
        content
    }
}

fn plugin_skill_key(plugin_id: &str, skill_name: &str) -> String {
    format!("{plugin_id}\0{}", normalize_skill_name(skill_name))
}

fn render_grouped_skills(
    skills: &[SkillDefinition],
    format: fn(&SkillDefinition) -> Vec<String>,
) -> String {
    let mut lines = Vec::new();
    for (source, label) in [
        (SkillSource::Project, "Project"),
        (SkillSource::User, "User"),
        (SkillSource::Extra, "Extra"),
        (SkillSource::Builtin, "Built-in"),
    ] {
        let group: Vec<_> = skills.iter().filter(|skill| skill.source == source).collect();
        if group.is_empty() {
            continue;
        }
        lines.push(format!("### {label}"));
        for skill in group {
            lines.extend(format(skill));
        }
    }
    lines.join("\n")
}

fn format_full_skill(skill: &SkillDefinition) -> Vec<String> {
    vec![
        format!("- {}", skill.name),
        format!("  - Path: {}", skill.path),
        format!("  - Description: {}", skill.description),
    ]
}

fn format_model_skill(skill: &SkillDefinition) -> Vec<String> {
    let mut lines = vec![format!("- {}: {}", skill.name, truncate(&skill.description, LISTING_DESC_MAX))];
    if let Some(when_to_use) = skill.metadata.get("whenToUse").and_then(serde_json::Value::as_str)
        && !when_to_use.is_empty()
    {
        lines.push(format!("  When to use: {when_to_use}"));
    }
    if let Some(workspace_dir) = &skill.workspace_dir {
        lines.push(format!("  Workspace: {workspace_dir}"));
    }
    lines
}

fn truncate(value: &str, max: usize) -> String {
    let script = format!(
        "(function (value, max) {{ if (value.length <= max) return value; let length = 0; let result = ''; for (const {{ segment }} of new Intl.Segmenter(undefined, {{ granularity: 'grapheme' }}).segment(value)) {{ if (length + segment.length > max - 3) break; result += segment; length += segment.length; }} return result + '...'; }})({}, {max})",
        js_string(value)
    );
    js_eval_string(script).unwrap_or_else(|_| {
        if value.encode_utf16().count() <= max {
            return value.to_string();
        }
        let mut length = 0;
        let mut result = String::new();
        for character in value.chars() {
            let size = character.len_utf16();
            if length + size > max - 3 {
                break;
            }
            result.push(character);
            length += size;
        }
        format!("{result}...")
    })
}

fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    let script = format!(
        "String((function (left, right) {{ return left.localeCompare(right); }})({}, {}))",
        js_string(left),
        js_string(right)
    );
    js_eval_string(script)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .map(|value| value.cmp(&0))
        .unwrap_or_else(|| left.encode_utf16().cmp(right.encode_utf16()))
}

fn escape_reg_exp(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        if matches!(character, '\\' | '^' | '$' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn js_replace_all_literal(input: &str, search: &str, replacement: &str) -> String {
    let script = format!(
        "(function (input, search, replacement) {{ return input.replaceAll(search, replacement); }})({}, {}, {})",
        js_string(input),
        js_string(search),
        js_string(replacement)
    );
    js_eval_string(script).unwrap_or_else(|_| input.replace(search, replacement))
}

fn js_replace_all_regex(input: &str, pattern: &str, replacement: &str) -> String {
    let script = format!(
        "(function (input, pattern, replacement) {{ return input.replaceAll(new RegExp(pattern, 'g'), replacement); }})({}, {}, {})",
        js_string(input),
        js_string(pattern),
        js_string(replacement)
    );
    js_eval_string(script).unwrap_or_else(|_| {
        Regex::new(pattern)
            .map(|regex| regex.replace_all(input, replacement).into_owned())
            .unwrap_or_else(|_| input.to_string())
    })
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

fn js_eval_string(script: String) -> Result<String, String> {
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| ctx.eval::<String, _>(script))
            })
        })
        .map_err(|error| error.to_string())
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn tokenize_args(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut has_content = false;
    for character in raw.chars() {
        if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                current.push(character);
                has_content = true;
            }
            continue;
        }
        if character == '\"' || character == '\'' {
            quote = Some(character);
            has_content = true;
            continue;
        }
        if is_js_whitespace(character) {
            if has_content {
                out.push(std::mem::take(&mut current));
                has_content = false;
            }
            continue;
        }
        current.push(character);
        has_content = true;
    }
    if has_content {
        out.push(current);
    }
    out
}
