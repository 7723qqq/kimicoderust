use serde_json::{Map, Value};

use crate::_base::utils::xml_escape::escape_xml_attr;

pub fn render_notification_xml(data: &Map<String, Value>) -> String {
    let id = string_attr(data.get("id"), "unknown");
    let category = string_attr(data.get("category"), "unknown");
    let type_ = string_attr(data.get("type"), "unknown");
    let source_kind = string_attr(data.get("source_kind"), "unknown");
    let source_id = string_attr(data.get("source_id"), "unknown");
    let agent_id = optional_string_attr(data.get("agent_id"));
    let title = data.get("title").and_then(Value::as_str).unwrap_or("");
    let severity = data.get("severity").and_then(Value::as_str).unwrap_or("");
    let body = data.get("body").and_then(Value::as_str).unwrap_or("");
    let children = data
        .get("children")
        .filter(|value| !value.is_null())
        .or_else(|| data.get("extraBlocks"))
        .map(child_blocks)
        .unwrap_or_default();
    let agent_id_attr = agent_id
        .map(|agent_id| format!(" agent_id=\"{agent_id}\""))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "<notification id=\"{id}\" category=\"{category}\" type=\"{type_}\" source_kind=\"{source_kind}\" source_id=\"{source_id}\"{agent_id_attr}>"
    )];
    if !title.is_empty() {
        lines.push(format!("Title: {title}"));
    }
    if !severity.is_empty() {
        lines.push(format!("Severity: {severity}"));
    }
    if !body.is_empty() {
        lines.push(body.to_string());
    }
    lines.extend(children);
    lines.push("</notification>".to_string());
    lines.join("\n")
}

fn string_attr(value: Option<&Value>, fallback: &str) -> String {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(escape_xml_attr)
        .unwrap_or_else(|| fallback.to_string())
}

fn optional_string_attr(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(escape_xml_attr)
}

fn child_blocks(value: &Value) -> Vec<String> {
    if let Some(value) = value.as_str().filter(|value| !value.is_empty()) {
        return vec![value.to_string()];
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}
