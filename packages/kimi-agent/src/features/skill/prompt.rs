pub use crate::prompt::skills_renderer::{
    generate_skills_section, generate_skills_section_with_extra,
    generate_skills_section_with_options, render_skills_markdown,
};

use serde_json::Map;
use serde_json::Value;

use crate::_base::utils::xml_escape::escape_xml;
use crate::human::agent::origin::prompt_display_text_from_content_parts;
use crate::human::llm::message::ContentPart;

use super::skill::SkillActivationInput;

pub fn prompt_metadata_text_from_parts(
    parts: &[ContentPart],
    client_metadata: Option<&[Map<String, Value>]>,
) -> Option<String> {
    let client_metadata = client_metadata.map(|entries| Value::Array(entries.to_vec()));
    crate::session::title::prompt_metadata_text_from_parts(
        &prompt_display_text_from_content_parts(parts),
        client_metadata.as_ref(),
    )
}

pub fn prompt_metadata_text_from_skill(input: &SkillActivationInput) -> Option<String> {
    let args = input.args.as_deref().map(trim_js).filter(|args| !args.is_empty());
    let text = match args {
        Some(args) => format!("/{} {args}", input.name),
        None => format!("/{}", input.name),
    };
    prompt_metadata_text_from_parts(
        &[ContentPart::Text {
            text,
            meta: None,
        }],
        input.client_metadata.as_deref(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillPromptTrigger {
    UserSlash,
    ModelTool,
    NestedSkill,
}

impl SkillPromptTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::UserSlash => "user-slash",
            Self::ModelTool => "model-tool",
            Self::NestedSkill => "nested-skill",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSkillPromptInput {
    pub skill_name: String,
    pub skill_args: String,
    pub skill_content: String,
    pub skill_source: Option<String>,
    pub skill_dir: Option<String>,
}

pub fn render_user_slash_skill_prompt(input: &RenderSkillPromptInput) -> String {
    format!(
        "User activated the skill \"{}\". Follow the loaded skill instructions.\n\n{}",
        escape_xml(&input.skill_name),
        render_skill_loaded_block(input, SkillPromptTrigger::UserSlash)
    )
}

pub fn render_model_tool_skill_prompt(
    input: &RenderSkillPromptInput,
    trigger: SkillPromptTrigger,
) -> String {
    format!(
        "Skill tool loaded instructions for this request. Follow them.\n\n{}",
        render_skill_loaded_block(input, trigger)
    )
}

pub fn render_skill_loaded_block(
    input: &RenderSkillPromptInput,
    trigger: SkillPromptTrigger,
) -> String {
    let mut attributes = String::new();
    for (name, value) in [
        ("name", Some(input.skill_name.as_str())),
        ("trigger", Some(trigger.as_str())),
        ("source", input.skill_source.as_deref()),
        ("dir", input.skill_dir.as_deref()),
        ("args", Some(input.skill_args.as_str())),
    ] {
        if let Some(value) = value {
            attributes.push_str(&format!(" {name}=\"{}\"", escape_xml(value)));
        }
    }
    format!(
        "<skill-loaded{attributes}>\n{}\n</skill-loaded>",
        input.skill_content
    )
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
