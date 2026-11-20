use std::sync::Arc;

use serde_json::Value;

#[derive(Clone)]
pub struct GitHubToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub method: String,
    pub path: Arc<dyn Fn(&Value) -> String + Send + Sync>,
    pub query: Option<Arc<dyn Fn(&Value) -> Vec<(String, Value)> + Send + Sync>>,
    pub body: Option<Arc<dyn Fn(&Value) -> Vec<(String, Value)> + Send + Sync>>,
    pub accept: Option<String>,
    pub max_bytes: Option<usize>,
    pub shape: Option<Arc<dyn Fn(&str, bool, &Value) -> Option<String> + Send + Sync>>,
    pub mutating: Option<bool>,
    pub subject: Arc<dyn Fn(&Value) -> String + Send + Sync>,
}

pub trait IGitHubTool: Send + Sync {}
