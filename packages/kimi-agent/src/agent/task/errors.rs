pub use kimi_core::primitives::errors::domains::task as TaskErrors;

pub fn register_task_errors() {
    let _ = kimi_core::primitives::errors::register_error_domain(&TaskErrors::DOMAIN);
}
