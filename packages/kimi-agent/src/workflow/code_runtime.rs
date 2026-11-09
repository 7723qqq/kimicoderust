//! `run_code` sandbox.
//!
//! v2 (`a0ccd487`) executes model-authored JavaScript in a fresh subprocess
//! (`codeExecutor.ts`) with a 512 MB RSS ceiling, a 30 s default timeout and a
//! 100k-character output budget. This port keeps the same contract but runs the
//! program in the embedded QuickJS runtime the workflow engine already uses
//! ([`rquickjs`]).
//!
//! The workflow sandbox in [`super::runtime`] is **not** reused as-is: it
//! injects `readFile` / `writeFile` / `exec` / `fetch` / `search` host
//! functions and has neither a memory limit nor an interrupt handler. `run_code`
//! gets its own context with only `console` and a closed `require` allowlist,
//! plus two hard bounds:
//!
//! * `JS_SetMemoryLimit` — allocations beyond [`MEMORY_CAP_BYTES`] fail inside
//!   QuickJS instead of growing the host process.
//! * `JS_SetInterruptHandler` — checked against a wall clock deadline, so a
//!   synchronous `while (true) {}` is interrupted from inside the engine rather
//!   than by abandoning the thread.
//!
//! Every wait on the engine goes through [`crate::primitives::deadline`].

#![cfg_attr(not(feature = "workflow-js"), allow(dead_code))]

#[cfg(feature = "workflow-js")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "workflow-js")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "workflow-js")]
use std::time::{Duration, Instant};

#[cfg(feature = "workflow-js")]
use serde::Deserialize;
use serde_json::Value;

#[cfg(feature = "workflow-js")]
use crate::primitives::deadline::Deadline;

#[cfg(feature = "workflow-js")]
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, Function, Promise};

/// v2 `DEFAULT_MEMORY_LIMIT_BYTES` (`codeRuntime.ts`).
pub const MEMORY_CAP_BYTES: usize = 512 * 1024 * 1024;
/// v2 `runCodeTool.ts` default execution budget.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// v2 `runCodeTool.ts` lower bound.
pub const MIN_TIMEOUT_MS: u64 = 1_000;
/// v2 `runCodeTool.ts` upper bound.
pub const MAX_TIMEOUT_MS: u64 = 120_000;
/// v2 `DEFAULT_MAX_OUTPUT_CHARS` (`codeRuntime.ts`).
pub const MAX_OUTPUT_CHARS: usize = 100_000;

/// Resource bounds for one program.
#[derive(Debug, Clone, Copy)]
pub struct CodeRunLimits {
    pub timeout_ms: u64,
    pub memory_limit_bytes: usize,
    pub max_output_chars: usize,
}

impl Default for CodeRunLimits {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            memory_limit_bytes: MEMORY_CAP_BYTES,
            max_output_chars: MAX_OUTPUT_CHARS,
        }
    }
}

/// A failed program run. `kind` uses the v2 vocabulary (`codeRuntime.ts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRunError {
    pub kind: &'static str,
    pub message: String,
}

/// What `execute_code` reports back: captured console lines, the completion
/// value (`None` for `undefined`) and at most one error.
#[derive(Debug, Clone, Default)]
pub struct CodeRunOutcome {
    pub logs: Vec<String>,
    pub value: Option<Value>,
    pub error: Option<CodeRunError>,
}

impl CodeRunOutcome {
    pub(crate) fn failed(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            logs: Vec::new(),
            value: None,
            error: Some(CodeRunError {
                kind,
                message: message.into(),
            }),
        }
    }
}

/// Run `code` in a fresh sandbox.
///
/// The caller owns the outer hard bound: this function returns as soon as the
/// interrupt handler trips or the engine goes idle, and never observes the
/// host filesystem.
pub async fn execute_code(code: &str, limits: CodeRunLimits) -> CodeRunOutcome {
    #[cfg(feature = "workflow-js")]
    {
        execute_code_js(code, limits).await
    }
    #[cfg(not(feature = "workflow-js"))]
    {
        let _ = (code, limits);
        CodeRunOutcome::failed(
            "sandbox-unavailable",
            "the run_code JS engine is not compiled into this build",
        )
    }
}

#[cfg(feature = "workflow-js")]
async fn execute_code_js(code: &str, limits: CodeRunLimits) -> CodeRunOutcome {
    let runtime = match AsyncRuntime::new() {
        Ok(runtime) => runtime,
        Err(error) => return CodeRunOutcome::failed("sandbox-unavailable", error.to_string()),
    };
    runtime.set_memory_limit(limits.memory_limit_bytes).await;

    // The interrupt handler runs inside QuickJS, on this thread, while the
    // program is executing. It must not depend on a tokio timer: a synchronous
    // loop never yields, so an external `timeout` would never be polled.
    let wall_deadline = Instant::now() + Duration::from_millis(limits.timeout_ms);
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let interrupted = interrupted.clone();
        let handler = Box::new(move || {
            if Instant::now() >= wall_deadline {
                interrupted.store(true, Ordering::SeqCst);
                true
            } else {
                false
            }
        });
        runtime.set_interrupt_handler(Some(handler)).await;
    }

    let context = match AsyncContext::full(&runtime).await {
        Ok(context) => context,
        Err(error) => return CodeRunOutcome::failed("sandbox-unavailable", error.to_string()),
    };

    let logs = Arc::new(Mutex::new(LogBuffer::new(limits.max_output_chars)));
    let deadline = Deadline::new("run_code", Duration::from_millis(limits.timeout_ms));

    let raw = {
        let logs = logs.clone();
        let deadline = deadline.clone();
        let interrupted = interrupted.clone();
        rquickjs::async_with!(context => |ctx| {
            if let Err(error) = install_log_sink(&ctx, logs) {
                return RawOutcome::Failure(CodeRunError {
                    kind: "worker-error",
                    message: render_js_error(&ctx, error),
                });
            }

            let wrapped = include_str!("run_code_prelude.js")
                .replace("/*__RUN_CODE_BODY__*/", code);
            let promise: Promise = match ctx.eval(wrapped) {
                Ok(promise) => promise,
                Err(error) => return classify_engine_error(&ctx, error, &interrupted),
            };

            // The wrap is an async IIFE: the synchronous part (including a
            // dead loop) has already run by the time `eval` returns, and a
            // pending `await` is bounded by the shared deadline below.
            match deadline
                .wait("waiting for the program to finish", promise.into_future::<String>())
                .await
            {
                Ok(Ok(envelope)) => RawOutcome::Envelope(envelope),
                Ok(Err(error)) => classify_engine_error(&ctx, error, &interrupted),
                Err(_) => RawOutcome::TimedOut,
            }
        })
        .await
    };

    // Stop interrupts before draining the job queue. These phases have no
    // reference counterpart and therefore no bound: the reference's only
    // timer is the execution watchdog itself.
    let _ = runtime.set_interrupt_handler(None).await;
    let _ = runtime.idle().await;
    drop(context);

    let (logs, remaining) = match logs.lock() {
        Ok(buffer) => (buffer.lines.clone(), buffer.remaining()),
        Err(_) => (Vec::new(), 0),
    };

    match raw {
        RawOutcome::Envelope(envelope) => finish_success(envelope, logs, remaining),
        RawOutcome::Failure(error) => CodeRunOutcome {
            logs,
            value: None,
            error: Some(error),
        },
        RawOutcome::TimedOut => CodeRunOutcome {
            logs,
            value: None,
            error: Some(CodeRunError {
                kind: "timeout",
                message: format!("program did not finish within {}ms", limits.timeout_ms),
            }),
        },
    }
}

#[cfg(feature = "workflow-js")]
enum RawOutcome {
    Envelope(String),
    Failure(CodeRunError),
    TimedOut,
}

#[cfg(feature = "workflow-js")]
fn classify_engine_error(
    ctx: &Ctx<'_>,
    error: rquickjs::Error,
    interrupted: &AtomicBool,
) -> RawOutcome {
    if interrupted.load(Ordering::SeqCst) {
        return RawOutcome::TimedOut;
    }
    let message = render_js_error(ctx, error);
    let kind = if message.to_ascii_lowercase().contains("out of memory") {
        "memory-limit"
    } else {
        "exception"
    };
    RawOutcome::Failure(CodeRunError { kind, message })
}

#[cfg(feature = "workflow-js")]
fn render_js_error(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    rquickjs::CaughtError::from_error(ctx, error).to_string()
}

#[cfg(feature = "workflow-js")]
fn install_log_sink<'js>(ctx: &Ctx<'js>, logs: Arc<Mutex<LogBuffer>>) -> rquickjs::Result<()> {
    let sink = Function::new(ctx.clone(), move |text: String| {
        if let Ok(mut buffer) = logs.lock() {
            buffer.push(text);
        }
    })?;
    ctx.globals().set("__native_log", sink)
}

#[cfg(feature = "workflow-js")]
#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(rename = "hasValue", default)]
    has_value: bool,
    #[serde(default)]
    json: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Apply the shared output budget to a program that finished. Mirrors
/// `prepareCompletion` in `codeShimSource.ts`: a truncated log stream leaves
/// no room for a completion value, and an unserializable completion is an
/// `invalid-output` error.
#[cfg(feature = "workflow-js")]
fn finish_success(envelope: String, logs: Vec<String>, remaining: usize) -> CodeRunOutcome {
    let envelope: Envelope = match serde_json::from_str(&envelope) {
        Ok(envelope) => envelope,
        Err(error) => {
            return CodeRunOutcome {
                logs,
                value: None,
                error: Some(CodeRunError {
                    kind: "worker-error",
                    message: format!("the sandbox returned malformed output: {error}"),
                }),
            };
        }
    };

    if !envelope.ok {
        return CodeRunOutcome {
            logs,
            value: None,
            error: Some(CodeRunError {
                kind: static_kind(envelope.kind.as_deref()),
                message: envelope
                    .message
                    .unwrap_or_else(|| "the program failed".to_string()),
            }),
        };
    }

    if !envelope.has_value {
        return CodeRunOutcome {
            logs,
            value: None,
            error: None,
        };
    }

    let json = envelope.json.unwrap_or_else(|| "null".to_string());
    if json.chars().count() > remaining {
        return CodeRunOutcome {
            logs,
            value: None,
            error: Some(CodeRunError {
                kind: "output-limit",
                message: "program output exceeded the size limit".to_string(),
            }),
        };
    }

    match serde_json::from_str::<Value>(&json) {
        Ok(value) => CodeRunOutcome {
            logs,
            value: Some(value),
            error: None,
        },
        Err(_) => CodeRunOutcome {
            logs,
            value: None,
            error: Some(CodeRunError {
                kind: "invalid-output",
                message: "program completion must be JSON-serializable".to_string(),
            }),
        },
    }
}

#[cfg(feature = "workflow-js")]
fn static_kind(kind: Option<&str>) -> &'static str {
    match kind {
        Some("timeout") => "timeout",
        Some("memory-limit") => "memory-limit",
        Some("output-limit") => "output-limit",
        Some("invalid-output") => "invalid-output",
        Some("worker-error") => "worker-error",
        Some("cancelled") => "cancelled",
        Some("sandbox-unavailable") => "sandbox-unavailable",
        _ => "exception",
    }
}

/// Console budget shared with the Rust host. v2 starts at two characters (the
/// empty `[]` envelope), then charges each line plus its separator.
struct LogBuffer {
    max_chars: usize,
    chars: usize,
    entries: usize,
    truncated: bool,
    lines: Vec<String>,
}

impl LogBuffer {
    fn new(max_chars: usize) -> Self {
        Self {
            max_chars,
            chars: 2,
            entries: 0,
            truncated: false,
            lines: Vec::new(),
        }
    }

    fn push(&mut self, text: String) {
        if self.truncated {
            return;
        }
        let separator = usize::from(self.entries > 0);
        let length = text.chars().count();
        let available = self.max_chars.saturating_sub(self.chars + separator);
        if length <= available {
            self.chars += length + separator;
            self.entries += 1;
            self.lines.push(text);
            return;
        }

        self.truncated = true;
        if available == 0 {
            return;
        }
        let prefix: String = text.chars().take(available).collect();
        self.chars += prefix.chars().count() + separator;
        self.entries += 1;
        self.lines.push(prefix);
    }

    fn remaining(&self) -> usize {
        self.max_chars.saturating_sub(self.chars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "workflow-js")]
    fn limits(timeout_ms: u64) -> CodeRunLimits {
        CodeRunLimits {
            timeout_ms,
            ..CodeRunLimits::default()
        }
    }

    #[cfg(feature = "workflow-js")]
    #[tokio::test]
    async fn runs_code_and_captures_logs_and_value() {
        let outcome = execute_code(
            "console.log('hello', { n: 1 });\nreturn { sum: 1 + 2 };",
            limits(5_000),
        )
        .await;
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.value, Some(serde_json::json!({ "sum": 3 })));
        assert_eq!(outcome.logs, vec!["hello { n: 1 }".to_string()]);
    }

    #[cfg(feature = "workflow-js")]
    #[tokio::test]
    async fn a_bare_return_reports_undefined() {
        let outcome = execute_code("console.log('done');\nreturn;", limits(5_000)).await;
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.value, None);
        assert_eq!(outcome.logs, vec!["done".to_string()]);
    }

    #[cfg(feature = "workflow-js")]
    #[tokio::test]
    async fn the_interrupt_handler_stops_a_dead_loop() {
        let started = Instant::now();
        let outcome = execute_code("while (true) {}", limits(200)).await;
        let error = outcome.error.expect("timeout");
        assert_eq!(error.kind, "timeout", "{error:?}");
        assert!(error.message.contains("200ms"), "{error:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    #[cfg(feature = "workflow-js")]
    #[tokio::test]
    async fn the_memory_limit_is_a_hard_ceiling() {
        let outcome = execute_code(
            "const held = [];\nwhile (true) held.push(new Array(4096).fill('x'));",
            CodeRunLimits {
                timeout_ms: 5_000,
                memory_limit_bytes: 8 * 1024 * 1024,
                max_output_chars: MAX_OUTPUT_CHARS,
            },
        )
        .await;
        let error = outcome.error.expect("memory-limit");
        assert_eq!(error.kind, "memory-limit", "{error:?}");
    }

    #[cfg(feature = "workflow-js")]
    #[tokio::test]
    async fn non_whitelisted_modules_are_refused() {
        let outcome = execute_code("return require('fs').readFileSync('x');", limits(5_000)).await;
        let error = outcome.error.expect("exception");
        assert_eq!(error.kind, "exception", "{error:?}");
        assert!(
            error.message.contains("not available in run_code"),
            "{error:?}"
        );
        assert!(error.message.contains("node:util"), "{error:?}");
    }

    #[cfg(feature = "workflow-js")]
    #[tokio::test]
    async fn thrown_errors_keep_the_script_text() {
        let outcome = execute_code("throw new Error('boom');", limits(5_000)).await;
        let error = outcome.error.expect("exception");
        assert_eq!(error.kind, "exception", "{error:?}");
        assert!(error.message.contains("boom"), "{error:?}");
    }

    #[cfg(feature = "workflow-js")]
    #[tokio::test]
    async fn an_unserializable_completion_is_invalid_output() {
        let outcome = execute_code("return () => 1;", limits(5_000)).await;
        let error = outcome.error.expect("invalid-output");
        assert_eq!(error.kind, "invalid-output", "{error:?}");
    }

    #[test]
    fn the_log_buffer_stops_at_the_budget() {
        let mut buffer = LogBuffer::new(11);
        buffer.push("1234".to_string());
        buffer.push("5678".to_string());
        buffer.push("never".to_string());
        assert_eq!(buffer.lines, vec!["1234".to_string(), "5678".to_string()]);
        assert!(buffer.truncated);
        assert_eq!(buffer.remaining(), 0);
    }
}
