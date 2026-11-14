use crate::_base::di::collection::{CollectionDescriptor, collection};
use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::ServiceIdentifier;
use crate::app::scopes::LifecycleScope;

#[derive(Clone)]
pub struct ContributedFeatureService {
    pub scope: LifecycleScope,
    pub id: ServiceIdentifier<DynamicValue>,
}

pub fn feature_service_contribution() -> CollectionDescriptor<ContributedFeatureService> {
    collection("feature-service", |value, existing| {
        if existing
            .iter()
            .any(|entry| entry.scope == value.scope && entry.id == value.id)
        {
            return Err(format!(
                "Service {} is already contributed at scope {}",
                value.id,
                lifecycle_scope_name(&value.scope)
            ));
        }
        Ok(())
    })
}

fn lifecycle_scope_name(scope: &LifecycleScope) -> &'static str {
    match scope {
        LifecycleScope::App => "app",
        LifecycleScope::Session => "session",
        LifecycleScope::Agent => "agent",
    }
}
