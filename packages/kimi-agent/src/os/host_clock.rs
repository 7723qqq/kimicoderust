pub trait HostClock: Send + Sync {
    fn now(&self) -> chrono::DateTime<chrono::Local>;

    fn time_zone(&self) -> String;
}
