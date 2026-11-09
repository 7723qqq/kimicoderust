use crate::turn_loop::types::{ExecutableToolResult, ToolCall};

pub const REPEAT_BREAKER_ENV: &str = "KIMI_CODE_REPEAT_BREAKER";

pub fn repeat_breaker_enabled() -> bool {
    crate::env::env_switch_default_off(REPEAT_BREAKER_ENV)
}

const REPEAT_REMINDER_1_START: u32 = 3;
const REPEAT_REMINDER_2_START: u32 = 5;
const REPEAT_REMINDER_3_START: u32 = 8;
const REPEAT_FORCE_STOP_STREAK: u32 = 12;

pub fn handoff_veto_text() -> String {
    format!(
        "This turn was ended by the repeat breaker after the same tool call was issued \
{REPEAT_FORCE_STOP_STREAK} times in a row. This step accepts a text response only, \
so the tool call was not executed. Reply in text: the current blocker, what you tried, \
and what you need next."
    )
}

const REMINDER_TEXT_1: &str = "\n\n<system-reminder>\n\
The same tool call has been repeated several times in a row. \
Before making your next call, write one sentence stating what new information you expect it to produce. \
Then act on that sentence: if it names something this result does not already give you, \
choose the action that best provides it; otherwise, continue with the evidence you already have.\
\n</system-reminder>";

const REMINDER_TEXT_3: &str = "\n\n<system-reminder>\n\
Write your final response now, without any further tool calls. \
Cover: the current blocker, each approach you have tried and what it established, \
and the specific information or decision you need from the user to unblock progress. \
Text only.\
\n</system-reminder>";

fn reminder_text_2(repeat_count: u32) -> String {
    format!(
        "\n\n<system-reminder>\n\
The same tool call has now been issued {repeat_count} times in a row. \
Choose exactly one of the following and state your choice before acting:\n\
(1) Falsification check: run the cheapest test that could conclusively disprove your current approach, if such a test exists.\n\
(2) Missing input: tell the user precisely what information or decision you need to proceed, and ask for it.\n\
(3) Conclude: deliver your best result based on the evidence already gathered, listing anything that remains uncertain.\
\n</system-reminder>"
    )
}

pub fn canonical_args(arguments: &serde_json::Value) -> String {
    serde_json::to_string(arguments).unwrap_or_else(|_| arguments.to_string())
}

pub fn make_key(tool_name: &str, arguments: &serde_json::Value) -> String {
    format!("{tool_name} {}", canonical_args(arguments))
}

pub fn dedupe_key(call: &ToolCall) -> String {
    match call.args_parse_error.as_ref() {
        Some(failure) => make_key(&call.name, &serde_json::Value::String(failure.raw.clone())),
        None => make_key(&call.name, &call.arguments),
    }
}

#[derive(Debug, Clone)]
pub struct StepPlan {
    pub keys: Vec<String>,

    pub original_of: Vec<usize>,
}

#[derive(Default)]
pub struct DedupeGuard {
    consecutive_key: Option<String>,
    consecutive_count: u32,

    exempt_seq: u64,

    repeat_breaker: bool,
}

impl DedupeGuard {
    pub fn new() -> Self {
        Self::with_repeat_breaker(repeat_breaker_enabled())
    }

    pub fn with_repeat_breaker(repeat_breaker: bool) -> Self {
        Self {
            repeat_breaker,
            ..Self::default()
        }
    }

    pub fn plan_step(&mut self, calls: &[ToolCall]) -> StepPlan {
        self.plan_step_by(calls, |call| Some(dedupe_key(call)))
    }

    pub fn plan_step_by<F>(&mut self, calls: &[ToolCall], mut key_of: F) -> StepPlan
    where
        F: FnMut(&ToolCall) -> Option<String>,
    {
        let mut keys = Vec::with_capacity(calls.len());
        let mut original_of = Vec::with_capacity(calls.len());
        for call in calls.iter() {
            let key = key_of(call).unwrap_or_else(|| {
                let id = self.exempt_seq;
                self.exempt_seq += 1;
                format!("\x00exempt-{id}")
            });
            let original = keys.iter().position(|k| *k == key).unwrap_or(keys.len());
            original_of.push(original);
            keys.push(key);
        }
        StepPlan { keys, original_of }
    }

    pub fn finalize_step(&mut self, plan: &StepPlan, results: &mut [ExecutableToolResult]) -> bool {
        let pre_key = self.consecutive_key.clone();
        let pre_count = self.consecutive_count;
        let mut force_stop = false;

        for (i, result) in results.iter_mut().enumerate() {
            if plan.original_of[i] != i {
                continue;
            }

            if !self.repeat_breaker {
                continue;
            }
            let streak = streak_at(&plan.keys, i, pre_key.as_deref(), pre_count);
            if streak >= REPEAT_FORCE_STOP_STREAK {
                result.content.push_str(REMINDER_TEXT_3);
                force_stop = true;
            } else if streak >= REPEAT_REMINDER_3_START {
                result.content.push_str(REMINDER_TEXT_3);
            } else if streak >= REPEAT_REMINDER_2_START {
                result.content.push_str(&reminder_text_2(streak));
            } else if streak >= REPEAT_REMINDER_1_START {
                result.content.push_str(REMINDER_TEXT_1);
            }
        }

        for (i, original) in plan.original_of.iter().enumerate() {
            if *original != i {
                results[i] = results[*original].clone();
            }
        }

        for key in &plan.keys {
            if self.consecutive_key.as_deref() == Some(key.as_str()) {
                self.consecutive_count += 1;
            } else {
                self.consecutive_key = Some(key.clone());
                self.consecutive_count = 1;
            }
        }
        force_stop
    }
}

fn streak_at(keys: &[String], index: usize, pre_key: Option<&str>, pre_count: u32) -> u32 {
    let mut last_key = pre_key;
    let mut streak = pre_count;
    for key in keys.iter().take(index + 1) {
        if last_key == Some(key.as_str()) {
            streak += 1;
        } else {
            last_key = Some(key.as_str());
            streak = 1;
        }
    }
    streak
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
            extras: None,
            args_parse_error: None,
        }
    }

    fn read_call(id: &str, path: &str) -> ToolCall {
        call(id, "Read", serde_json::json!({ "path": path }))
    }

    fn result(content: &str) -> ExecutableToolResult {
        ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: content.into(),
            is_error: false,
            note: None,
            display: None,
        }
    }

    #[test]
    fn breaker_defaults_off_when_the_env_is_unset() {
        let _scope = crate::test_support::EnvScope::unset(&[REPEAT_BREAKER_ENV]);
        assert!(!repeat_breaker_enabled());
        let mut guard = DedupeGuard::new();
        for _ in 0..12 {
            let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
            let mut results = vec![result("ok")];
            assert!(!guard.finalize_step(&plan, &mut results));
            assert_eq!(results[0].content, "ok");
        }
    }

    #[test]
    fn explicit_truthy_env_arms_the_breaker() {
        let _scope =
            crate::test_support::EnvScope::set(vec![(REPEAT_BREAKER_ENV, Some("1".into()))]);
        assert!(repeat_breaker_enabled());
    }

    #[test]
    fn canonical_args_sorts_object_keys() {
        let a = serde_json::json!({ "b": 1, "a": { "d": 2, "c": 3 } });
        let b = serde_json::json!({ "a": { "c": 3, "d": 2 }, "b": 1 });
        assert_eq!(canonical_args(&a), canonical_args(&b));
        assert_eq!(canonical_args(&a), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }

    #[test]
    fn make_key_combines_name_and_args() {
        let key = make_key("Read", &serde_json::json!({ "path": "a.rs" }));
        assert_eq!(key, r#"Read {"path":"a.rs"}"#);
    }

    fn broken_call(id: &str, raw: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "Bash".into(),
            arguments: serde_json::json!({}),
            extras: None,
            args_parse_error: Some(crate::turn_loop::types::ArgsParseError {
                raw: raw.into(),
                error: "EOF while parsing a value".into(),
            }),
        }
    }

    #[test]
    fn identical_broken_payloads_streak() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);
        let mut last = String::new();
        for i in 0..3 {
            let plan = guard.plan_step(&[broken_call(&format!("c{i}"), r#"{"command":"#)]);
            let mut results = vec![result("bad")];
            guard.finalize_step(&plan, &mut results);
            last = results[0].content.clone();
        }
        assert!(last.contains("repeated several times in a row"), "{last}");
    }

    #[test]
    fn different_broken_payloads_never_streak() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);
        for i in 0..12 {
            let raw = format!(r#"{{"command":"ls {i}"#);
            let plan = guard.plan_step(&[broken_call(&format!("c{i}"), &raw)]);
            let mut results = vec![result("bad")];
            assert!(
                !guard.finalize_step(&plan, &mut results),
                "distinct payload {raw} must not force-stop the turn"
            );
            assert_eq!(results[0].content, "bad", "no reminder either");
        }
    }

    #[test]
    fn plan_step_marks_same_step_duplicates() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);
        let plan = guard.plan_step(&[
            read_call("c1", "a.rs"),
            read_call("c2", "b.rs"),
            read_call("c3", "a.rs"),
        ]);
        assert_eq!(plan.original_of, vec![0, 1, 0]);
        assert_eq!(plan.keys[0], plan.keys[2]);
    }

    #[test]
    fn plan_step_by_exempts_none_keyed_calls() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);

        let plan = guard.plan_step_by(
            &[read_call("c1", "a.rs"), read_call("c2", "a.rs")],
            |call| (call.id == "c1").then(|| make_key(&call.name, &call.arguments)),
        );
        assert_eq!(plan.original_of, vec![0, 1]);
    }

    #[test]
    fn exempt_calls_never_streak_across_steps() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);

        for _ in 0..15 {
            let plan = guard.plan_step_by(&[read_call("c1", "a.rs")], |_| None);
            let mut results = vec![result("ok")];
            assert!(!guard.finalize_step(&plan, &mut results));
            assert_eq!(results[0].content, "ok");
        }
    }

    #[test]
    fn no_reminder_below_threshold() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);
        for _ in 0..2 {
            let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
            let mut results = vec![result("ok")];
            assert!(!guard.finalize_step(&plan, &mut results));
            assert_eq!(results[0].content, "ok");
        }
    }

    #[test]
    fn reminder_1_at_streak_3_across_steps() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);
        let mut last = String::new();
        for _ in 0..3 {
            let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
            let mut results = vec![result("ok")];
            assert!(!guard.finalize_step(&plan, &mut results));
            last = results[0].content.clone();
        }
        assert!(last.starts_with("ok\n\n<system-reminder>"));
        assert!(last.contains("repeated several times in a row"));
    }

    #[test]
    fn reminder_2_embeds_streak_count_at_5() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);
        let mut last = String::new();
        for _ in 0..5 {
            let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
            let mut results = vec![result("ok")];
            guard.finalize_step(&plan, &mut results);
            last = results[0].content.clone();
        }
        assert!(last.contains("issued 5 times in a row"));
    }

    #[test]
    fn reminder_3_at_8_and_force_stop_at_12() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);
        let mut stopped = false;
        for round in 0..12 {
            let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
            let mut results = vec![result("ok")];
            stopped = guard.finalize_step(&plan, &mut results);
            if round == 7 {
                assert!(results[0].content.contains("Write your final response now"));
                assert!(!stopped);
            }
        }
        assert!(stopped, "streak 12 must force-stop the turn");
    }

    #[test]
    fn different_call_resets_the_streak() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);
        for _ in 0..2 {
            let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
            let mut results = vec![result("ok")];
            guard.finalize_step(&plan, &mut results);
        }

        let plan = guard.plan_step(&[read_call("c1", "b.rs")]);
        let mut results = vec![result("ok")];
        guard.finalize_step(&plan, &mut results);

        for _ in 0..2 {
            let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
            let mut results = vec![result("ok")];
            guard.finalize_step(&plan, &mut results);
            assert_eq!(results[0].content, "ok");
        }
    }

    #[test]
    fn streak_extends_within_a_multi_call_step() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);

        let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
        let mut results = vec![result("ok")];
        guard.finalize_step(&plan, &mut results);

        let plan = guard.plan_step(&[read_call("c2", "a.rs"), read_call("c3", "a.rs")]);
        let mut results = vec![result("raw-1"), result("raw-2")];
        assert!(!guard.finalize_step(&plan, &mut results));
        assert_eq!(results[0].content, "raw-1");
        assert_eq!(results[0].content, results[1].content);

        let plan = guard.plan_step(&[read_call("c4", "a.rs")]);
        let mut results = vec![result("raw-3")];
        assert!(!guard.finalize_step(&plan, &mut results));
        assert!(
            results[0]
                .content
                .contains("repeated several times in a row")
        );
    }

    #[test]
    fn breaker_disabled_skips_ladder_but_keeps_same_step_dedupe() {
        let mut guard = DedupeGuard::with_repeat_breaker(false);
        for _ in 0..12 {
            let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
            let mut results = vec![result("ok")];
            assert!(
                !guard.finalize_step(&plan, &mut results),
                "breaker off must never force-stop"
            );
            assert_eq!(results[0].content, "ok", "breaker off injects nothing");
        }
        let plan = guard.plan_step(&[read_call("c2", "b.rs"), read_call("c3", "b.rs")]);
        let mut results = vec![result("ran"), result("never-ran")];
        assert!(!guard.finalize_step(&plan, &mut results));
        assert_eq!(results[1].content, results[0].content);
    }

    #[test]
    fn duplicate_gets_original_final_result() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);

        for _ in 0..2 {
            let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
            let mut results = vec![result("ok")];
            guard.finalize_step(&plan, &mut results);
        }
        let plan = guard.plan_step(&[read_call("c2", "a.rs"), read_call("c3", "a.rs")]);
        let mut results = vec![result("original-output"), result("never-ran")];
        guard.finalize_step(&plan, &mut results);
        assert_eq!(results[1].content, results[0].content);
        assert!(results[1].content.starts_with("original-output"));
    }

    #[test]
    fn force_stop_preserves_error_flag() {
        let mut guard = DedupeGuard::with_repeat_breaker(true);
        for _ in 0..11 {
            let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
            let mut results = vec![result("ok")];
            guard.finalize_step(&plan, &mut results);
        }
        let plan = guard.plan_step(&[read_call("c1", "a.rs")]);
        let mut results = vec![ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: "boom".into(),
            is_error: true,
            note: None,
            display: None,
        }];
        assert!(guard.finalize_step(&plan, &mut results));
        assert!(results[0].is_error);
        assert!(results[0].content.starts_with("boom\n\n<system-reminder>"));
    }
}
