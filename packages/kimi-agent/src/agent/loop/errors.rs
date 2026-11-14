pub use kimi_core::primitives::errors::domains::agent_loop as LoopErrors;

pub fn register_loop_errors() {
    let _ = kimi_core::primitives::errors::register_error_domain(&LoopErrors::DOMAIN);
}
