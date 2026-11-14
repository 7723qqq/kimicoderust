use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const SESSION_IDLE_RECLAIM_ENV: &str = "KIMI_CODE_SESSION_IDLE_RECLAIM_MS";

pub const DEFAULT_SESSION_IDLE_RECLAIM_MS: i64 = 30 * 60 * 1000;

pub const SESSION_IDLE_SWEEP_INTERVAL_MS: u64 = 60_000;

pub fn resolve_session_idle_reclaim_ms(raw: Option<&str>) -> Result<i64, String> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_SESSION_IDLE_RECLAIM_MS);
    };
    if raw.trim().is_empty() {
        return Ok(DEFAULT_SESSION_IDLE_RECLAIM_MS);
    }
    let parsed: i64 = raw.trim().parse().map_err(|_| {
        format!("{SESSION_IDLE_RECLAIM_ENV} must be a non-negative integer, got {raw:?}.")
    })?;
    if parsed < 0 {
        return Err(format!(
            "{SESSION_IDLE_RECLAIM_ENV} must be a non-negative integer, got {raw:?}."
        ));
    }
    Ok(parsed)
}

pub fn resolve_session_idle_reclaim_ms_from_env() -> Result<i64, String> {
    let raw = std::env::var(SESSION_IDLE_RECLAIM_ENV).ok();
    resolve_session_idle_reclaim_ms(raw.as_deref())
}

pub trait IdleSessionControl: Send + Sync {
    fn list_sessions_by_activity(&self) -> Vec<(String, i64)>;

    fn close_if_idle(&self, session_id: &str, idle_timeout_ms: i64) -> bool;
}

pub struct SessionIdleReaper<C: IdleSessionControl> {
    control: Arc<C>,
    idle_timeout_ms: i64,
    sweep_interval: Duration,
    closed: AtomicBool,
    in_flight: AtomicBool,
}

impl<C: IdleSessionControl + 'static> SessionIdleReaper<C> {
    pub fn new(control: Arc<C>, idle_timeout_ms: i64) -> Self {
        Self {
            control,
            idle_timeout_ms,
            sweep_interval: Duration::from_millis(SESSION_IDLE_SWEEP_INTERVAL_MS),
            closed: AtomicBool::new(false),
            in_flight: AtomicBool::new(false),
        }
    }

    pub fn with_sweep_interval(mut self, interval: Duration) -> Self {
        self.sweep_interval = interval;
        self
    }

    pub fn enabled(&self) -> bool {
        self.idle_timeout_ms > 0 && !self.closed.load(Ordering::SeqCst)
    }

    pub fn sweep(&self) -> usize {
        if !self.enabled() {
            return 0;
        }
        if self
            .in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return 0;
        }
        let mut closed = 0;
        for (session_id, last_active_at) in self.control.list_sessions_by_activity() {
            if self.closed.load(Ordering::SeqCst) {
                break;
            }

            if now_millis() - last_active_at < self.idle_timeout_ms {
                continue;
            }
            if self
                .control
                .close_if_idle(&session_id, self.idle_timeout_ms)
            {
                closed += 1;
                tracing::info!(session_id, "session reclaimed while idle");
            }
        }
        self.in_flight.store(false, Ordering::SeqCst);
        closed
    }

    pub async fn run(self: Arc<Self>) {
        if !self.enabled() {
            return;
        }
        tracing::info!(
            idle_timeout_ms = self.idle_timeout_ms,
            sweep_interval_ms = self.sweep_interval.as_millis() as u64,
            "session idle reclaim enabled"
        );
        while !self.closed.load(Ordering::SeqCst) {
            tokio::time::sleep(self.sweep_interval).await;
            let reaper = Arc::clone(&self);
            let _ = tokio::task::spawn_blocking(move || reaper.sweep()).await;
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        while self.in_flight.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeControl {
        sessions: Mutex<Vec<(String, i64)>>,
        retained: Mutex<Vec<String>>,
        closed: Mutex<Vec<String>>,
    }

    impl IdleSessionControl for FakeControl {
        fn list_sessions_by_activity(&self) -> Vec<(String, i64)> {
            self.sessions.lock().unwrap().clone()
        }

        fn close_if_idle(&self, session_id: &str, _idle_timeout_ms: i64) -> bool {
            if self
                .retained
                .lock()
                .unwrap()
                .iter()
                .any(|id| id == session_id)
            {
                return false;
            }
            self.closed.lock().unwrap().push(session_id.to_string());
            true
        }
    }

    #[test]
    fn the_environment_parsing_matches_v2() {
        assert_eq!(
            resolve_session_idle_reclaim_ms(None).unwrap(),
            DEFAULT_SESSION_IDLE_RECLAIM_MS
        );
        assert_eq!(
            resolve_session_idle_reclaim_ms(Some("   ")).unwrap(),
            DEFAULT_SESSION_IDLE_RECLAIM_MS
        );
        assert_eq!(resolve_session_idle_reclaim_ms(Some("5000")).unwrap(), 5000);
        assert_eq!(resolve_session_idle_reclaim_ms(Some("0")).unwrap(), 0);
        assert!(
            resolve_session_idle_reclaim_ms(Some("nope"))
                .unwrap_err()
                .contains("non-negative integer")
        );
        assert!(resolve_session_idle_reclaim_ms(Some("-1")).is_err());
    }

    #[test]
    fn only_sessions_past_the_timeout_are_closed() {
        let control = Arc::new(FakeControl::default());
        let now = now_millis();
        *control.sessions.lock().unwrap() = vec![
            ("idle".to_string(), now - 10_000),
            ("active".to_string(), now),
        ];
        let reaper = SessionIdleReaper::new(Arc::clone(&control), 5_000);
        assert_eq!(reaper.sweep(), 1);
        assert_eq!(*control.closed.lock().unwrap(), vec!["idle".to_string()]);
    }

    #[test]
    fn a_retained_session_is_not_closed() {
        let control = Arc::new(FakeControl::default());
        let now = now_millis();
        *control.sessions.lock().unwrap() = vec![("busy".to_string(), now - 10_000)];
        *control.retained.lock().unwrap() = vec!["busy".to_string()];
        let reaper = SessionIdleReaper::new(Arc::clone(&control), 5_000);
        assert_eq!(reaper.sweep(), 0);
        assert!(control.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_zero_timeout_disables_reclaim() {
        let control = Arc::new(FakeControl::default());
        *control.sessions.lock().unwrap() = vec![("idle".to_string(), 0)];
        let reaper = SessionIdleReaper::new(Arc::clone(&control), 0);
        assert!(!reaper.enabled());
        assert_eq!(reaper.sweep(), 0);
        assert!(control.closed.lock().unwrap().is_empty());
    }
}
