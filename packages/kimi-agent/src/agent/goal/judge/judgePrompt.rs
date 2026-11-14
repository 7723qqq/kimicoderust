pub use crate::goal::judge::{JUDGE_SYSTEM_PROMPT, JudgeVerdict, parse_verdict};

use crate::goal::GoalSnapshot;

pub fn build_judge_user_prompt(objective: &str, completion_criterion: Option<&str>) -> String {
    let mut lines = vec!["## Goal Objective", objective];
    if let Some(criterion) = completion_criterion.filter(|criterion| !criterion.is_empty()) {
        lines.extend([
            "",
            "## Completion Criterion",
            criterion,
            "",
            "## Evaluation Instructions",
            "You MUST verify EACH condition in the completion criterion independently.",
            "For each condition, cite the specific transcript evidence (tool output, command result, file content) that proves it.",
            "If any condition lacks direct evidence, the goal is NOT complete.",
        ]);
    } else {
        lines.extend([
            "",
            "## Evaluation Instructions",
            "No explicit completion criterion was provided. Judge whether the objective",
            "has been fully achieved based on the transcript evidence.",
        ]);
    }
    lines.extend([
        "",
        "## Task",
        "Based on the conversation transcript above, produce your verdict as a single JSON object.",
    ]);
    lines.join("\n")
}

pub fn build_retry_user_prompt(goal: &GoalSnapshot) -> String {
    let mut text = format!("The goal objective is: {}\n", goal.objective);
    if let Some(criterion) = goal
        .completion_criterion
        .as_deref()
        .filter(|criterion| !criterion.is_empty())
    {
        text.push_str(&format!("Completion criterion: {criterion}\n"));
    }
    text.push_str(
        "Based on the conversation transcript above, is this goal complete? Return ONLY {\"ok\": true/false, \"reason\": \"...\"}",
    );
    text
}

pub fn build_judge_verification_prompt(
    objective: &str,
    completion_criterion: Option<&str>,
    cwd: Option<&str>,
) -> String {
    let mut lines = vec!["# Goal Verification Task", "", "## Objective", objective];
    if let Some(criterion) = completion_criterion.filter(|criterion| !criterion.is_empty()) {
        lines.extend([
            "",
            "## Completion Criterion",
            criterion,
            "",
            "## Your Task",
            "Verify EACH condition in the completion criterion by executing commands.",
            "Do NOT trust any prior conversation — verify from scratch by reading files,",
            "running tests, or checking command outputs.",
        ]);
    } else {
        lines.extend([
            "",
            "## Your Task",
            "No explicit completion criterion was provided.",
            "Verify the objective has been achieved by inspecting files and running commands.",
        ]);
    }
    let working_directory = cwd
        .filter(|cwd| !cwd.is_empty())
        .map(|cwd| format!("Working directory: {cwd}"));
    if let Some(working_directory) = &working_directory {
        lines.push("");
        lines.push(working_directory);
    }
    lines.extend([
        "",
        "## Instructions",
        "1. Determine what commands/reads would verify the criterion",
        "2. Execute them using your tools (Bash, Read, Grep)",
        "3. Based on ACTUAL outputs, produce your verdict",
        "4. Output your verdict as the LAST thing you write, as a JSON object:",
        "   {\"ok\": true, \"reason\": \"...\"}  or  {\"ok\": false, \"reason\": \"...\"}",
    ]);
    lines.join("\n")
}
