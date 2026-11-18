use std::sync::Arc;

use serde_json::{Map, Value};

#[derive(Clone)]
pub struct GitHubToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub method: String,
    pub path: Arc<dyn Fn(&Value) -> String + Send + Sync>,
    pub query: Option<Arc<dyn Fn(&Value) -> Map<String, Value> + Send + Sync>>,
    pub body: Option<Arc<dyn Fn(&Value) -> Value + Send + Sync>>,
    pub accept: Option<String>,
    pub max_bytes: Option<usize>,
    pub mutating: Option<bool>,
    pub subject: Arc<dyn Fn(&Value) -> String + Send + Sync>,
}

pub trait IGitHubTool: Send + Sync {}
