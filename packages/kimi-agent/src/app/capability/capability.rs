use async_trait::async_trait;
use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::event::Event;
use crate::wire::record::WireAsyncError;

use super::types::{
    CapabilityDescriptor, CapabilityInstallChange, CapabilityInstallOptions, CapabilityStatus,
};

#[async_trait]
pub trait ICapabilityService: Send + Sync {
    fn on_did_change_install(&self) -> Event<CapabilityInstallChange>;
    fn describe_capabilities(&self) -> Vec<CapabilityDescriptor>;
    async fn list_capabilities(&self) -> Vec<CapabilityStatus>;
    async fn get_capability(&self, id: &str) -> Result<CapabilityStatus, WireAsyncError>;
    async fn install_capability(
        &self,
        id: &str,
        options: Option<CapabilityInstallOptions>,
    ) -> Result<CapabilityStatus, WireAsyncError>;
}

pub static I_CAPABILITY_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("capabilityService"));
