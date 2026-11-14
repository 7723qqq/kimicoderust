use crate::os::interface::host_clock::IHostClock;

pub struct HostClockService;

impl HostClockService {
    pub fn new() -> Self {
        Self
    }
}

impl Default for HostClockService {
    fn default() -> Self {
        Self::new()
    }
}

impl IHostClock for HostClockService {
    fn now(&self) -> chrono::DateTime<chrono::Local> {
        chrono::Local::now()
    }

    fn time_zone(&self) -> String {
        if let Ok(zone) = std::env::var("TZ")
            && !zone.trim().is_empty()
        {
            return zone.trim().to_string();
        }
        let zone = chrono::Local::now().format("%Z").to_string();
        if zone.is_empty() {
            "UTC".to_string()
        } else {
            zone
        }
    }
}
