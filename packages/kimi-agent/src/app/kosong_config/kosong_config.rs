use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::Error2;
use serde_json::Value;

#[async_trait]
pub trait IKosongConfigService: Send + Sync {
    async fn ready(&self) -> Result<(), Error2>;
}

#[derive(Debug, Clone, Default)]
pub struct ConfigInspectValue {
    pub value: Option<Value>,
    pub default_value: Option<Value>,
    pub user_value: Option<Value>,
    pub memory_value: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct ConfigReplaceSectionsOptions {
    pub preserve_unknown: Option<bool>,
    pub exact_keys: BTreeMap<String, Vec<String>>,
    pub expected_values: BTreeMap<String, Option<Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigChangeSource {
    Load,
    Reload,
    Set,
}

#[derive(Debug, Clone)]
pub struct ConfigSectionChangedEvent {
    pub domain: String,
    pub source: ConfigChangeSource,
    pub value: Option<Value>,
    pub previous_value: Option<Value>,
}

pub type ConfigSectionChangeListener =
    Arc<dyn Fn(ConfigSectionChangedEvent) + Send + Sync>;
pub type Dispose = Box<dyn FnOnce() + Send>;

#[async_trait]
pub trait KosongConfigStore: Send + Sync {
    async fn ready(&self) -> Result<(), Error2>;
    fn get(&self, domain: &str) -> Option<Value>;
    fn inspect(&self, domain: &str) -> ConfigInspectValue;
    async fn replace(&self, domain: &str, value: Option<Value>) -> Result<(), Error2>;
    async fn replace_sections(
        &self,
        sections: BTreeMap<String, Option<Value>>,
        options: Option<ConfigReplaceSectionsOptions>,
    ) -> Result<(), Error2>;
    async fn reload(&self) -> Result<(), Error2>;
}

pub trait KosongConfigChangeStore: KosongConfigStore {
    fn on_did_section_change(&self, listener: ConfigSectionChangeListener) -> Dispose;
}
