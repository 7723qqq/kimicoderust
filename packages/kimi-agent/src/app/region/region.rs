use futures_util::future::BoxFuture;

pub use crate::region::KimiRegion;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiRegionSource {
    Env,
    Login,
    Marker,
    LocalSignals,
    Fallback,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KimiRegionResolution {
    pub region: KimiRegion,
    pub source: KimiRegionSource,
}

pub trait IRegionService: Send + Sync {
    fn current(&self) -> KimiRegionResolution;
    fn get_region_async(&self) -> BoxFuture<'_, KimiRegion>;
}
