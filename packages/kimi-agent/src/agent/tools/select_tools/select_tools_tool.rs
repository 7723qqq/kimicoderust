use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::select_tools::select_tools_schema;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::agent::tool_select::tool_select::{IAgentToolSelectService, SELECT_TOOLS_TOOL_NAME};
use crate::wire::record::WireAsyncError;

pub const DESCRIPTION: &str = "Load tools by exact name, or search for them by capability when the name is unknown. The loadable names are listed in the <tools_added>/<tools_removed> or <tools_relevant> announcements in the system context — use the latest announcement. If you have not seen a loadable-tools announcement in this session, no tools are currently loadable. Pass names to load them, or pass query to search all loadable tools; a search returns candidates only, so call select_tools again with exact names to load the ones you need. The full definitions become available immediately, so you can call them directly in your next tool call. Tools you already have available are called directly, never passed to select_tools.";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SelectToolsInput {
    #[serde(default)]
    pub names: Option<Vec<String>>,
    #[serde(default)]
    pub query: Option<String>,
}

pub struct SelectToolsTool {
    tool_select: Arc<dyn IAgentToolSelectService>,
}

impl SelectToolsTool {
    pub fn new(tool_select: Arc<dyn IAgentToolSelectService>) -> Self {
        Self { tool_select }
    }

    pub fn name(&self) -> &'static str {
        SELECT_TOOLS_TOOL_NAME
    }

    pub fn description(&self) -> &'static str {
        DESCRIPTION
    }
    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn parameters(&self) -> Map<String, Value> {
        crate::tool::input_schema::to_input_json_schema(select_tools_schema())
            .as_object()
            .cloned()
            .expect("select_tools schema is an object")
    }

    pub fn resolve_execution(&self, args: SelectToolsInput) -> ToolExecution {
        let Some(names) = args.names else {
            let query = args.query.unwrap_or_default();
            let tool_select = self.tool_select.clone();
            let execute_query = query.clone();
            return ToolExecution::Runnable(RunnableToolExecution {
                accesses: None,
                display: None,
                description: Some(format!("Searching tools for \"{query}\"")),
                stop_batch_after_this: None,
                approval_rule: SELECT_TOOLS_TOOL_NAME.to_string(),
                matches_rule: None,
                execute: Arc::new(move |_| {
                    let tool_select = tool_select.clone();
                    let query = execute_query.clone();
                    Box::pin(async move { Ok(search_tools(tool_select, query)) })
                }),
            });
        };
        let tool_select = self.tool_select.clone();
        let execute_names = names.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(format!("Loading {}", names.join(", "))),
            stop_batch_after_this: None,
            approval_rule: SELECT_TOOLS_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_| {
                let tool_select = tool_select.clone();
                let names = execute_names.clone();
                Box::pin(async move { Ok(load_tools(tool_select, names)) })
            }),
        })
    }
}

impl ExecutableTool for SelectToolsTool {
    fn name(&self) -> &str {
        SelectToolsTool::name(self)
    }

    fn description(&self) -> &str {
        SelectToolsTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        SelectToolsTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let args = match serde_json::from_value::<SelectToolsInput>(input) {
            Ok(args) => args,
            Err(error) => {
                return Box::pin(async move { Err(Box::new(error) as WireAsyncError) });
            }
        };
        let execution = SelectToolsTool::resolve_execution(self, args);
        Box::pin(async move { Ok(execution) })
    }
}

fn result(content: String, is_error: bool) -> ExecutableToolResult {
    ExecutableToolResult {
        output: content.into(),
        is_error: is_error.then_some(true),
        ..Default::default()
    }
}

fn search_tools(
    tool_select: Arc<dyn IAgentToolSelectService>,
    query: String,
) -> ExecutableToolResult {
    if !tool_select.enabled(None) {
        return result(
            "select_tools is not available for the current model.".to_string(),
            true,
        );
    }
    if query.is_empty() {
        return result(
            "Provide names to load or a query to search.".to_string(),
            true,
        );
    }
    let hits = tool_select.search(&query, Some(10));
    if hits.is_empty() {
        return result(
            format!(
                "No loadable tools matched \"{query}\". Try a different capability description or use a tool you already have."
            ),
            false,
        );
    }
    let mut lines = vec![format!("Search results for \"{query}\":")];
    for hit in hits {
        let description = utf16_prefix(&collapse_js_whitespace(&hit.description), 120);
        if description.is_empty() {
            lines.push(hit.name);
        } else {
            lines.push(format!("{} — {description}", hit.name));
        }
    }
    lines.push(
        "These are candidates. Tools you already have can be called directly; call select_tools with exact names to load the others.".to_string(),
    );
    result(lines.join("\n"), false)
}

fn load_tools(
    tool_select: Arc<dyn IAgentToolSelectService>,
    names: Vec<String>,
) -> ExecutableToolResult {
    if !tool_select.enabled(None) {
        return result(
            "select_tools is not available for the current model.".to_string(),
            true,
        );
    }
    let loaded = tool_select.load(&names);
    let mut lines = Vec::new();
    if !loaded.to_load.is_empty() {
        lines.push(format!("Loaded: {}", loaded.to_load.join(", ")));
    }
    if !loaded.already_available.is_empty() {
        lines.push(format!(
            "Already available: {}",
            loaded.already_available.join(", ")
        ));
    }
    for name in &loaded.already_callable {
        lines.push(format!(
            "\"{name}\" is already available — call it directly; select_tools is only for names in the <tools_added> announcements."
        ));
    }
    for name in &loaded.stale_loaded {
        lines.push(format!(
            "\"{name}\" was previously selected or loaded but is no longer registered. select_tools cannot restore it. Do not select it again unless a new <tools_added> announcement lists it."
        ));
    }
    for name in &loaded.policy_denied {
        lines.push(format!(
            "\"{name}\" is disabled by the current tool policy. select_tools cannot enable it. Do not retry unless the policy changes; use an available tool instead."
        ));
    }
    for name in &loaded.unknown {
        let reason = format!(
            "Unknown tool: {name}. This name is not in the current loadable tool list. Call select_tools with a query to search by capability, then load an exact name; or use an available tool. "
        );
        let candidates = loaded.suggestions.get(name);
        if let Some(candidates) = candidates.filter(|candidates| !candidates.is_empty()) {
            lines.push(format!(
                "{reason}Did you mean: {}? Use an exact listed name.",
                candidates.join(", ")
            ));
        } else {
            let available = if loaded.loadable.is_empty() {
                "No additional tools can be loaded right now; use the tools you already have.".to_string()
            } else if loaded.loadable.len() <= 5 {
                format!("Loadable tools: {}.", loaded.loadable.join(", "))
            } else {
                "Select only exact names from the current announcements.".to_string()
            };
            lines.push(format!(
                "{reason}{available} Do not guess aliases; use an available tool or tell the user what capability is missing."
            ));
        }
    }
    let mut escalated = loaded
        .stale_loaded
        .iter()
        .chain(&loaded.policy_denied)
        .chain(&loaded.unknown)
        .filter(|name| loaded.escalated.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    for name in loaded.escalated.keys() {
        if !escalated.contains(name) {
            escalated.push(name.clone());
        }
    }
    let mut seen = std::collections::HashSet::new();
    escalated.retain(|name| seen.insert(name.clone()));
    escalated.sort_by(|left, right| {
        match (js_array_index(left), js_array_index(right)) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
    for name in escalated {
        let attempts = loaded.escalated.get(&name).expect("escalated entry");
        lines.push(format!(
            "\"{name}\" has already failed selection {attempts} times. Stop retrying this name until its availability changes."
        ));
    }
    if loaded.consecutive_failures >= 3 {
        lines.push(format!(
            "{} consecutive select_tools calls failed without making any tool available. Stop guessing names or retrying unavailable tools. Use only exact names from the current announcements, use tools already available, or tell the user what capability is missing.",
            loaded.consecutive_failures
        ));
    }
    let is_error = loaded.to_load.is_empty()
        && loaded.already_available.is_empty()
        && loaded.already_callable.is_empty();
    result(lines.join("\n"), is_error)
}

fn js_array_index(text: &str) -> Option<u32> {
    let index = text.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == text).then_some(index)
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

fn collapse_js_whitespace(text: &str) -> String {
    let mut output = String::new();
    let mut whitespace = false;
    for character in text.chars() {
        if is_js_whitespace(character) {
            whitespace = !output.is_empty();
            continue;
        }
        if whitespace {
            output.push(' ');
        }
        whitespace = false;
        output.push(character);
    }
    output
}

fn utf16_prefix(text: &str, max_units: usize) -> String {
    let mut units = 0usize;
    let mut end = 0usize;
    for (index, character) in text.char_indices() {
        let next = units + character.len_utf16();
        if next > max_units {
            break;
        }
        units = next;
        end = index + character.len_utf8();
    }
    text[..end].to_string()
}
