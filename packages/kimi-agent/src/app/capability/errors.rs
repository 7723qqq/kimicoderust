pub use kimi_core::primitives::errors::domains::capability as CapabilityErrors;
pub use kimi_core::primitives::errors::domains::capability::{
    CAPABILITY_INSTALL_IN_PROGRESS, CAPABILITY_NOT_FOUND, CAPABILITY_PROCESS_CONTROL_FAILED,
    CAPABILITY_PROCESSES_RUNNING, CAPABILITY_UNSUPPORTED,
};

pub fn register_capability_errors() {
    let _ = kimi_core::primitives::errors::register_error_domain(&CapabilityErrors::DOMAIN);
}
