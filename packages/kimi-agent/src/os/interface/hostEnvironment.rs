use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::wire::record::WireAsyncError;

pub type OsKind = String;
pub type PathClass = String;
pub type ShellName = String;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsRuntimeInfo {
    pub name: String,
    pub version: String,
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostEnvironmentInfo {
    pub os_kind: OsKind,
    pub os_arch: String,
    pub os_version: String,
    pub shell_name: ShellName,
    pub shell_path: String,
    pub path_class: PathClass,
    pub home_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub js_runtimes: Option<Vec<JsRuntimeInfo>>,
}

pub trait IHostEnvironment: Send + Sync {
    fn os_kind(&self) -> OsKind;
    fn os_arch(&self) -> String;
    fn os_version(&self) -> String;
    fn shell_name(&self) -> ShellName;
    fn shell_path(&self) -> String;
    fn path_class(&self) -> PathClass;
    fn home_dir(&self) -> String;
    fn js_runtimes(&self) -> Option<Vec<JsRuntimeInfo>>;
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;
}
