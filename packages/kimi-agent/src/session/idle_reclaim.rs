//! Session idle reclaim (v2 `kap-server/src/services/sessionIdleReaper.ts` +
//! `app/sessionManager/sessionManagerService.ts:286-334`).
//!
//! A session that has been idle for longer than the configured timeout is
//! released: its in-memory scope is closed, and a later request resumes it from
//! persistence. The reaper never closes a session that is still doing work —
//! a running turn, a pending interaction, or an agent with cron work keeps the
//! session retained.
//!
//! The control seam is a trait so the policy (what "idle" and "retained" mean)
//! lives with the thing that owns the scopes, while the scheduling, the
//! single-flight guard and the environment parsing live here.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// v2 `SESSION_IDLE_RECLAIM_ENV`.
pub const SESSION_IDLE_RECLAIM_ENV: &str = "KIMI_CODE_SESSION_IDLE_RECLAIM_MS";

/// v2 `DEFAULT_SESSION_IDLE_RECLAIM_MS`: 30 minutes.
pub const DEFAULT_SESSION_IDLE_RECLAIM_MS: i64 = 30 * 60 * 1000;

/// v2 `SWEEP_INTERVAL_MS`.
pub const SESSION_IDLE_SWEEP_INTERVAL_MS: u64 = 60_000;

/// v2 `resolveSessionIdleReclaimMs`: absent or blank means the default, `0`
/// disables reclaim, and anything that is not a non-negative integer is a
/// configuration error (not a silent fallback).
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

/// [`resolve_session_idle_reclaim_ms`] over the process environment.
pub fn resolve_session_idle_reclaim_ms_from_env() -> Result<i64, String> {
    let raw = std::env::var(SESSION_IDLE_RECLAIM_ENV).ok();
    resolve_session_idle_reclaim_ms(raw.as_deref())
}

/// The session-scope operations the reaper needs. Implemented by whatever owns
/// live session scopes; the reaper itself is storage-agnostic.
pub trait IdleSessionControl: Send + Sync {
    /// Every live session and when it was last active (epoch millis).
    fn list_sessions_by_activity(&self) -> Vec<(String, i64)>;

    /// Close the session if it is still idle and not retained.
    ///
    /// Implementations must re-check activity under the same serialization
    /// that guards `close` (v2 `retained` + `closeIfIdle` run inside
    /// `serializeLifecycle`), and must give up when the session lock is held by
    /// another process. Returns whether the session was closed.
    fn close_if_idle(&self, session_id: &str, idle_timeout_ms: i64) -> bool;
}

/// v2 `SessionIdleReaper`: a periodic sweep with a single in-flight run.
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

    /// Override the sweep cadence (tests use milliseconds, not a minute).
    pub fn with_sweep_interval(mut self, interval: Duration) -> Self {
        self.sweep_interval = interval;
        self
    }

    /// Whether the reaper is enabled: a timeout of `0` disables reclaim
    /// (v2 `start` returns early for a non-positive timeout).
    pub fn enabled(&self) -> bool {
        self.idle_timeout_ms > 0 && !self.closed.load(Ordering::SeqCst)
    }

    /// One sweep. Returns the number of sessions closed.
    ///
    /// Overlapping sweeps collapse into the first one (v2 `inFlight`), so a
    /// slow close cannot make the next tick queue a second close of the same
    /// session.
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
            // Cheap pre-check; `close_if_idle` re-checks under its lifecycle
            // serialization because activity can resume between the two.
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

    /// Run sweeps until [`Self::close`]. Returns immediately when disabled.
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

    /// Stop the loop. A sweep already running finishes first (v2 `close`
    /// awaits `inFlight`).
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
