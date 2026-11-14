use std::sync::Arc;

use parking_lot::Mutex;

use crate::_base::log::log::ILogService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::session::cache_observation::CacheBreakTracker;

use super::cache_break::{CacheBreakObservation, CacheBreakObserveInput, IAgentCacheBreakService};

pub struct AgentCacheBreakService {
    tracker: Mutex<CacheBreakTracker>,
    telemetry: Arc<dyn ITelemetryService>,
    logger: Arc<dyn ILogService>,
}

impl AgentCacheBreakService {
    pub fn new(
        telemetry: Arc<dyn ITelemetryService>,
        logger: Arc<dyn ILogService>,
    ) -> Self {
        Self {
            tracker: Mutex::new(CacheBreakTracker::new()),
            telemetry,
            logger,
        }
    }
}

impl IAgentCacheBreakService for AgentCacheBreakService {
    fn observe(&self, input: CacheBreakObserveInput) -> Option<CacheBreakObservation> {
        let now = input.now.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let observation = self.tracker.lock().observe(
            &input.usage,
            input.cache.as_ref(),
            now,
        )?;
        let previous_read = observation.previous.input_cache_read;
        let current_read = observation.current.input_cache_read;
        self.logger.warn(
            &format!(
                "[cache] prompt-cache break detected ({}): cache read dropped {} tokens ({}%) between steps, {:.1}s after the previous request (model {} -> {}, effort {} -> {})",
                observation.cause.as_str(),
                previous_read - current_read,
                (observation.cache_read_drop_ratio * 100.0).round(),
                observation.interval_ms as f64 / 1000.0,
                observation.previous.model,
                observation.current.model,
                observation.previous.effort,
                observation.current.effort
            ),
            None,
        );
        let payload = observation.telemetry_payload(input.turn_id.as_deref(), input.step);
        let properties = payload.as_object().cloned().map(|mut properties| {
            properties.remove("type");
            let null_keys = properties
                .keys()
                .filter(|key| properties.get(*key).is_some_and(serde_json::Value::is_null))
                .cloned()
                .collect::<Vec<_>>();
            for key in null_keys {
                properties.remove(&key);
            }
            properties
        });
        self.telemetry.track2("cache_break_detected", properties);
        Some(observation)
    }

    fn reset(&self) {
        self.tracker.lock().reset();
    }
}
