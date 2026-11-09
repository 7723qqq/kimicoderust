//! napi-rs bindings for the kimi-agent Rust engine.
//!
//! This module exposes the turn loop as a native Node.js addon via napi-rs,
//! enabling direct in-process communication between Node.js and Rust without
//! the stdio JSON-RPC bridge.
//!
//! ## Callback architecture
//!
//! napi-rs 2.16 `call_async` does not properly await JS Promises returned by
//! async callbacks (it tries to convert the Promise object directly to a
//! String, triggering `StringExpected`). To work around this, we use a
//! **callback registry** pattern:
//!
//! 1. Rust assigns a unique `callback_id` + creates a `oneshot` channel.
//! 2. Rust calls the JS function via `tsfn.call()` (fire-and-forget), passing
//!    the input payload and the `callback_id`.
//! 3. The JS function processes the request asynchronously, then calls the
//!    exported `resolveCallback(id, error, result)` napi function.
//! 4. `resolveCallback` looks up the `oneshot` sender and sends the result.
//! 5. The Rust future (from step 1) awaits the `oneshot` receiver.
//!
//! This avoids `call_async` entirely and works with both sync and async JS
//! callbacks.

mod callbacks;
mod errors;
mod mcp;
mod misc;
mod params;
mod plugins;
mod session;
mod turn;

#[cfg(test)]
mod tests;

pub use callbacks::*;
pub use mcp::*;
pub use misc::*;
pub use params::*;
pub use plugins::*;
pub use session::*;
pub use turn::*;

use napi_derive::napi;

/// Emit a one-shot `tracing::info!` event. Used by the test harness to
/// confirm the subscriber is wired (so trace files are non-empty even on
/// trivial inputs that don't traverse instrumented hot paths).
#[napi]
pub fn emit_test_trace_event(message: String) -> napi::Result<()> {
    tracing::info!(test_message = %message, "kimi-agent tracing smoke event");
    Ok(())
}

/// Initialise tracing from `KIMI_AGENT_TRACE` / `KIMI_AGENT_TRACE_FORMAT`.
///
/// Returns `true` when the subscriber was installed by this call, `false`
/// when one was already registered (a process-wide subscriber can only be
/// set once) or when the env is not set (no-op). The test harness and
/// future operators call this from JS to turn the P20-A/B/C / future
/// performance work's tracing spans on without restarting the process.
#[napi]
pub fn init_tracing_from_env() -> napi::Result<bool> {
    use std::sync::Once;
    static STARTED: Once = Once::new();
    let mut installed = false;
    STARTED.call_once(|| {
        let enabled = std::env::var("KIMI_AGENT_TRACE")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if !enabled {
            return;
        }
        let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("kimi_agent=info"));
        // Default to stdout: vitest's reporter captures stderr in some
        // harness modes, and our existing `eprintln!` diagnostics already
        // write to stderr. Sending tracing to stdout keeps the two
        // channels separate and lets vitest users pipe the trace.
        let use_stderr = std::env::var("KIMI_AGENT_TRACE_STDERR").is_ok();
        let use_json = std::env::var("KIMI_AGENT_TRACE_FORMAT").as_deref() == Ok("json");
        let result = if use_stderr {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .try_init()
        } else if use_json {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .json()
                .with_writer(std::io::stdout)
                .try_init()
        } else {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stdout)
                .try_init()
        };
        if result.is_ok() {
            installed = true;
        }
    });
    Ok(installed)
}
