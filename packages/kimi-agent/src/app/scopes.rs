pub use crate::_base::di::scope::LifecycleScope;

pub const SCOPE_TOPOLOGY: [LifecycleScope; 3] = [
    LifecycleScope::App,
    LifecycleScope::Session,
    LifecycleScope::Agent,
];

pub fn register_scope_topology() -> Result<(), crate::_base::errors::errors::BugIndicatingError> {
    crate::_base::di::scope::set_scope_topology(
        SCOPE_TOPOLOGY.iter().map(|scope| scope.as_str().to_string()).collect(),
    )
}
