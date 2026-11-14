pub trait ISessionRunGate: Send + Sync {
    fn begin(&self) -> Result<(), crate::wire::record::WireAsyncError>;
}
