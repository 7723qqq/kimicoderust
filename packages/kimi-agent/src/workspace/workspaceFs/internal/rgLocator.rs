use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;

use crate::_base::utils::abort::{AbortSignal, abort_error};
use crate::environment::environment::Environment;
use crate::environment::share_bin_rg::share_bin_rg_path;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::os_fs::OS_FS_UNAVAILABLE;

pub use crate::environment::share_bin_rg::get_share_bin_rg_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RgResolutionSource {
    SystemPath,
    ShareBinCached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgResolution {
    pub path: String,
    pub source: RgResolutionSource,
}

#[derive(Clone)]
pub struct RgProbe {
    pub exec: Arc<
        dyn Fn(Vec<String>) -> BoxFuture<'static, Result<RgProbeResult, WireAsyncError>>
            + Send
            + Sync,
    >,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgProbeResult {
    pub exit_code: i32,
}

#[derive(Default)]
pub struct EnsureRgPathOptions {
    pub signal: Option<AbortSignal>,
    pub allow_cached_fallback: bool,
    pub environment: Option<Arc<dyn Environment>>,
}

pub fn ensure_rg_path(
    probe: RgProbe,
    options: EnsureRgPathOptions,
) -> BoxFuture<'static, Result<RgResolution, WireAsyncError>> {
    async move {
        throw_if_aborted(options.signal.as_ref())?;
        let system = (probe.exec)(vec!["rg".to_string(), "--version".to_string()])
            .await
            .unwrap_or(RgProbeResult { exit_code: -1 });
        if system.exit_code == 0 {
            return Ok(RgResolution {
                path: "rg".to_string(),
                source: RgResolutionSource::SystemPath,
            });
        }
        if options.allow_cached_fallback {
            throw_if_aborted(options.signal.as_ref())?;
            let cached = share_bin_rg_path(options.environment.as_ref());
            let cached_run = (probe.exec)(vec![cached.clone(), "--version".to_string()])
                .await
                .unwrap_or(RgProbeResult { exit_code: -1 });
            if cached_run.exit_code == 0 {
                return Ok(RgResolution {
                    path: cached,
                    source: RgResolutionSource::ShareBinCached,
                });
            }
        }
        Err(Box::new(Error2::new(
            OS_FS_UNAVAILABLE,
            "ripgrep (rg) is not available on PATH",
        )) as WireAsyncError)
    }
    .boxed()
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), WireAsyncError> {
    if signal.is_some_and(|signal| signal.aborted()) {
        return Err(Box::new(abort_error()));
    }
    Ok(())
}
