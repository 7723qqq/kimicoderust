use serde_json::{Map, Value, json};

pub const TOOL_NAME_ALIASES: &[(&str, &str)] = &[("Bash", "bash"), ("Read", "read")];

pub const FREE_TIER_REQUIRED_TOOLS: &[&str] = &["bash", "read"];

const PLACEHOLDER_DESCRIPTION: &str =
    "Shape placeholder required by the gateway. Do not call this tool.";

pub fn is_opencode_endpoint(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url.trim()) else {
        return false;
    };
    url.host_str() == Some("opencode.ai") && url.path().starts_with("/zen/")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaceholderShape {
    ChatCompletions,

    Responses,

    Anthropic,

    Google,
}

fn placeholder_shape(protocol: &str) -> PlaceholderShape {
    match protocol {
        "anthropic" => PlaceholderShape::Anthropic,
        "openai_responses" | "openai-responses" => PlaceholderShape::Responses,
        "google" | "google-genai" | "gemini" => PlaceholderShape::Google,
        _ => PlaceholderShape::ChatCompletions,
    }
}

pub fn apply(body: &mut Value, protocol: &str) {
    rewrite_tool_names(body);
    enforce_free_tier_shape(body, placeholder_shape(protocol));
}

fn rewrite_tool_names(body: &mut Value) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        if let Some(function) = tool.get_mut("function") {
            rewrite_field(function, "name", TOOL_NAME_ALIASES);
        }
        rewrite_field(tool, "name", TOOL_NAME_ALIASES);
        if let Some(declarations) = tool
            .get_mut("functionDeclarations")
            .and_then(Value::as_array_mut)
        {
            for declaration in declarations {
                rewrite_field(declaration, "name", TOOL_NAME_ALIASES);
            }
        }
    }
}

fn rewrite_field(container: &mut Value, field: &str, aliases: &[(&str, &str)]) {
    let Some(current) = container.get(field).and_then(Value::as_str) else {
        return;
    };
    let Some((_, alias)) = aliases.iter().find(|(from, _)| *from == current) else {
        return;
    };
    if let Some(slot) = container.get_mut(field) {
        *slot = Value::String((*alias).to_owned());
    }
}

fn enforce_free_tier_shape(body: &mut Value, shape: PlaceholderShape) {
    let Some(object) = body.as_object_mut() else {
        return;
    };

    if object.get("stream").and_then(Value::as_bool) != Some(true) {
        object.insert("stream".to_owned(), Value::Bool(true));
    }

    let missing: Vec<&str> = FREE_TIER_REQUIRED_TOOLS
        .iter()
        .copied()
        .filter(|required| !declares_tool(object, required))
        .collect();
    if missing.is_empty() {
        return;
    }

    let entry = object
        .entry("tools".to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(array) = entry.as_array_mut() else {
        return;
    };

    if shape == PlaceholderShape::Google {
        let existing = array.iter_mut().find_map(|tool| {
            tool.get_mut("functionDeclarations")
                .and_then(Value::as_array_mut)
        });
        match existing {
            Some(declarations) => {
                declarations.extend(missing.iter().map(|name| declaration(name)));
            }
            None => array.push(json!({
                "functionDeclarations": missing
                    .iter()
                    .map(|name| declaration(name))
                    .collect::<Vec<Value>>(),
            })),
        }
        return;
    }

    for name in missing {
        array.push(placeholder_tool(name, shape));
    }
}

fn declares_tool(object: &Map<String, Value>, name: &str) -> bool {
    let Some(tools) = object.get("tools").and_then(Value::as_array) else {
        return false;
    };
    tools.iter().any(|tool| {
        let in_function = tool
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str);
        let at_top_level = tool.get("name").and_then(Value::as_str);
        let in_declarations = tool
            .get("functionDeclarations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|d| d.get("name").and_then(Value::as_str))
            .any(|declared| declared == name);
        in_function == Some(name) || at_top_level == Some(name) || in_declarations
    })
}

fn empty_parameters() -> Value {
    json!({ "type": "object", "properties": {} })
}

fn placeholder_tool(name: &str, shape: PlaceholderShape) -> Value {
    match shape {
        PlaceholderShape::ChatCompletions => json!({
            "type": "function",
            "function": {
                "name": name,
                "description": PLACEHOLDER_DESCRIPTION,
                "parameters": empty_parameters(),
            },
        }),
        PlaceholderShape::Responses => json!({
            "type": "function",
            "name": name,
            "description": PLACEHOLDER_DESCRIPTION,
            "parameters": empty_parameters(),
        }),
        PlaceholderShape::Anthropic => json!({
            "name": name,
            "description": PLACEHOLDER_DESCRIPTION,
            "input_schema": empty_parameters(),
        }),

        PlaceholderShape::Google => json!({ "functionDeclarations": [declaration(name)] }),
    }
}

fn declaration(name: &str) -> Value {
    json!({
        "name": name,
        "description": PLACEHOLDER_DESCRIPTION,
        "parameters": empty_parameters(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Value {
        json!({
            "type": "function",
            "function": { "name": name, "description": "d", "parameters": { "type": "object" } }
        })
    }

    fn names(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| {
                t["function"]["name"]
                    .as_str()
                    .or_else(|| t["name"].as_str())
                    .map(str::to_owned)
            })
            .collect()
    }

    fn declared_names(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["functionDeclarations"].as_array())
            .flatten()
            .filter_map(|d| d["name"].as_str().map(str::to_owned))
            .collect()
    }

    fn is_placeholder(body: &Value, name: &str) -> bool {
        let entry = body["tools"].as_array().unwrap().iter().find(|t| {
            t["function"]["name"].as_str() == Some(name) || t["name"].as_str() == Some(name)
        });
        entry.is_some_and(|t| {
            t["function"]["description"]
                .as_str()
                .or_else(|| t["description"].as_str())
                .is_some_and(|d| d.contains("Do not call"))
        })
    }

    #[test]
    fn rewrites_only_the_aliased_names() {
        let mut body = json!({
            "model": "mimo-v2.6-flash-free",
            "tools": [
                tool("Bash"),
                tool("Read"),
                tool("Grep"),
                tool("Glob"),
                tool("Write"),
                tool("Edit"),
                tool("FetchURL"),
            ],
        });
        apply(&mut body, "openai");
        assert_eq!(
            names(&body),
            vec!["bash", "read", "Grep", "Glob", "Write", "Edit", "FetchURL"]
        );
    }

    #[test]
    fn rewrites_the_anthropic_and_responses_shapes() {
        let mut body = json!({
            "tools": [
                { "name": "Bash", "description": "d", "input_schema": { "type": "object" } },
                { "name": "Read", "description": "d", "input_schema": { "type": "object" } },
                { "name": "Grep", "description": "d", "input_schema": { "type": "object" } },
            ],
        });
        apply(&mut body, "openai");
        let got: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(got, vec!["bash", "read", "Grep"]);
    }

    #[test]
    fn rewrites_the_google_shape() {
        let mut body = json!({
            "tools": [{
                "functionDeclarations": [
                    { "name": "Bash", "description": "d" },
                    { "name": "Read", "description": "d" },
                    { "name": "Glob", "description": "d" },
                ]
            }],
        });
        apply(&mut body, "openai");
        let got: Vec<&str> = body["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(got, vec!["bash", "read", "Glob"]);
    }

    #[test]
    fn leaves_the_reasoning_effort_alone() {
        for effort in ["low", "medium", "high", "xhigh", "max", "minimal", "none"] {
            let mut body = json!({ "reasoning_effort": effort });
            apply(&mut body, "openai");
            assert_eq!(body["reasoning_effort"], effort, "{effort} must survive");
        }
    }

    #[test]
    fn forces_streaming_on() {
        let mut body = json!({ "model": "m", "messages": [] });
        apply(&mut body, "openai");
        assert_eq!(body["stream"], Value::Bool(true));
    }

    #[test]
    fn injects_the_shape_placeholders_when_tools_are_absent() {
        let mut body = json!({ "model": "m", "messages": [], "stream": true });
        apply(&mut body, "openai");
        let got = names(&body);
        assert!(
            got.contains(&"bash".to_owned()),
            "bash placeholder missing: {got:?}"
        );
        assert!(
            got.contains(&"read".to_owned()),
            "read placeholder missing: {got:?}"
        );

        let bash = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["function"]["name"] == "bash")
            .unwrap();
        assert!(
            bash["function"]["description"]
                .as_str()
                .unwrap()
                .contains("Do not call")
        );
    }

    #[test]
    fn appends_only_the_missing_placeholder() {
        let mut body = json!({ "stream": true, "tools": [tool("bash"), tool("Grep")] });
        apply(&mut body, "openai");
        assert_eq!(names(&body), vec!["bash", "Grep", "read"]);
    }

    #[test]
    fn a_full_toolset_is_left_alone() {
        let mut body = json!({ "stream": true, "tools": [tool("Bash"), tool("Read")] });
        apply(&mut body, "openai");
        assert_eq!(names(&body), vec!["bash", "read"]);
        assert!(
            !is_placeholder(&body, "bash"),
            "a declared tool must not be replaced by a placeholder: {body}"
        );
        assert!(!is_placeholder(&body, "read"));
    }

    #[test]
    fn a_restricted_tool_table_still_gets_the_gate_pair() {
        let mut body = json!({ "stream": true, "tools": [tool("Read"), tool("Grep")] });
        apply(&mut body, "openai");
        assert_eq!(names(&body), vec!["read", "Grep", "bash"]);
        assert!(is_placeholder(&body, "bash"));
        assert!(!is_placeholder(&body, "read"), "the real Read is untouched");
    }

    #[test]
    fn tolerates_malformed_tool_entries() {
        let mut body = json!({
            "stream": true,
            "tools": [
                { "type": "function" },
                { "function": {} },
                { "function": { "name": 7 } },
                tool("Bash"),
            ],
        });
        apply(&mut body, "openai");
        assert_eq!(body["tools"][3]["function"]["name"], "bash");

        assert!(names(&body).contains(&"read".to_owned()));
    }

    #[test]
    fn a_non_array_tools_value_is_left_to_the_gate() {
        let mut body = json!({ "stream": true, "tools": "nonsense" });
        apply(&mut body, "openai");
        assert_eq!(body["tools"], json!("nonsense"));
    }

    #[test]
    fn the_placeholder_shape_follows_the_protocol() {
        let mut responses = json!({ "stream": true, "tools": [] });
        apply(&mut responses, "openai_responses");
        let entry = &responses["tools"][0];
        assert_eq!(entry["type"], "function");
        assert_eq!(entry["name"], "bash");
        assert!(
            entry["parameters"].is_object(),
            "Responses carries `parameters`: {entry}"
        );

        let mut anthropic = json!({ "stream": true, "tools": [] });
        apply(&mut anthropic, "anthropic");
        let entry = &anthropic["tools"][0];
        assert_eq!(entry["name"], "bash");
        assert!(
            entry["input_schema"].is_object(),
            "Anthropic carries `input_schema`: {entry}"
        );
        assert!(
            entry.get("type").is_none(),
            "no `type` on Anthropic: {entry}"
        );
        assert!(entry.get("parameters").is_none());
    }

    #[test]
    fn placeholders_join_the_google_declaration_group() {
        let mut body = json!({
            "stream": true,
            "tools": [{ "functionDeclarations": [{ "name": "Read", "description": "d" }] }],
        });
        apply(&mut body, "google");
        assert_eq!(declared_names(&body), vec!["read", "bash"]);
        assert_eq!(
            body["tools"].as_array().unwrap().len(),
            1,
            "no second group"
        );

        let mut bare = json!({ "stream": true });
        apply(&mut bare, "google-genai");
        assert_eq!(declared_names(&bare), vec!["bash", "read"]);
        assert_eq!(bare["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn recognises_the_zen_endpoints() {
        assert!(is_opencode_endpoint("https://opencode.ai/zen/v1"));
        assert!(is_opencode_endpoint("https://opencode.ai/zen/go/v1"));
        assert!(is_opencode_endpoint("https://opencode.ai/zen/v1/"));
        assert!(is_opencode_endpoint("  https://opencode.ai/zen/v1  "));
    }

    #[test]
    fn rejects_other_endpoints() {
        assert!(!is_opencode_endpoint("https://api.deepseek.com"));
        assert!(!is_opencode_endpoint("https://opencode.ai/theme"));
        assert!(!is_opencode_endpoint("https://opencode.ai/"));
        assert!(!is_opencode_endpoint("http://127.0.0.1:18992/v1"));
        assert!(!is_opencode_endpoint("not a url"));
        assert!(!is_opencode_endpoint(""));
    }
}
