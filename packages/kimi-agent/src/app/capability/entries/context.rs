use std::sync::Arc;

use futures_util::future::BoxFuture;

use crate::_base::utils::fetch_like::FetchLike;
use crate::app::plugin::plugin::IPluginService;
use crate::os::interface::host_process::IHostProcessService;
use crate::region::KimiRegion;

#[derive(Clone)]
pub struct CapabilityEntryContext {
    pub platform: String,
    pub arch: String,
    pub kimi_home_dir: String,
    pub user_home_dir: String,
    pub plugins: Arc<dyn IPluginService>,
    pub host_process: Arc<dyn IHostProcessService>,
    pub fetch_impl: Option<Arc<dyn FetchLike>>,
    pub applications_dir: Option<String>,
    pub webbridge_base_url: Option<String>,
    pub detect_probe_timeout_ms: Option<u64>,
    pub command_timeout_ms: Option<u64>,
    pub resolve_region: Option<Arc<dyn Fn() -> BoxFuture<'static, KimiRegion> + Send + Sync>>,
}
