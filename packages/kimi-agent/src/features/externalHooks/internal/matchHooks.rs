use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Map, Value};

use super::super::app::externalHooksRunner::ExternalHooksRunnerTriggerArgs;
use super::runHook::{RunHookOptions, run_hook};
use super::types::{HookAction, HookBlockDecision, HookDef, HookResult};

pub const DEFAULT_HOOK_TIMEOUT_SECONDS: u64 = 30;

#[derive(Clone, Default)]
pub struct HookRunCallbacks {
    pub on_triggered: Option<Arc<dyn Fn(&str, &str, usize) + Send + Sync>>,
    pub on_resolved: Option<Arc<dyn Fn(&str, &str, &str, Option<&str>, u128) + Send + Sync>>,
    pub on_error: Option<Arc<dyn Fn(&str, &HookDef, &str) + Send + Sync>>,
}

pub fn index_hooks(hooks: &[HookDef]) -> BTreeMap<String, Vec<HookDef>> {
    let mut by_event: BTreeMap<String, Vec<HookDef>> = BTreeMap::new();
    for hook in hooks {
        by_event.entry(hook.event.clone()).or_default().push(hook.clone());
    }
    by_event
}

pub async fn run_matched_hooks(
    by_event: &BTreeMap<String, Vec<HookDef>>,
    event: &str,
    args: ExternalHooksRunnerTriggerArgs,
    callbacks: HookRunCallbacks,
    fail_closed: bool,
) -> Vec<HookResult> {
    let matcher_value = args
        .matcher_value
        .as_ref()
        .map(|value| value.text())
        .unwrap_or_default();
    let cwd = args.cwd.clone().unwrap_or_default();
    let mut matched = Vec::new();
    let mut seen = HashSet::new();
    for hook in by_event.get(event).into_iter().flatten() {
        if !matches(&hook.matcher, &matcher_value) {
            continue;
        }
        let key = format!("{}\0{}", hook.cwd.as_deref().unwrap_or(""), hook.command);
        if !seen.insert(key) {
            continue;
        }
        matched.push(hook.clone());
    }
    if matched.is_empty() {
        return Vec::new();
    }
    if let Some(callback) = &callbacks.on_triggered {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback(event, &matcher_value, matched.len());
        }));
    }

    let mut input_data = Map::new();
    input_data.insert("hookEventName".to_string(), Value::String(event.to_string()));
    input_data.insert(
        "sessionId".to_string(),
        Value::String(args.session_id.clone().unwrap_or_default()),
    );
    input_data.insert("cwd".to_string(), Value::String(cwd.clone()));
    input_data.extend(args.input_data);
    let input_data = to_hook_input_data(&input_data);

    let started_at = Instant::now();
    let results = futures_util::future::join_all(matched.iter().map(|hook| {
        run_hook(
            &hook.command,
            &input_data,
            RunHookOptions {
                timeout: hook.timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECONDS),
                cwd: hook
                    .cwd
                    .clone()
                    .or_else(|| (!cwd.is_empty()).then(|| cwd.clone())),
                env: hook.env.clone(),
                signal: args.signal.clone(),
                fail_closed,
            },
        )
    }))
    .await;

    for (hook, result) in matched.iter().zip(results.iter()) {
        if let Some(failure) = hook_execution_error(hook, result)
            && let Some(callback) = &callbacks.on_error
        {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                callback(event, hook, &failure);
            }));
        }
    }

    let decision = block_decision(event, &results);
    if let Some(callback) = &callbacks.on_resolved {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback(
                event,
                &matcher_value,
                if decision.is_some() { "block" } else { "allow" },
                decision.as_ref().map(|decision| decision.reason.as_str()),
                started_at.elapsed().as_millis(),
            );
        }));
    }
    results
}

pub fn block_decision(event: &str, results: &[HookResult]) -> Option<HookBlockDecision> {
    let block = results
        .iter()
        .find(|result| result.action == HookAction::Block)?;
    let reason = block
        .reason
        .as_deref()
        .map(trim_js)
        .filter(|reason| !reason.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Blocked by {event} hook"));
    Some(HookBlockDecision { block: true, reason })
}

pub fn hook_execution_error(hook: &HookDef, result: &HookResult) -> Option<String> {
    if let Some(error) = &result.error {
        return Some(error.clone());
    }
    if result.timed_out == Some(true) {
        return Some(format!(
            "timed out after {}s",
            hook.timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECONDS)
        ));
    }
    if let Some(code) = result.exit_code
        && code != 0
        && code != 2
    {
        return Some(format!("exited with exit code {code}"));
    }
    None
}

fn matches(pattern: &str, value: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    crate::tools::regex_js::is_match_with_flags(pattern, value, "").unwrap_or(false)
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

fn to_hook_input_data(input: &Map<String, Value>) -> Map<String, Value> {
    input
        .iter()
        .map(|(key, value)| (camel_to_snake(key), value.clone()))
        .collect()
}

fn camel_to_snake(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 8);
    for character in value.chars() {
        if character.is_ascii_uppercase() {
            output.push('_');
            output.push(character.to_ascii_lowercase());
        } else {
            output.push(character);
        }
    }
    output
}
