use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::fiber::{
    FiberHandle, FiberProvideOptions, FiberState, ServiceClassRecipe, ServiceRecipe,
};
use crate::_base::di::instantiation::ServiceIdentifier;
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::wire::record::WireAsyncError;

use super::feature_service_contribution::ContributedFeatureService;

#[derive(Clone)]
pub struct ManagedUnitInfo {
    pub name: String,
    pub state: FiberState,
    pub uid: Option<u64>,
    pub meta: Map<String, Value>,
}

#[async_trait]
pub trait IFeatureManager: Send + Sync {
    fn provide_unit(
        &self,
        recipe: ServiceRecipe,
        opts: Option<FiberProvideOptions>,
    ) -> Arc<dyn FiberHandle>;

    fn provide_unit_for_id(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        recipe: ServiceClassRecipe,
        opts: Option<FiberProvideOptions>,
    ) -> Arc<dyn FiberHandle>;

    async fn unprovide_unit(&self, name: &str) -> Result<(), WireAsyncError>;

    async fn update_unit(&self, name: &str, config: Option<Value>) -> Result<(), WireAsyncError>;

    fn units(&self) -> Vec<ManagedUnitInfo>;

    fn contributed_services(&self) -> Vec<ContributedFeatureService>;

    fn on_did_change_units(&self, listener: Arc<dyn Fn() + Send + Sync>) -> DisposableRef;
}
