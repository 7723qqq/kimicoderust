use super::super::agent_call_display;
use serde_json::json;

/// v2 labels a swarm by its size, which is what tells a reader the call
/// fans out rather than spawning one child.
#[test]
fn a_swarm_display_names_its_size() {
    let display = agent_call_display(
        "AgentSwarm",
        &json!({
            "description": "three-way review",
            "prompt_template": "review {{item}}",
            "items": ["a", "b", "c"],
        }),
    )
    .expect("AgentSwarm declares a display");
    assert_eq!(display["kind"], "agent_call");
    assert_eq!(display["agent_name"], "swarm (3 subagents)");
    assert_eq!(display["prompt"], "three-way review");
}

/// Items and resumes add up — a mixed call is still one swarm.
#[test]
fn a_swarm_display_counts_items_and_resumes() {
    let display = agent_call_display(
        "agent_swarm",
        &json!({
            "description": "mixed",
            "items": ["a", "b"],
            "resume_agent_ids": { "sub-1": "continue" },
        }),
    )
    .expect("the snake_case spelling is the same tool");
    assert_eq!(display["agent_name"], "swarm (3 subagents)");
}

#[test]
fn a_single_agent_display_uses_the_profile_and_prompt() {
    let display = agent_call_display(
        "Agent",
        &json!({ "prompt": "investigate this", "subagent_type": "research" }),
    )
    .expect("Agent declares a display");
    assert_eq!(display["agent_name"], "research");
    assert_eq!(display["prompt"], "investigate this");
}

/// v2 counts the RAW argument lengths, so a malformed call still reports
/// the size the model asked for.
#[test]
fn the_count_is_the_raw_argument_length() {
    let display = agent_call_display(
        "AgentSwarm",
        &json!({ "description": "d", "items": ["a", "", "b"] }),
    )
    .expect("display");
    assert_eq!(display["agent_name"], "swarm (3 subagents)");
}

/// Tools that declare no display must not grow one.
#[test]
fn other_tools_get_no_display() {
    assert!(agent_call_display("Bash", &json!({ "command": "ls" })).is_none());
    assert!(agent_call_display("Read", &json!({ "path": "a" })).is_none());
    assert!(agent_call_display("AgentSwarm", &json!("not an object")).is_none());
}
