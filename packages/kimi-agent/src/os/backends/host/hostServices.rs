use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;

pub fn register_host_services() {
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "hostClock",
    );
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "hostEnvironment",
    );
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "hostFs",
    );
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "hostProcess",
    );
}
